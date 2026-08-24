//! Incidents end to end: damping, the audit trail, and what `report` admits it
//! did not see.
//!
//! The log here is a real file, because the properties being checked are about
//! what survives to disk and reads back.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use stillwatch::config::Config;
use stillwatch::evaluate::evaluate;
use stillwatch::incidents::{self, Event};
use stillwatch::notify::{Dispatcher, Notification, Notifier, NotifyError};
use stillwatch::state::{SharedState, State};

const START: u64 = 1_755_000_000;
const TICK: u64 = 5;

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

const CONFIG: &str = r#"
listen = "127.0.0.1:9111"
confirm_after = "30s"

[[job]]
name = "product-scraper"
  [job.alive]
  expect_every   = "60s"
  warn_after     = "5m"
  critical_after = "15m"
"#;

#[derive(Default)]
struct Silent;

#[async_trait]
impl Notifier for Silent {
    async fn send(&self, _: &Notification) -> Result<(), NotifyError> {
        Ok(())
    }

    fn channel(&self) -> &'static str {
        "silent"
    }
}

#[derive(Default)]
struct Counting(Mutex<usize>);

#[async_trait]
impl Notifier for Counting {
    async fn send(&self, _: &Notification) -> Result<(), NotifyError> {
        *self.0.lock().expect("lock") += 1;
        Ok(())
    }

    fn channel(&self) -> &'static str {
        "counting"
    }
}

fn config() -> Config {
    Config::from_toml(CONFIG, &HashMap::new()).expect("config should load")
}

/// Runs the daemon's evaluate-and-dispatch cycle across a span.
async fn run_until(state: &SharedState, dispatcher: &mut Dispatcher, from: u64, to: u64) {
    let mut now = from;
    while now <= to {
        let assessments = state.read(|s| evaluate(s, at(now)));
        dispatcher.dispatch(&assessments, at(now)).await;
        now += TICK;
    }
}

/// The whole loop: an incident opens, escalates, resolves, and every step of it
/// survives to disk in a form `report` can read back.
#[tokio::test]
async fn an_incident_is_written_to_disk_and_read_back_by_report() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("incidents.jsonl");
    let config = config();

    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    state.record_beat("product-scraper", at(START));

    let log = incidents::Log::open(&path, 1_000_000).expect("open");
    let mut dispatcher =
        Dispatcher::with_confirmation(Arc::new(Silent), config.confirm_after).recording_to(log);

    // Warn at 5m, critical at 15m, then the job comes back.
    run_until(&state, &mut dispatcher, START, START + 1_200).await;
    state.record_beat("product-scraper", at(START + 1_205));
    run_until(&state, &mut dispatcher, START + 1_205, START + 1_400).await;

    drop(dispatcher);

    let events = incidents::read_all(&path).expect("read");
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| match event {
            Event::Started { .. } => "started",
            Event::Stopped { .. } => "stopped",
            Event::Watching { .. } => "watching",
            Event::Opened { .. } => "opened",
            Event::Escalated { .. } => "escalated",
            Event::Resolved { .. } => "resolved",
        })
        .collect();

    assert_eq!(
        kinds,
        ["opened", "escalated", "resolved"],
        "the whole arc of one incident: {events:#?}"
    );

    // An incident that escalated must not be filed forever as a warning.
    match &events[1] {
        Event::Escalated { severity, .. } => assert_eq!(severity, "critical"),
        other => panic!("expected an escalation, got {other:?}"),
    }

    // And the resolution pairs with the open by its start time.
    let (Event::Opened { ts: opened, .. }, Event::Resolved { opened_ts, .. }) =
        (&events[0], &events[2])
    else {
        panic!("expected an open and a resolution")
    };
    assert_eq!(opened, opened_ts, "the pair must line up");
}

/// A condition that clears inside the confirmation window never reaches disk,
/// because it never became an incident.
#[tokio::test]
async fn a_flap_leaves_no_trace_in_the_audit_trail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("incidents.jsonl");
    let config = config();

    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    state.record_beat("product-scraper", at(START));

    let log = incidents::Log::open(&path, 1_000_000).expect("open");
    let notifier = Arc::new(Counting::default());
    let mut dispatcher =
        Dispatcher::with_confirmation(notifier.clone(), config.confirm_after).recording_to(log);

    // Crosses the threshold, then a beat arrives before the window elapses.
    run_until(&state, &mut dispatcher, START, START + 310).await;
    state.record_beat("product-scraper", at(START + 315));
    run_until(&state, &mut dispatcher, START + 315, START + 600).await;

    drop(dispatcher);

    assert_eq!(*notifier.0.lock().expect("lock"), 0, "nothing was sent");
    assert!(
        incidents::read_all(&path).expect("read").is_empty(),
        "and nothing was recorded, because nothing happened"
    );
}

/// The self-awareness requirement, through real files.
#[tokio::test]
async fn report_admits_the_time_it_was_not_watching() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("incidents.jsonl");

    // Two runs with a gap between them, the first ended cleanly.
    let mut log = incidents::Log::open(&path, 1_000_000).expect("open");
    log.append(&incidents::started(at(START)));
    log.append(&incidents::stopped(at(START + 3_600)));
    log.append(&incidents::started(at(START + 7_200)));
    log.append(&incidents::watching(at(START + 10_800)));
    drop(log);

    let events = incidents::read_all(&path).expect("read");
    let report = incidents::report(&events, at(START), at(START + 10_800));

    assert_eq!(report.watched, Duration::from_secs(7_200));
    assert_eq!(
        report.unknown,
        Duration::from_secs(3_600),
        "the hour between the runs belongs to nobody"
    );

    let rendered = incidents::render_report(&report);
    assert!(rendered.contains("1h unaccounted for"), "{rendered}");
    assert!(rendered.contains("of the watched time only"), "{rendered}");
}

/// Neither 0% nor 100% for a window nothing was watching.
#[tokio::test]
async fn report_over_an_empty_window_is_not_a_percentage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("incidents.jsonl");

    let mut log = incidents::Log::open(&path, 1_000_000).expect("open");
    log.append(&incidents::started(at(START)));
    log.append(&incidents::stopped(at(START + 60)));
    drop(log);

    let events = incidents::read_all(&path).expect("read");
    // A window long after everything in the log.
    let report = incidents::report(&events, at(START + 100_000), at(START + 186_400));

    let rendered = incidents::render_report(&report);
    assert!(rendered.contains("no record of watching"), "{rendered}");
    assert!(rendered.contains("unknown rather than good"), "{rendered}");
    assert!(
        !rendered.contains('%'),
        "no percentage is honest here: {rendered}"
    );
}

/// A watchdog built to run for months must not fill a disk.
#[tokio::test]
async fn the_audit_trail_is_bounded_on_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("incidents.jsonl");

    let cap = 8_000;
    let mut log = incidents::Log::open(&path, cap).expect("open");
    for tick in 0..5_000 {
        log.append(&incidents::watching(at(START + tick)));
    }
    drop(log);

    let total: u64 = std::fs::read_dir(dir.path())
        .expect("readdir")
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum();

    assert!(
        total <= cap * 2 + 200,
        "five thousand records must stay bounded at twice the cap, got {total}"
    );
}

/// If the audit trail cannot be opened, that is a startup failure and not
/// something to shrug at.
#[tokio::test]
async fn an_unwritable_audit_trail_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A directory can never be opened as a file for appending.
    assert!(incidents::Log::open(dir.path(), 1_000).is_err());
}

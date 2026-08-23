//! Phase 1 end to end: a config, a stream of beats, and the alerts a person
//! actually receives.
//!
//! Time is driven directly. Nothing here sleeps, and the whole scenario — a
//! twenty-five minute outage and its recovery — runs in microseconds.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use stillwatch::config::Config;
use stillwatch::evaluate::evaluate;
use stillwatch::notify::{Dispatcher, Level, Notification, Notifier, NotifyError};
use stillwatch::state::{SharedState, State};

/// Two unrelated workload shapes, so domain assumptions cannot creep in: one
/// job that runs continuously, one that is quiet by design.
const CONFIG: &str = r#"
listen = "127.0.0.1:9111"

[notify.telegram]
token   = "${TELEGRAM_TOKEN}"
chat_id = "${TELEGRAM_CHAT}"

# a scraper that should be beating all the time
[[job]]
name = "product-scraper"
  [job.alive]
  expect_every   = "60s"
  warn_after     = "5m"
  critical_after = "15m"

# a nightly ETL that legitimately does nothing for most of the day
[[job]]
name = "nightly-sync"
"#;

/// The tick the daemon actually runs at.
const TICK: u64 = 5;

const START: u64 = 1_755_000_000;

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Notification>>);

impl Recorder {
    fn received(&self) -> Vec<Notification> {
        self.0.lock().expect("lock").clone()
    }

    fn summary(&self) -> Vec<(String, Level)> {
        self.received()
            .into_iter()
            .map(|n| (n.subject, n.level))
            .collect()
    }
}

#[async_trait]
impl Notifier for Recorder {
    async fn send(&self, notification: &Notification) -> Result<(), NotifyError> {
        self.0.lock().expect("lock").push(notification.clone());
        Ok(())
    }

    fn channel(&self) -> &'static str {
        "recorder"
    }
}

fn load() -> Config {
    let env: HashMap<String, String> = [("TELEGRAM_TOKEN", "t"), ("TELEGRAM_CHAT", "-1")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    Config::from_toml(CONFIG, &env).expect("config should load")
}

/// Runs the daemon's evaluate-and-dispatch cycle over a span of time.
async fn run_until(state: &SharedState, dispatcher: &mut Dispatcher, from: u64, to: u64) {
    let mut now = from;
    while now <= to {
        let assessments = state.read(|state| evaluate(state, at(now)));
        dispatcher.dispatch(&assessments, at(now)).await;
        now += TICK;
    }
}

#[tokio::test]
async fn a_scraper_dies_is_reported_and_recovers_while_a_nightly_job_stays_quiet() {
    let config = load();
    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    let recorder = Arc::new(Recorder::default());
    let mut dispatcher = Dispatcher::new(recorder.clone());

    // Ten minutes of both jobs behaving: the scraper beats every 60s, the
    // nightly job says nothing at all because it has nothing to do.
    for minute in 0..10 {
        let now = START + minute * 60;
        state.record_beat("product-scraper", at(now));
        run_until(&state, &mut dispatcher, now, now + 55).await;
    }

    assert!(
        recorder.received().is_empty(),
        "a healthy scraper and a silent nightly job are not news: {:?}",
        recorder.summary()
    );

    // The scraper's last beat was at START + 540. It now stops dead.
    let last_beat = START + 540;

    // Four minutes of silence: still inside the warn threshold.
    run_until(&state, &mut dispatcher, last_beat, last_beat + 240).await;
    assert!(
        recorder.received().is_empty(),
        "4m of silence is not yet late"
    );

    // Five minutes: a warning.
    run_until(&state, &mut dispatcher, last_beat + 245, last_beat + 300).await;
    assert_eq!(
        recorder.summary(),
        [("product-scraper".to_string(), Level::Warn)]
    );

    // Ten more minutes of the same silence produce nothing new until the
    // critical threshold, and then exactly one more message.
    run_until(&state, &mut dispatcher, last_beat + 305, last_beat + 900).await;
    assert_eq!(
        recorder.summary(),
        [
            ("product-scraper".to_string(), Level::Warn),
            ("product-scraper".to_string(), Level::Critical),
        ],
        "one alert per incident per severity, not one per cycle"
    );

    // Another ten minutes of silence: it does not nag.
    run_until(&state, &mut dispatcher, last_beat + 905, last_beat + 1_500).await;
    assert_eq!(recorder.received().len(), 2, "warn, critical, then nothing");

    // The scraper comes back.
    let recovered_at = last_beat + 1_505;
    state.record_beat("product-scraper", at(recovered_at));
    run_until(&state, &mut dispatcher, recovered_at, recovered_at).await;

    let summary = recorder.summary();
    assert_eq!(summary.len(), 3);
    assert_eq!(
        summary[2],
        ("product-scraper".to_string(), Level::Recovered)
    );

    // The incident opened at the first warn, five minutes after the last beat,
    // and ran until the scraper came back 1205s later. The all-clear reports the
    // whole incident, not just the time since it escalated to critical.
    assert_eq!(
        recorder.received()[2].text,
        "✅  product-scraper recovered — no heartbeat for 20m5s"
    );

    // Through all of it, the nightly job never produced a single alert.
    assert!(
        recorder
            .received()
            .iter()
            .all(|n| n.subject == "product-scraper"),
        "the quiet-by-design job must never be alerted on: {:?}",
        recorder.summary()
    );
}

/// A job that was already dead before stillwatch started has no beat history at
/// all. It is measured from process start, and the alert says so rather than
/// naming a beat that was never seen.
#[tokio::test]
async fn a_job_dead_before_the_watchdog_started_is_reported_from_process_start() {
    let config = load();
    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    let recorder = Arc::new(Recorder::default());
    let mut dispatcher = Dispatcher::new(recorder.clone());

    run_until(&state, &mut dispatcher, START, START + 900).await;

    assert_eq!(
        recorder.summary(),
        [
            ("product-scraper".to_string(), Level::Warn),
            ("product-scraper".to_string(), Level::Critical),
        ]
    );

    let first = &recorder.received()[0].text;
    assert!(
        first.contains("no heartbeat since stillwatch started"),
        "{first}"
    );
    assert!(
        !first.contains("last beat"),
        "must not invent a beat it never saw: {first}"
    );
}

/// A restart is not an outage: with no history and less than one threshold
/// elapsed, stillwatch says nothing.
#[tokio::test]
async fn a_fresh_start_is_quiet_until_a_threshold_is_actually_crossed() {
    let config = load();
    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    let recorder = Arc::new(Recorder::default());
    let mut dispatcher = Dispatcher::new(recorder.clone());

    run_until(&state, &mut dispatcher, START, START + 295).await;

    assert!(
        recorder.received().is_empty(),
        "absent history is not an outage: {:?}",
        recorder.summary()
    );
}

/// The whole point of the tool: a job with no declared cadence is never late,
/// however long it stays silent.
#[tokio::test]
async fn a_quiet_by_design_job_is_never_paged_on_however_long_it_waits() {
    let config = load();
    let jobs: Vec<_> = config
        .jobs
        .into_iter()
        .filter(|job| job.name == "nightly-sync")
        .collect();
    assert_eq!(jobs.len(), 1);

    let state = SharedState::new(State::new(at(START), &jobs, &[]));
    let recorder = Arc::new(Recorder::default());
    let mut dispatcher = Dispatcher::new(recorder.clone());

    // Three days, sampled rather than ticked, because the assertion is about
    // the rule and not the cadence.
    for hour in [1, 12, 26, 50, 72] {
        let now = at(START + hour * 3_600);
        let assessments = state.read(|state| evaluate(state, now));
        dispatcher.dispatch(&assessments, now).await;
    }

    assert!(
        recorder.received().is_empty(),
        "a job with no [job.alive] block must never produce a liveness alert"
    );
}

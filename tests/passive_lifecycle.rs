//! Passive mode end to end: watching a job from outside, through real files.
//!
//! The filesystem is real here — a `tempdir`, actual rotations, actual writes —
//! because the failures this phase is about live in the filesystem's behaviour
//! and a mock would be a mock of my own assumptions. Time is still injected.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use stillwatch::config::Config;
use stillwatch::evaluate::{evaluate, Condition};
use stillwatch::notify::{render, Dispatcher, Level, Notification, Notifier, NotifyError};
use stillwatch::passive::Watcher;
use stillwatch::state::{SharedState, State};

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

/// A config pointing at real paths inside a temporary directory.
fn config_for(dir: &Path) -> Config {
    let dir = dir.display().to_string().replace('\\', "/");
    let text = format!(
        r#"
listen = "127.0.0.1:9111"

[[job]]
name = "clients-etl"
mode = "passive"

  [job.process]
  pidfile      = "{dir}/etl.pid"
  absent_after = "60s"

  [job.log]
  path        = "{dir}/etl.log"
  stale_after = "10m"
  error_regex = "(?i)(traceback|fatal|failed to write)"

  [job.artifact]
  path        = "{dir}/daily.csv"
  stale_after = "26h"
  min_bytes   = 1024
"#
    );

    Config::from_toml(&text, &HashMap::new()).expect("config should load")
}

fn watcher(config: &Config) -> Watcher {
    Watcher::new(config.jobs[0].clone())
}

fn write(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write");
}

fn append(path: &Path, contents: &str) {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open for append");
    file.write_all(contents.as_bytes()).expect("append");
}

fn conditions(state: &SharedState, now: u64) -> Vec<Condition> {
    state
        .read(|s| evaluate(s, at(now)))
        .iter()
        .map(|a| a.reason.condition())
        .collect()
}

fn texts(state: &SharedState, now: u64) -> Vec<String> {
    state
        .read(|s| evaluate(s, at(now)))
        .iter()
        .map(|a| render(a, at(now)).text)
        .collect()
}

/// The whole passive picture: a job that is up, logging and producing output.
#[tokio::test]
async fn a_healthy_passive_job_produces_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_for(dir.path());

    write(&dir.path().join("etl.pid"), &std::process::id().to_string());
    write(&dir.path().join("etl.log"), "starting up\nwrote 400 rows\n");
    write(&dir.path().join("daily.csv"), &"x".repeat(40_000));

    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    watcher(&config).poll(&state, SystemTime::now());

    let recorder = Arc::new(Recorder::default());
    let mut dispatcher = Dispatcher::new(recorder.clone());
    let assessments = state.read(|s| evaluate(s, SystemTime::now()));
    dispatcher.dispatch(&assessments, SystemTime::now()).await;

    assert!(
        recorder.received().is_empty(),
        "a job that is up, logging and producing is not news: {:?}",
        texts(&state, START)
    );
}

/// The trap: a rotation must not leave a healthy job looking permanently stale,
/// and the tail must follow into the new file.
#[tokio::test]
async fn a_real_rotation_is_followed_and_does_not_look_like_staleness() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_for(dir.path());
    let log = dir.path().join("etl.log");

    write(&log, &"steady output\n".repeat(500));
    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));

    // One watcher across both looks: the tail position it carries is exactly
    // what a rotation invalidates.
    let mut watching = watcher(&config);
    watching.poll(&state, SystemTime::now());

    // logrotate's usual move, and the replacement immediately says something bad.
    std::fs::rename(&log, dir.path().join("etl.log.1")).expect("rename");
    write(&log, "Traceback (most recent call last):\n");

    watching.poll(&state, SystemTime::now());

    let rotations = state.read(|s| s.job("clients-etl").expect("job").passive.log_rotations);
    assert_eq!(rotations, 1, "the replacement must be noticed");

    // Read from the top of the new file, not from the old offset.
    let error = state.read(|s| {
        s.job("clients-etl")
            .expect("job")
            .passive
            .log_last_error
            .clone()
    });
    assert!(
        error.is_some_and(|e| e.line.contains("Traceback")),
        "the tail must follow into the new file"
    );

    // And the live file is fresh, so nothing reports staleness.
    let found = conditions(&state, START);
    assert!(
        !found.contains(&Condition::FileStale(
            stillwatch::evaluate::WatchedFile::Log
        )),
        "a rotated log that is being written to is not stale: {found:?}"
    );
}

/// A log that has never existed and one that stopped moving are different
/// facts, and send you to different places.
#[tokio::test]
async fn a_missing_log_and_a_stale_one_read_differently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_for(dir.path());

    // Nothing at the path at all.
    let never = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    watcher(&config).poll(&never, at(START + 5));

    let missing = texts(&never, START + 700)
        .into_iter()
        .find(|t| t.contains("log has never appeared"))
        .expect("a never-appeared finding");
    assert!(missing.contains("path in the config is wrong"), "{missing}");

    // Now one that exists and has gone quiet.
    let log = dir.path().join("etl.log");
    write(&log, "wrote 400 rows\n");
    let stale = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    watcher(&config).poll(&stale, at(START + 5));

    let far_future = 2_000_000_000;
    let text = texts(&stale, far_future)
        .into_iter()
        .find(|t| t.contains("has not changed in"))
        .expect("a staleness finding");
    assert!(text.contains("wedged, idle, or logging"), "{text}");
    assert!(!text.contains("never appeared"), "{text}");
}

/// The classic: the run completed, exited zero, and wrote nothing.
#[tokio::test]
async fn a_fresh_but_empty_export_is_reported_as_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_for(dir.path());

    write(&dir.path().join("etl.log"), "done\n");
    write(&dir.path().join("daily.csv"), "id,name\n");

    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    watcher(&config).poll(&state, SystemTime::now());

    let text = texts(&state, START)
        .into_iter()
        .find(|t| t.contains("nearly empty"))
        .expect("an empty-output finding");

    assert!(
        text.contains("not a stale artifact, it is an empty one"),
        "{text}"
    );
    assert!(text.contains("exits zero"), "{text}");
}

/// Runs a process to completion and returns its now-free pid.
///
/// Sentinel values do not work here: 0 and anything past `pid_t` are rejected as
/// corrupt pidfiles, and picking a "probably unused" high number is a guess. A
/// process that genuinely ran and exited is the real thing. The pid could in
/// principle be handed to something else before the assertion runs, which is the
/// same recycling limit the README documents.
fn a_reaped_pid() -> u32 {
    let mut child = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "exit"])
            .spawn()
    } else {
        std::process::Command::new("true").spawn()
    }
    .expect("spawn a short-lived process");

    let pid = child.id();
    child.wait().expect("wait for it to exit");
    // Windows keeps the process object alive while a handle is open, and `Child`
    // holds one until it is dropped.
    drop(child);
    pid
}

/// A pidfile naming a process that does not exist.
#[tokio::test]
async fn a_dead_pid_is_reported_and_names_the_pid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_for(dir.path());

    let pid = a_reaped_pid();
    write(&dir.path().join("etl.pid"), &format!("{pid}\n"));

    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    watcher(&config).poll(&state, at(START + 5));

    let text = texts(&state, START + 200)
        .into_iter()
        .find(|t| t.contains(&format!("process {pid} is gone")))
        .expect("a dead-process finding");

    assert!(text.contains("did not exit on purpose"), "{text}");
    assert!(text.contains("weaker signal than a heartbeat"), "{text}");
}

/// The job's own words outrank every inference about it.
#[tokio::test]
async fn an_error_the_job_logged_is_repeated_verbatim() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_for(dir.path());
    let log = dir.path().join("etl.log");

    write(&log, "wrote 400 rows\n");
    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));

    // The same watcher across both looks, so only lines arriving *after* the
    // first one count as new.
    let mut watching = watcher(&config);
    watching.poll(&state, SystemTime::now());

    append(&log, "failed to write batch 7: disk full\n");
    watching.poll(&state, SystemTime::now());

    let text = texts(&state, START)
        .into_iter()
        .find(|t| t.contains("reporting an error"))
        .expect("a logged-error finding");

    assert!(
        text.contains("failed to write batch 7: disk full"),
        "{text}"
    );
    assert!(text.contains("the job said this about itself"), "{text}");
    assert!(text.contains("only repeating it"), "{text}");
}

/// Every passive alert admits what it is, so nobody who has only ever seen
/// these comes away trusting a pidfile as much as a heartbeat.
#[tokio::test]
async fn every_passive_alert_admits_the_signal_is_indirect() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_for(dir.path());

    write(&dir.path().join("etl.pid"), "0\n");
    write(&dir.path().join("daily.csv"), "id\n");

    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    watcher(&config).poll(&state, at(START + 5));

    let texts = texts(&state, START + 700);
    assert!(texts.len() >= 3, "expected several findings: {texts:?}");

    for text in &texts {
        assert!(
            text.contains("weaker signal than a heartbeat")
                || text.contains("the job said this about itself"),
            "a passive alert must not read as strongly as a heartbeat: {text}"
        );
    }
}

/// Each passive signal is its own incident, and each recovers on its own.
#[tokio::test]
async fn passive_findings_alert_and_recover_independently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_for(dir.path());
    let csv = dir.path().join("daily.csv");

    write(&dir.path().join("etl.pid"), &std::process::id().to_string());
    write(&dir.path().join("etl.log"), "wrote 400 rows\n");
    write(&csv, "id\n");

    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    let recorder = Arc::new(Recorder::default());
    let mut dispatcher = Dispatcher::new(recorder.clone());

    let mut watching = watcher(&config);
    watching.poll(&state, SystemTime::now());
    let now = SystemTime::now();
    let assessments = state.read(|s| evaluate(s, now));
    dispatcher.dispatch(&assessments, now).await;

    assert_eq!(
        recorder.received().len(),
        1,
        "only the empty export is wrong: {:?}",
        recorder.received()
    );

    // The next run writes a real file.
    write(&csv, &"x".repeat(40_000));
    watching.poll(&state, SystemTime::now());

    let now = SystemTime::now();
    let assessments = state.read(|s| evaluate(s, now));
    dispatcher.dispatch(&assessments, now).await;

    let received = recorder.received();
    assert_eq!(received.len(), 2, "{received:?}");
    assert_eq!(received[1].level, Level::Recovered);
    assert!(
        received[1].text.contains("empty output"),
        "{}",
        received[1].text
    );
}

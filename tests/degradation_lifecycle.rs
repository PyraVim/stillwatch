//! Degradation end to end: a dependency's latency drifting, and the alerts a
//! person actually receives.
//!
//! Time is driven directly and no probe is ever made. Every scenario here — an
//! hour of baseline, a slow stretch, a recovery — runs in microseconds.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use stillwatch::config::Config;
use stillwatch::evaluate::{check_health, evaluate, CheckHealth, Unjudged};
use stillwatch::notify::{Dispatcher, Level, Notification, Notifier, NotifyError};
use stillwatch::state::{Observation, Outcome, SharedState, State};

/// A dependency judged on latency, and one watched for up/down only — two
/// shapes, so no assumption about "every check has a baseline" can creep in.
const CONFIG: &str = r#"
listen = "127.0.0.1:9111"

[[check]]
name     = "vendor-api"
type     = "http"
url      = "https://api.vendor.com/health"
interval = "30s"
timeout  = "3s"

  [check.degradation]
  baseline_window   = "1h"
  warn_multiple     = 3.0
  critical_multiple = 8.0
  absolute_ceiling  = "2s"

[[check]]
name       = "queue-broker"
url        = "https://broker.internal/ping"
interval   = "30s"
timeout    = "3s"
down_after = "90s"
"#;

const INTERVAL: u64 = 30;
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
    Config::from_toml(CONFIG, &HashMap::new()).expect("config should load")
}

fn fresh() -> (SharedState, Arc<Recorder>, Dispatcher) {
    let config = load();
    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    let recorder = Arc::new(Recorder::default());
    let dispatcher = Dispatcher::new(recorder.clone());
    (state, recorder, dispatcher)
}

fn ok(millis: u64) -> Outcome {
    Outcome::Responded(Duration::from_millis(millis))
}

fn err(message: &str) -> Outcome {
    Outcome::Failed(message.to_string())
}

/// Advances time from `from` to `to`, probing each named check on its own
/// interval and running evaluate-and-dispatch on every daemon tick.
///
/// The interleaving matters. Loading a whole history up front and only then
/// evaluating would let the evaluator see a complete baseline from its very
/// first tick, which is exactly the situation the cold-start and poisoning cases
/// are about *not* being in.
async fn simulate(
    state: &SharedState,
    dispatcher: &mut Dispatcher,
    from: u64,
    to: u64,
    probes: &[(&str, Outcome)],
) {
    let mut now = from;
    while now <= to {
        if now % INTERVAL == 0 {
            for (check, outcome) in probes {
                state.record_probe(
                    check,
                    Observation {
                        at: at(now),
                        outcome: outcome.clone(),
                    },
                );
            }
        }

        let assessments = state.read(|state| evaluate(state, at(now)));
        dispatcher.dispatch(&assessments, at(now)).await;
        now += TICK;
    }
}

fn health(state: &SharedState, check: &str, now: u64) -> CheckHealth {
    state
        .read(|state| check_health(state, at(now)))
        .into_iter()
        .find(|(name, _)| name == check)
        .map(|(_, health)| health)
        .expect("check should exist")
}

/// The spec's headline degradation case: latency climbing against a dependency's
/// own baseline while every single request still succeeds.
#[tokio::test]
async fn latency_rises_against_its_own_baseline_and_recovers() {
    let (state, recorder, mut dispatcher) = fresh();

    // An hour and a half of healthy 140ms responses.
    simulate(
        &state,
        &mut dispatcher,
        START,
        START + 5_400,
        &[("vendor-api", ok(140))],
    )
    .await;

    assert!(
        recorder.received().is_empty(),
        "a steady dependency is not news: {:?}",
        recorder.summary()
    );
    assert_eq!(health(&state, "vendor-api", START + 5_400), CheckHealth::Ok);

    // It slows to 1.4s — ten times its own normal, still under the 2s ceiling,
    // still 200 OK on every request.
    simulate(
        &state,
        &mut dispatcher,
        START + 5_405,
        START + 6_100,
        &[("vendor-api", ok(1_400))],
    )
    .await;

    assert_eq!(
        recorder.summary(),
        [("vendor-api".to_string(), Level::Critical)],
        "ten times its own baseline should be reported exactly once"
    );
    let text = &recorder.received()[0].text;
    assert!(text.contains("p90 1.4s"), "{text}");
    assert!(text.contains("baseline 140ms"), "{text}");
    assert!(
        text.contains("still responding"),
        "the alert must rule out an outage: {text}"
    );

    // It comes back, and the slow samples roll out of the recent window.
    simulate(
        &state,
        &mut dispatcher,
        START + 6_105,
        START + 7_000,
        &[("vendor-api", ok(140))],
    )
    .await;

    let summary = recorder.summary();
    assert_eq!(summary.len(), 2, "{summary:?}");
    assert_eq!(summary[1], ("vendor-api".to_string(), Level::Recovered));
    assert!(recorder.received()[1].text.contains("degraded for"));
}

/// Cold start: a check with no baseline must not be judged on multiples, and
/// must not be reported as healthy either.
#[tokio::test]
async fn a_check_is_not_judged_on_multiples_until_its_baseline_exists() {
    let (state, recorder, mut dispatcher) = fresh();

    // Ten minutes: enough probes to have data, not enough for a baseline.
    simulate(
        &state,
        &mut dispatcher,
        START,
        START + 600,
        &[("vendor-api", ok(140))],
    )
    .await;

    assert!(recorder.received().is_empty());
    assert!(
        matches!(
            health(&state, "vendor-api", START + 600),
            CheckHealth::NotJudged(Unjudged::Warming { needed: 30, .. })
        ),
        "a check with no baseline is warming, not ok: {:?}",
        health(&state, "vendor-api", START + 600)
    );
}

/// The failure mode that matters most: stillwatch starts while the dependency is
/// already degraded. The baseline learns that slow is normal, so the multiples
/// are useless — and the ceiling has to carry the whole thing.
#[tokio::test]
async fn a_dependency_already_degraded_at_startup_is_still_caught() {
    let (state, recorder, mut dispatcher) = fresh();

    // Two hours of uniformly slow responses from the very first probe. There is
    // no healthy period anywhere in the history to compare against.
    simulate(
        &state,
        &mut dispatcher,
        START,
        START + 7_200,
        &[("vendor-api", ok(3_000))],
    )
    .await;

    let summary = recorder.summary();
    assert!(
        !summary.is_empty(),
        "a watchdog that starts during an outage and stays silent is worse than none"
    );
    assert_eq!(summary[0], ("vendor-api".to_string(), Level::Warn));

    // The first alert fires long before any baseline exists, so it names the
    // ceiling and admits it has nothing to compare against.
    let first = &recorder.received()[0].text;
    assert!(
        first.contains("no baseline yet"),
        "the opening alert must not imply a comparison it could not make: {first}"
    );
    assert!(
        first.contains("2s"),
        "and must name the ceiling that caught it: {first}"
    );

    // By the end the baseline has filled — with nothing but slow samples. The
    // evaluator now knows the baseline is worthless, and says so.
    let assessments = state.read(|s| evaluate(s, at(START + 7_200)));
    let text = stillwatch::notify::render(&assessments[0], at(START + 7_200)).text;
    assert!(
        text.contains("learned that slow is normal"),
        "a poisoned baseline must be named rather than quoted as a reassuring \
         1.0x ratio: {text}"
    );

    // And it stays one alert. The condition never changed severity, so nobody
    // gets told twice.
    assert_eq!(recorder.received().len(), 1, "{:?}", recorder.summary());
}

/// A check with no `[check.degradation]` block is watched for up/down only,
/// however slow it gets.
#[tokio::test]
async fn a_check_without_degradation_rules_is_never_judged_on_latency() {
    let (state, recorder, mut dispatcher) = fresh();

    simulate(
        &state,
        &mut dispatcher,
        START,
        START + 7_200,
        &[("queue-broker", ok(30_000))],
    )
    .await;

    assert!(
        recorder.received().is_empty(),
        "a check that declared no latency rules must never produce a latency alert: {:?}",
        recorder.summary()
    );
    assert_eq!(
        health(&state, "queue-broker", START + 7_200),
        CheckHealth::Ok
    );
}

/// One failed probe is a blip. Sustained failure is an outage.
#[tokio::test]
async fn a_blip_is_not_an_outage_but_sustained_failure_is() {
    let (state, recorder, mut dispatcher) = fresh();

    simulate(
        &state,
        &mut dispatcher,
        START,
        START + 600,
        &[("queue-broker", ok(20))],
    )
    .await;

    // One failed probe, then straight back to healthy.
    simulate(
        &state,
        &mut dispatcher,
        START + 605,
        START + 635,
        &[("queue-broker", err("connection reset"))],
    )
    .await;
    simulate(
        &state,
        &mut dispatcher,
        START + 640,
        START + 900,
        &[("queue-broker", ok(20))],
    )
    .await;

    assert!(
        recorder.received().is_empty(),
        "one failed probe inside a healthy stretch must not page: {:?}",
        recorder.summary()
    );

    // Now it stays down past `down_after`.
    simulate(
        &state,
        &mut dispatcher,
        START + 905,
        START + 1_200,
        &[("queue-broker", err("connection refused"))],
    )
    .await;

    assert_eq!(
        recorder.summary(),
        [("queue-broker".to_string(), Level::Critical)]
    );
    assert!(recorder.received()[0].text.contains("connection refused"));

    // And recovers.
    simulate(
        &state,
        &mut dispatcher,
        START + 1_205,
        START + 1_400,
        &[("queue-broker", ok(20))],
    )
    .await;

    let summary = recorder.summary();
    assert_eq!(summary.len(), 2);
    assert_eq!(summary[1], ("queue-broker".to_string(), Level::Recovered));
    assert!(recorder.received()[1].text.contains("down for"));
}

/// A dependency that is down is reported as down, not as slow — and the two
/// checks never interfere with each other.
#[tokio::test]
async fn two_checks_are_assessed_independently() {
    let (state, recorder, mut dispatcher) = fresh();

    simulate(
        &state,
        &mut dispatcher,
        START,
        START + 5_400,
        &[("vendor-api", ok(140)), ("queue-broker", ok(20))],
    )
    .await;
    assert!(recorder.received().is_empty());

    // The broker dies; the vendor api stays perfectly healthy.
    simulate(
        &state,
        &mut dispatcher,
        START + 5_405,
        START + 5_700,
        &[
            ("vendor-api", ok(140)),
            ("queue-broker", err("no response within 3s")),
        ],
    )
    .await;

    assert_eq!(
        recorder.summary(),
        [("queue-broker".to_string(), Level::Critical)],
        "only the broker is in trouble"
    );
    assert_eq!(health(&state, "vendor-api", START + 5_700), CheckHealth::Ok);
}

/// Evaluation never sees the system clock, so the entire scenario is
/// reproducible from the same inputs.
#[tokio::test]
async fn evaluation_of_checks_is_reproducible() {
    let (state, _, mut dispatcher) = fresh();
    simulate(
        &state,
        &mut dispatcher,
        START,
        START + 5_400,
        &[("vendor-api", ok(140))],
    )
    .await;
    simulate(
        &state,
        &mut dispatcher,
        START + 5_405,
        START + 6_100,
        &[("vendor-api", ok(1_400))],
    )
    .await;

    let once = state.read(|s| evaluate(s, at(START + 6_100)));
    let twice = state.read(|s| evaluate(s, at(START + 6_100)));

    assert_eq!(once, twice);
    assert_eq!(once.len(), 1);
}

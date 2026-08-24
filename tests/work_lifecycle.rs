//! Work signals end to end: `worked`, data freshness, and counter ratios.
//!
//! These three back the failure modes the README leads with — a job that is up
//! and idle, one acting on frozen data, and one attempting work that never
//! lands. Time is driven directly; nothing sleeps.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use stillwatch::config::Config;
use stillwatch::evaluate::{evaluate, unjudged_signals, Condition, Unjudged};
use stillwatch::notify::{Dispatcher, Level, Notification, Notifier, NotifyError};
use stillwatch::state::{BeatDetail, SharedState, State};

/// Two unrelated workload shapes again: a scraper that beats continuously and
/// is judged on its parse rate, and a nightly ETL that is quiet by design.
const CONFIG: &str = r#"
listen = "127.0.0.1:9111"

[[job]]
name = "product-scraper"

  [job.alive]
  expect_every   = "60s"
  warn_after     = "5m"
  critical_after = "15m"

  [job.worked]
  warn_after     = "2h"
  critical_after = "6h"

  [job.freshness]
  warn_after     = "10m"
  critical_after = "30m"

  [[job.ratio]]
  name        = "parse rate"
  numerator   = "parsed"
  denominator = "fetched"
  window      = "1h"
  min         = 0.9
  min_sample  = 50
  message     = "fetching fine, parsing broken — source markup likely changed"

[[job]]
name = "nightly-sync"

  [job.worked]
  warn_after     = "26h"
  critical_after = "50h"

  [[job.ratio]]
  name        = "write rate"
  numerator   = "rows_written"
  denominator = "rows_read"
  window      = "24h"
  min         = 0.99
  min_sample  = 100
"#;

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

fn fresh() -> (SharedState, Arc<Recorder>, Dispatcher) {
    let config = Config::from_toml(CONFIG, &HashMap::new()).expect("config should load");
    let state = SharedState::new(State::new(at(START), &config.jobs, &config.checks));
    let recorder = Arc::new(Recorder::default());
    let dispatcher = Dispatcher::new(recorder.clone());
    (state, recorder, dispatcher)
}

fn detail(worked: bool, data_ts: Option<u64>, counters: &[(&str, f64)]) -> BeatDetail {
    BeatDetail {
        worked: Some(worked),
        data_ts: data_ts.map(at),
        counters: counters
            .iter()
            .map(|(name, value)| (name.to_string(), *value))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// Advances time, beating `job` on `every` seconds with `detail`, and running
/// evaluate-and-dispatch on every daemon tick.
async fn simulate(
    state: &SharedState,
    dispatcher: &mut Dispatcher,
    job: &str,
    from: u64,
    to: u64,
    every: u64,
    detail: impl Fn(u64) -> Option<BeatDetail>,
) {
    let mut now = from;
    while now <= to {
        if now.is_multiple_of(every) {
            if let Some(detail) = detail(now) {
                state.record_beat_with(job, at(now), &detail);
            }
        }

        let assessments = state.read(|state| evaluate(state, at(now)));
        dispatcher.dispatch(&assessments, at(now)).await;
        now += TICK;
    }
}

fn conditions(state: &SharedState, now: u64) -> Vec<Condition> {
    state
        .read(|s| evaluate(s, at(now)))
        .iter()
        .map(|a| a.reason.condition())
        .collect()
}

// ---------------------------------------------------------------------------
// alive but idle-dead
// ---------------------------------------------------------------------------

/// The failure the README leads with: the loop is running, every heartbeat is
/// on time, and nothing is getting done.
#[tokio::test]
async fn a_scraper_that_beats_perfectly_and_does_nothing_is_reported_but_never_paged() {
    let (state, recorder, mut dispatcher) = fresh();

    // Eight hours of flawless heartbeats carrying fresh data and healthy
    // counters — but never once `worked: true`.
    simulate(
        &state,
        &mut dispatcher,
        "product-scraper",
        START,
        START + 8 * 3_600,
        60,
        |now| {
            Some(detail(
                false,
                Some(now),
                &[("fetched", 120.0), ("parsed", 118.0)],
            ))
        },
    )
    .await;

    let summary = recorder.summary();
    assert_eq!(
        summary.len(),
        1,
        "one finding, not one per cycle: {summary:?}"
    );
    assert_eq!(summary[0].0, "product-scraper");
    assert_eq!(
        summary[0].1,
        Level::Warn,
        "eight hours past a six-hour critical threshold, but the loop is provably \
         alive: a quiet worked signal is a warn at most"
    );

    let text = &recorder.received()[0].text;
    assert!(text.contains("still beating"), "{text}");
    assert!(text.contains("idle-dead"), "{text}");
}

/// A job with nothing vouching for its loop is not covered by that cap.
#[tokio::test]
async fn a_nightly_job_that_misses_two_runs_goes_critical() {
    let (state, recorder, mut dispatcher) = fresh();

    simulate(
        &state,
        &mut dispatcher,
        "nightly-sync",
        START,
        START + 51 * 3_600,
        3_600,
        |_| None,
    )
    .await;

    // The scraper in this config is also silent throughout and produces its own
    // heartbeat alerts; this test is about the nightly job only.
    let levels: Vec<_> = recorder
        .summary()
        .into_iter()
        .filter(|(subject, _)| subject == "nightly-sync")
        .map(|(_, level)| level)
        .collect();

    assert_eq!(
        levels,
        [Level::Warn, Level::Critical],
        "warn at 26h, critical at 50h, and nothing in between"
    );
}

// ---------------------------------------------------------------------------
// acting on stale data
// ---------------------------------------------------------------------------

/// A job reporting in punctually about data that stopped moving.
#[tokio::test]
async fn frozen_data_is_caught_while_every_heartbeat_arrives_on_time() {
    let (state, recorder, mut dispatcher) = fresh();

    // The source froze at START. The scraper keeps beating and keeps working.
    simulate(
        &state,
        &mut dispatcher,
        "product-scraper",
        START,
        START + 2_400,
        60,
        |_| {
            Some(detail(
                true,
                Some(START),
                &[("fetched", 120.0), ("parsed", 118.0)],
            ))
        },
    )
    .await;

    let summary = recorder.summary();
    assert_eq!(
        summary,
        [
            ("product-scraper".to_string(), Level::Warn),
            ("product-scraper".to_string(), Level::Critical),
        ],
        "stale at 10m, critical at 30m"
    );

    let text = &recorder.received()[0].text;
    assert!(text.contains("acting on data"), "{text}");
    assert!(
        text.contains("this is the source, not the job"),
        "the alert must rule the job itself out: {text}"
    );
}

/// A job that never sends `data_ts` is not fresh and is not stale. It is
/// unjudged, and the tool says which.
#[tokio::test]
async fn a_job_that_never_sends_data_ts_is_reported_as_unjudged_not_fresh() {
    let (state, recorder, mut dispatcher) = fresh();

    simulate(
        &state,
        &mut dispatcher,
        "product-scraper",
        START,
        START + 300,
        60,
        |_| Some(detail(true, None, &[("fetched", 120.0), ("parsed", 118.0)])),
    )
    .await;

    assert!(
        recorder.received().is_empty(),
        "inside the grace window there is nothing to say"
    );

    let unjudged = state.read(|s| unjudged_signals(s, at(START + 300)));
    assert!(
        unjudged
            .iter()
            .any(|s| s.signal == "freshness" && s.why == Unjudged::NeverReported),
        "freshness must be reported as unjudged, not silently fine: {unjudged:?}"
    );
}

// ---------------------------------------------------------------------------
// working but not landing
// ---------------------------------------------------------------------------

/// Fetching fine, parsing broken. Every beat arrives, work is reported, and the
/// output has collapsed.
#[tokio::test]
async fn a_collapsed_parse_rate_is_caught_and_recovers() {
    let (state, recorder, mut dispatcher) = fresh();

    // A healthy hour first.
    simulate(
        &state,
        &mut dispatcher,
        "product-scraper",
        START,
        START + 3_600,
        60,
        |now| {
            Some(detail(
                true,
                Some(now),
                &[("fetched", 120.0), ("parsed", 118.0)],
            ))
        },
    )
    .await;
    assert!(
        recorder.received().is_empty(),
        "a healthy scraper is not news: {:?}",
        recorder.summary()
    );

    // The markup changes. Fetching carries on; parsing stops.
    simulate(
        &state,
        &mut dispatcher,
        "product-scraper",
        START + 3_660,
        START + 8_400,
        60,
        |now| {
            Some(detail(
                true,
                Some(now),
                &[("fetched", 120.0), ("parsed", 2.0)],
            ))
        },
    )
    .await;

    let summary = recorder.summary();
    assert_eq!(summary.len(), 1, "{summary:?}");
    assert_eq!(summary[0].1, Level::Warn);

    let text = &recorder.received()[0].text;
    assert!(text.contains("parse rate"), "{text}");
    assert!(text.contains("min 90%"), "{text}");
    assert!(
        text.contains("markup likely changed"),
        "the operator's own message must survive to the alert: {text}"
    );
    assert!(
        text.contains("the loop is running"),
        "the alert must rule the loop out: {text}"
    );

    // Someone fixes the parser.
    simulate(
        &state,
        &mut dispatcher,
        "product-scraper",
        START + 8_460,
        START + 15_000,
        60,
        |now| {
            Some(detail(
                true,
                Some(now),
                &[("fetched", 120.0), ("parsed", 119.0)],
            ))
        },
    )
    .await;

    let summary = recorder.summary();
    assert_eq!(summary.len(), 2, "{summary:?}");
    assert_eq!(summary[1].1, Level::Recovered);
    assert!(recorder.received()[1].text.contains("a low parse rate"));
}

/// Below `min_sample` a rule has no verdict, however bad the numbers look.
#[tokio::test]
async fn a_ratio_with_too_little_evidence_never_fires() {
    let (state, recorder, mut dispatcher) = fresh();

    // Twenty fetches, none parsed — a 0% rate on far too little evidence for a
    // rule that asked for fifty samples.
    simulate(
        &state,
        &mut dispatcher,
        "product-scraper",
        START,
        START + 600,
        60,
        |now| {
            Some(detail(
                true,
                Some(now),
                &[("fetched", 2.0), ("parsed", 0.0)],
            ))
        },
    )
    .await;

    assert!(
        recorder.received().is_empty(),
        "20 samples cannot condemn a rule that asked for 50: {:?}",
        recorder.summary()
    );
}

// ---------------------------------------------------------------------------
// the signals stay apart
// ---------------------------------------------------------------------------

/// A dead loop explains everything downstream of it. Reporting four findings
/// about one stopped job would bury the one that matters.
#[tokio::test]
async fn a_stopped_loop_is_one_message_not_four() {
    let (state, recorder, mut dispatcher) = fresh();

    // A healthy hour, then nothing at all for a day.
    simulate(
        &state,
        &mut dispatcher,
        "product-scraper",
        START,
        START + 3_600,
        60,
        |now| {
            Some(detail(
                true,
                Some(now),
                &[("fetched", 120.0), ("parsed", 118.0)],
            ))
        },
    )
    .await;
    assert!(
        recorder.received().is_empty(),
        "a healthy hour is not news: {:?}",
        recorder.summary()
    );
    let before = recorder.received().len();

    simulate(
        &state,
        &mut dispatcher,
        "product-scraper",
        START + 3_660,
        START + 30_000,
        60,
        |_| None,
    )
    .await;

    // Every other rule is now also breached: no work, stale data, dead ratio.
    // Only the heartbeat is reported.
    let after: Vec<_> = recorder.summary().into_iter().skip(before).collect();
    assert_eq!(
        after.len(),
        2,
        "warn then critical on the heartbeat: {after:?}"
    );

    for notification in recorder.received().iter().skip(before) {
        assert!(
            notification.text.contains("no heartbeat"),
            "a stopped loop should not also report its consequences: {}",
            notification.text
        );
    }

    assert_eq!(
        conditions(&state, START + 30_000),
        [Condition::NoHeartbeat],
        "liveness suppresses the signals that depend on it"
    );
}

/// A live job can have several genuinely independent problems, and each is its
/// own incident.
#[tokio::test]
async fn a_live_job_reports_each_of_its_problems_separately() {
    let (state, recorder, mut dispatcher) = fresh();

    // Beating on time, data frozen, parse rate collapsed, no work reported.
    simulate(
        &state,
        &mut dispatcher,
        "product-scraper",
        START,
        START + 3 * 3_600,
        60,
        |_| {
            Some(detail(
                false,
                Some(START),
                &[("fetched", 120.0), ("parsed", 1.0)],
            ))
        },
    )
    .await;

    let found = conditions(&state, START + 3 * 3_600);
    assert_eq!(
        found,
        [
            Condition::NoWork,
            Condition::Stale,
            Condition::Ratio("parse rate".to_string())
        ],
        "three distinct findings about one live job"
    );

    // Four messages for three conditions: staleness crossed its warn threshold
    // and then escalated, which is one incident escalating rather than two.
    let headlines: Vec<_> = recorder
        .received()
        .iter()
        .map(|n| n.text.lines().next().unwrap_or_default().to_string())
        .collect();

    assert_eq!(headlines.len(), 4, "{headlines:?}");
    assert_eq!(
        headlines
            .iter()
            .filter(|line| line.contains("no work"))
            .count(),
        1,
        "{headlines:?}"
    );
    assert_eq!(
        headlines
            .iter()
            .filter(|line| line.contains("parse rate"))
            .count(),
        1,
        "{headlines:?}"
    );
    assert_eq!(
        headlines
            .iter()
            .filter(|line| line.contains("acting on data"))
            .count(),
        2,
        "warn then critical on one staleness incident: {headlines:?}"
    );
}

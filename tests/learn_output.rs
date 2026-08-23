//! Phase 4 end to end: what `learn` writes, and whether it can be trusted.
//!
//! The property that matters most here is that the emitted block **loads**.
//! A learned config is pasted into a file and relied on for months; one that
//! stillwatch itself would reject, or that contains a threshold that can never
//! fire, is worse than no learn mode at all.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, SystemTime};

use stillwatch::config::Config;
use stillwatch::learn;
use stillwatch::state::{BeatDetail, Observation, Outcome, State};

const CONFIG: &str = r#"
listen = "127.0.0.1:9111"

[[job]]
name = "product-scraper"

[[job]]
name = "quiet-job"

[[check]]
name     = "vendor-api"
url      = "https://api.vendor.com/health"
interval = "30s"
timeout  = "3s"
"#;

const START: u64 = 1_755_000_000;

fn at(secs: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
}

fn config() -> Config {
    Config::from_toml(CONFIG, &HashMap::new()).expect("config should load")
}

fn worked() -> BeatDetail {
    BeatDetail {
        worked: Some(true),
        counters: BTreeMap::from([
            ("fetched".to_string(), 120.0),
            ("parsed".to_string(), 118.0),
        ]),
        ..BeatDetail::default()
    }
}

/// Records `count` beats every `every` seconds, starting at `from`.
fn beats(state: &mut State, job: &str, from: u64, count: u64, every: u64) -> u64 {
    let mut t = from;
    for _ in 0..count {
        state.record_beat_with(job, at(t), &worked());
        t += every;
    }
    t
}

fn learning() -> State {
    let config = config();
    let mut state = State::new(at(START), &config.jobs, &config.checks);
    state.start_journal();
    state
}

fn report_of(state: &State, now: u64) -> String {
    let journal = state.journal().expect("journal");
    learn::report(journal, &config(), None, at(now))
}

/// The whole point: what `learn` prints has to be something stillwatch can read
/// back and run. Anything less and the feature is a suggestion box.
#[test]
fn the_emitted_block_loads_as_a_config() {
    let mut state = learning();
    let end = beats(&mut state, "product-scraper", START, 300, 60);

    let report = report_of(&state, end);
    let loaded = Config::from_toml(&report, &HashMap::new())
        .expect("learn must emit a config stillwatch can read back");

    let job = loaded
        .jobs
        .iter()
        .find(|job| job.name == "product-scraper")
        .expect("the learned job");
    let alive = job.alive.expect("a derived alive block");

    assert_eq!(alive.expect_every, Duration::from_secs(60));
    assert!(alive.warn_after > alive.expect_every);
    assert!(alive.critical_after > alive.warn_after);
}

/// Regression, found by running it: at a one-second cadence the derivation
/// rounded `expect_every` to `"0s"` and landed `critical_after` on the same
/// value as `warn_after` — two configs stillwatch refuses to load. Checking one
/// cadence was not enough, so this checks the whole range.
#[test]
fn the_emitted_block_loads_at_every_cadence() {
    for every in [1, 2, 7, 30, 60, 300, 3_600, 86_400] {
        let mut state = learning();
        let end = beats(&mut state, "product-scraper", START, 300, every);

        let report = report_of(&state, end);
        let loaded = Config::from_toml(&report, &HashMap::new()).unwrap_or_else(|err| {
            panic!("cadence {every}s produced an unloadable config: {err}\n{report}")
        });

        let job = &loaded.jobs[0];
        let alive = job
            .alive
            .unwrap_or_else(|| panic!("cadence {every}s derived no alive block:\n{report}"));

        assert!(
            !alive.expect_every.is_zero(),
            "cadence {every}s rounded away to nothing"
        );
        assert!(alive.warn_after > alive.expect_every, "cadence {every}s");
        assert!(alive.critical_after > alive.warn_after, "cadence {every}s");

        let worked = job
            .worked
            .unwrap_or_else(|| panic!("cadence {every}s derived no worked block:\n{report}"));
        assert!(
            worked.critical_after.expect("critical") > worked.warn_after,
            "cadence {every}s: worked thresholds collided"
        );
    }
}

/// An outage inside the observation window must not become the cadence. This is
/// the failure the whole module is built around, and it is durable: a poisoned
/// threshold gets pasted into a file and trusted for months.
#[test]
fn an_outage_during_learning_does_not_become_the_threshold() {
    let mut state = learning();

    // Two and a half hours of steady minute beats, a forty minute outage, then
    // another two and a half hours — a realistic `learn --for 6h`.
    let mut t = beats(&mut state, "product-scraper", START, 150, 60);
    // `beats` has already advanced one interval, so this makes the gap exactly
    // forty minutes.
    t += 2_400 - 60;
    let end = beats(&mut state, "product-scraper", t, 150, 60);

    let report = report_of(&state, end);
    let loaded = Config::from_toml(&report, &HashMap::new()).expect("should load");
    let alive = loaded.jobs[0].alive.expect("alive");

    assert!(
        alive.warn_after < Duration::from_secs(2_400),
        "a threshold wider than the outage that produced it could never fire: {:?}",
        alive.warn_after
    );
    assert_eq!(alive.expect_every, Duration::from_secs(60));

    // And it says what it did, prominently, rather than quietly dropping data.
    assert!(report.contains("NOTE:"), "{report}");
    assert!(report.contains("EXCLUDED"), "{report}");
    assert!(
        report.contains("40m"),
        "the excluded gap should be named: {report}"
    );
}

/// But a window where the outage is a large share of the whole has no "normal"
/// left in it, and deriving anything from it would be guesswork wearing
/// evidence. It refuses instead.
#[test]
fn a_window_dominated_by_an_outage_is_refused_rather_than_salvaged() {
    let mut state = learning();

    // An hour of beats, forty minutes of nothing, an hour of beats: over a
    // quarter of the window is outage.
    let mut t = beats(&mut state, "product-scraper", START, 60, 60);
    t += 2_400;
    let end = beats(&mut state, "product-scraper", t, 60, 60);

    let report = report_of(&state, end);
    let loaded = Config::from_toml(&report, &HashMap::new()).expect("still valid TOML");

    assert!(loaded.jobs[0].alive.is_none(), "nothing should be derived");
    assert!(report.contains("more outage than cadence"), "{report}");
    assert!(
        report.contains("suspected incidents account for"),
        "{report}"
    );
}

/// Six samples must not become confident-looking numbers.
#[test]
fn a_window_too_short_to_mean_anything_is_refused() {
    let mut state = learning();
    let end = beats(&mut state, "product-scraper", START, 6, 60);

    let report = report_of(&state, end);

    assert!(report.contains("no [job.alive] block derived"), "{report}");
    assert!(report.contains("only 5 intervals"), "{report}");
    assert!(
        !report.contains("expect_every"),
        "a refusal must not also emit the thing it refused: {report}"
    );

    // Still valid TOML, so redirecting it to a file leaves an explanation
    // rather than a mystery.
    Config::from_toml(&report, &HashMap::new()).expect("a refusal is still valid TOML");
}

/// A job that never reported anything is named as such, not silently skipped.
#[test]
fn a_job_that_said_nothing_is_reported_rather_than_omitted() {
    let mut state = learning();
    let end = beats(&mut state, "product-scraper", START, 300, 60);

    let report = report_of(&state, end);

    assert!(report.contains("quiet-job"), "{report}");
    assert!(
        report.contains("nothing was received from this job"),
        "{report}"
    );
}

/// `worked` is derived from work, not from beats.
#[test]
fn the_worked_block_comes_from_work_not_heartbeats() {
    let mut state = learning();

    // Beating every minute, but only working once an hour.
    let mut t = START;
    for tick in 0..1_500u64 {
        let detail = if tick % 60 == 0 {
            worked()
        } else {
            BeatDetail::default()
        };
        state.record_beat_with("product-scraper", at(t), &detail);
        t += 60;
    }

    let report = report_of(&state, t);
    let loaded = Config::from_toml(&report, &HashMap::new()).expect("should load");
    let worked = loaded.jobs[0].worked.expect("a derived worked block");

    assert!(
        worked.warn_after > Duration::from_secs(3_600),
        "an hourly cadence should not warn inside the hour: {:?}",
        worked.warn_after
    );
    assert!(worked.critical_after.expect("critical") > worked.warn_after);
}

#[test]
fn a_job_that_never_reported_work_gets_no_worked_block() {
    let mut state = learning();
    let mut t = START;
    for _ in 0..300 {
        state.record_beat("product-scraper", at(t));
        t += 60;
    }

    let report = report_of(&state, t);
    let loaded = Config::from_toml(&report, &HashMap::new()).expect("should load");

    assert!(loaded.jobs[0].worked.is_none());
    assert!(report.contains("no beat reported worked:true"), "{report}");
}

/// Latency evidence for a check, with the ceiling as the one number worth
/// suggesting — the baseline rebuilds itself at runtime.
#[test]
fn check_latency_is_reported_as_evidence_with_a_suggested_ceiling() {
    let mut state = learning();
    let mut t = START;
    for tick in 0..200u64 {
        // Mostly 90ms with an occasional slower response.
        let millis = if tick % 20 == 0 { 260 } else { 90 };
        state.record_probe(
            "vendor-api",
            Observation {
                at: at(t),
                outcome: Outcome::Responded(Duration::from_millis(millis)),
            },
        );
        t += 30;
    }

    let report = report_of(&state, t);

    assert!(report.contains("vendor-api: p50 90ms"), "{report}");
    assert!(report.contains("absolute_ceiling"), "{report}");
    assert!(
        report.contains("200 probes"),
        "the evidence should be attached: {report}"
    );
    Config::from_toml(&report, &HashMap::new()).expect("still valid TOML");
}

#[test]
fn a_check_with_too_few_probes_gets_no_suggested_ceiling() {
    let mut state = learning();
    let mut t = START;
    for _ in 0..5 {
        state.record_probe(
            "vendor-api",
            Observation {
                at: at(t),
                outcome: Outcome::Responded(Duration::from_millis(90)),
            },
        );
        t += 30;
    }

    let report = report_of(&state, t);

    assert!(report.contains("only 5 successful probes"), "{report}");
    assert!(!report.contains("absolute_ceiling"), "{report}");
}

/// Learning one job at a time leaves the others out entirely.
#[test]
fn learning_a_single_job_reports_only_that_job() {
    let mut state = learning();
    let end = beats(&mut state, "product-scraper", START, 300, 60);

    let journal = state.journal().expect("journal");
    let report = learn::report(journal, &config(), Some("product-scraper"), at(end));

    assert!(report.contains("product-scraper"), "{report}");
    assert!(!report.contains("quiet-job"), "{report}");
    assert!(!report.contains("vendor-api"), "{report}");
}

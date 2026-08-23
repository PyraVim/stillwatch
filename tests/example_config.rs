//! The example config that ships in the repo has to actually work.
//!
//! A `stillwatch.toml.example` that no longer parses is the first thing a new
//! user hits and the last thing anyone thinks to check by hand.

use std::collections::HashMap;
use std::path::Path;

use stillwatch::config::Config;

fn env() -> HashMap<String, String> {
    [
        ("TELEGRAM_TOKEN", "12345:example-token"),
        ("TELEGRAM_CHAT", "-1001234567890"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

#[test]
fn the_shipped_example_config_loads() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("stillwatch.toml.example");

    let config = Config::load(&path, &env()).expect("the example config must load");

    assert_eq!(config.listen.to_string(), "127.0.0.1:9111");

    let telegram = config.telegram.expect("the example configures telegram");
    assert_eq!(telegram.token, "12345:example-token");
    assert_eq!(telegram.chat_id, "-1001234567890");
}

/// The example is meant to demonstrate both workload shapes, so that nobody
/// reading it walks away thinking a job must have a heartbeat cadence.
#[test]
fn the_example_covers_a_continuous_job_and_a_quiet_one() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("stillwatch.toml.example");
    let config = Config::load(&path, &env()).expect("the example config must load");

    let with_cadence = config.jobs.iter().filter(|j| j.alive.is_some()).count();
    let without_cadence = config.jobs.iter().filter(|j| j.alive.is_none()).count();

    assert!(with_cadence >= 1, "expected a continuously-running example");
    assert!(without_cadence >= 1, "expected a quiet-by-design example");
}

/// The example is the primary documentation for degradation config, so the
/// block it shows has to be one that actually resolves.
#[test]
fn the_example_demonstrates_a_probed_dependency() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("stillwatch.toml.example");
    let config = Config::load(&path, &env()).expect("the example config must load");

    let check = config.checks.first().expect("expected a [[check]] example");
    let degradation = check
        .degradation
        .expect("the example should show a degradation block");

    assert!(degradation.warn_multiple > 1.0);
    assert!(degradation.critical_multiple > degradation.warn_multiple);
    assert!(!degradation.absolute_ceiling.is_zero());
    assert!(
        degradation.recent_window < degradation.baseline_window,
        "the recent window must leave room for a baseline"
    );
}

#[test]
fn a_missing_config_file_names_the_path_it_could_not_read() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-config.toml");

    let err = Config::load(&path, &env()).expect_err("should fail");

    assert!(
        err.to_string().contains("no-such-config.toml"),
        "unhelpful error: {err}"
    );
}

//! Liveness evaluation.
//!
//! A pure function of state plus the time it is told. Nothing here reads the
//! system clock, opens a socket, or mutates anything: given the same state and
//! the same `now`, it returns the same answer. Tests drive time directly and
//! never sleep.

use std::time::{Duration, SystemTime};

use crate::config::DegradationConfig;
use crate::state::{percentile, CheckState, JobState, Observation, Outcome, State};

/// The percentile latency is judged at, for both the baseline and the current
/// window. A p90 ignores the one-in-ten slow request that every dependency has
/// while still moving as soon as most requests get slower.
const JUDGED_PERCENTILE: f64 = 0.9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Warn,
    Critical,
}

/// One thing that is wrong, right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    /// The job this is about. Alerts lead with it, never with a check id.
    pub subject: String,
    pub severity: Severity,
    pub reason: Reason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    NoHeartbeat {
        /// How long the silence has lasted, measured from `since`.
        silent_for: Duration,
        since: LastSeen,
        expect_every: Duration,
    },

    /// Every probe for at least `down_after` has failed.
    CheckDown {
        failing_for: Duration,
        failed_probes: usize,
        last_error: String,
    },

    /// Latency has crossed the ceiling, its own baseline, or both.
    Degraded {
        recent_p90: Duration,
        recent_window: Duration,
        recent_samples: usize,
        baseline: Baseline,
        baseline_window: Duration,
        absolute_ceiling: Duration,
        trigger: Trigger,
    },

    /// Nothing is slow right now, but the baseline this check is judged against
    /// has itself learned a normal at or above the ceiling — so the multiples
    /// can never fire and the check is not protecting anything.
    BaselineNotCredible {
        baseline_p90: Duration,
        baseline_samples: usize,
        baseline_window: Duration,
        absolute_ceiling: Duration,
    },
}

/// What the current latency is being compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Baseline {
    /// Not enough observations yet. The check is being judged against the
    /// ceiling alone, and the alert says so — "no baseline" must never be
    /// quietly reported as "healthy".
    Warming {
        samples: usize,
        needed: usize,
    },

    Ready {
        p90: Duration,
        samples: usize,
    },

    /// Enough observations, but they taught a normal at or above the ceiling.
    ///
    /// This is what a poisoned baseline looks like from the inside: stillwatch
    /// started while the dependency was already slow, learned that slow is
    /// normal, and the multiples will now never fire. Carried into the alert so
    /// a reader is told the comparison is worthless rather than being handed a
    /// reassuring ratio.
    NotCredible {
        p90: Duration,
        samples: usize,
    },
}

impl Baseline {
    /// The p90, if there is one worth comparing against.
    pub fn p90(&self) -> Option<Duration> {
        match self {
            Baseline::Ready { p90, .. } | Baseline::NotCredible { p90, .. } => Some(*p90),
            Baseline::Warming { .. } => None,
        }
    }
}

/// Which rule fired.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Trigger {
    /// Past `absolute_ceiling`. Evaluated with or without a baseline, and it is
    /// the only thing that fires when the baseline has been poisoned.
    Ceiling,

    /// A multiple of the check's own recent normal.
    Baseline {
        ratio: f64,
    },

    Both {
        ratio: f64,
    },
}

impl Eq for Trigger {}

/// What an incident is *about*, independent of its severity or detail.
///
/// This is half the deduplication key, alongside the subject. One subject can
/// have several things wrong with it at once — a job can be missing its
/// heartbeat while a counter ratio has also collapsed — and keying on the
/// subject alone would let the first of those silently suppress the rest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Condition {
    NoHeartbeat,
    Down,
    Degraded,
    UntrustworthyBaseline,
}

impl Reason {
    /// A few words naming the condition, reused verbatim in the all-clear so
    /// that "recovered — no heartbeat for 18m4s" lines up with the alert that
    /// opened the incident.
    pub fn headline(&self) -> String {
        match self {
            Reason::NoHeartbeat { .. } => "no heartbeat".to_string(),
            Reason::CheckDown { .. } => "down".to_string(),
            Reason::Degraded { .. } => "degraded".to_string(),
            Reason::BaselineNotCredible { .. } => "an untrustworthy baseline".to_string(),
        }
    }

    pub fn condition(&self) -> Condition {
        match self {
            Reason::NoHeartbeat { .. } => Condition::NoHeartbeat,
            Reason::CheckDown { .. } => Condition::Down,
            Reason::Degraded { .. } => Condition::Degraded,
            Reason::BaselineNotCredible { .. } => Condition::UntrustworthyBaseline,
        }
    }
}

/// Why a rule has no verdict yet.
///
/// One vocabulary for every signal in the tool, because it is one idea: a rule
/// that cannot yet reach a conclusion is *not* a rule that concluded everything
/// is fine. Silence caused by missing data and silence caused by good health
/// look identical from outside, so they are never represented the same way here
/// and never reported the same way to a reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unjudged {
    /// The signal has never been reported at all.
    ///
    /// Distinct from "not enough of it yet": nothing is on its way. Usually it
    /// means the job was never wired up to send it, or the config names a
    /// counter that does not exist.
    NeverReported,

    /// Reported, but there is not yet enough of it to conclude anything.
    Warming { have: u64, needed: u64 },
}

impl Unjudged {
    /// A phrase that completes "not judged: ...".
    pub fn describe(&self) -> String {
        match self {
            Unjudged::NeverReported => "never reported".to_string(),
            Unjudged::Warming { have, needed } => format!("{have} of {needed} so far"),
        }
    }
}

/// What a reader needs to know about whether a check is actually being judged.
///
/// The spec called for three states. There are four, because conflating "not
/// judged yet" with "healthy" is the same mistake as treating a job with no
/// heartbeat history as fine: in both cases the tool would be silent for a
/// reason that has nothing to do with the dependency being well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckHealth {
    /// Responding, but the baseline is not yet usable — only the ceiling is
    /// being applied.
    NotJudged(Unjudged),

    Ok,

    /// Responding well, but the baseline is far worse than current reality,
    /// which means it was learned during a slow stretch and will not fire when
    /// it should. Logged rather than alerted: this is the expected state for a
    /// window's length after every genuine incident recovers, and paging on it
    /// would add a message to every incident.
    OkWithStaleBaseline {
        baseline_p90: Duration,
        recent_p90: Duration,
    },

    Degraded,
    Down,
}

/// What the silence is being measured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastSeen {
    /// A beat was seen at this time.
    Beat(SystemTime),

    /// No beat has ever arrived, so the silence is measured from when this
    /// process started watching.
    ///
    /// This is the case that matters: a job that was already dead before
    /// stillwatch started has no history at all, and treating "no history" as
    /// "nothing to say" would make the watchdog silent about exactly the
    /// failure it exists to catch. What it must not do is invent a last-beat
    /// time it never observed, so the two cases stay distinguishable all the
    /// way into the alert text.
    WatchdogStart(SystemTime),
}

/// Assesses every job against the clock it is given.
///
/// Jobs come out in a fixed order, so two evaluations of the same state produce
/// byte-identical output.
pub fn evaluate(state: &State, now: SystemTime) -> Vec<Assessment> {
    let jobs = state
        .jobs()
        .filter_map(|job| assess_alive(job, state.started_at(), now));

    let checks = state
        .checks()
        .filter_map(|check| assess_check(check, now).1);

    jobs.chain(checks).collect()
}

/// Reports what each check's latency verdict is currently based on.
///
/// Separate from `evaluate` because most of these are not alerts: a check that
/// is warming up or judging against a stale baseline is not an incident, but a
/// reader still needs to be able to tell whether it is being judged at all.
pub fn check_health(state: &State, now: SystemTime) -> Vec<(String, CheckHealth)> {
    state
        .checks()
        .map(|check| (check.name().to_string(), assess_check(check, now).0))
        .collect()
}

fn assess_alive(job: &JobState, watch_started: SystemTime, now: SystemTime) -> Option<Assessment> {
    // No `[job.alive]` block means the job never claimed a cadence, so there is
    // no such thing as it being late. A nightly sync that is legitimately quiet
    // for twenty-three hours must not be judged against a rule it never agreed
    // to.
    let alive = job.config.alive?;

    let (since, measured_from) = match job.last_beat {
        Some(last) => (LastSeen::Beat(last), last),
        None => (LastSeen::WatchdogStart(watch_started), watch_started),
    };

    // A timestamp in the future means the clock stepped backwards, not that the
    // job has been silent for a negative time.
    let silent_for = now.duration_since(measured_from).unwrap_or(Duration::ZERO);

    let severity = if silent_for >= alive.critical_after {
        Severity::Critical
    } else if silent_for >= alive.warn_after {
        Severity::Warn
    } else {
        return None;
    };

    Some(Assessment {
        subject: job.name().to_string(),
        severity,
        reason: Reason::NoHeartbeat {
            silent_for,
            since,
            expect_every: alive.expect_every,
        },
    })
}

fn assess_check(check: &CheckState, now: SystemTime) -> (CheckHealth, Option<Assessment>) {
    let config = &check.config;
    let subject = check.name().to_string();

    // -- down --------------------------------------------------------------
    //
    // Every probe within `down_after` has to have failed. One blip inside an
    // otherwise healthy stretch is not an outage, and requiring at least one
    // observation means a check that has not run yet is never called down.
    let recent: Vec<&Observation> = check.window(config.down_after, now).collect();
    let all_failed = !recent.is_empty()
        && recent
            .iter()
            .all(|observation| matches!(observation.outcome, Outcome::Failed(_)));

    if all_failed {
        let run = failing_run(check);
        let failing_for = run
            .start
            .and_then(|start| now.duration_since(start).ok())
            .unwrap_or_default();

        // The failures have to have *lasted* `down_after`, not merely be the
        // only thing inside a window that long. Without this, a check whose very
        // first probe fails is called down immediately — the same mistake as
        // treating a job with no heartbeat history as dead, and it makes
        // `down_after` mean nothing on a cold start.
        if failing_for >= config.down_after {
            return (
                CheckHealth::Down,
                Some(Assessment {
                    subject,
                    severity: Severity::Critical,
                    reason: Reason::CheckDown {
                        failing_for,
                        failed_probes: run.count,
                        last_error: run.last_error,
                    },
                }),
            );
        }
    }

    // A check with no `[check.degradation]` block asked to be watched for
    // up/down only, and is never judged on latency.
    let Some(degradation) = config.degradation else {
        return (CheckHealth::Ok, None);
    };

    // -- current latency ---------------------------------------------------
    let recent: Vec<&Observation> = check.window(degradation.recent_window, now).collect();
    let recent_samples = recent
        .iter()
        .filter(|observation| matches!(observation.outcome, Outcome::Responded(_)))
        .count();

    let baseline = build_baseline(check, &degradation, now);

    let Some(recent_p90) = percentile(recent.iter().copied(), JUDGED_PERCENTILE) else {
        // Nothing has answered recently, but not for long enough to be down.
        // There is nothing to judge and nothing to claim.
        return (health_without_verdict(baseline), None);
    };

    // -- the ceiling, which does not depend on the baseline -----------------
    //
    // This is the whole defence against a poisoned baseline. It is evaluated
    // before any baseline exists and regardless of what one has learned.
    let over_ceiling = recent_p90 >= degradation.absolute_ceiling;

    // -- the multiples, which do --------------------------------------------
    let ratio = baseline.p90().and_then(|baseline_p90| {
        // A baseline of zero cannot be multiplied into anything meaningful, so
        // such a check rests on the ceiling alone.
        (!baseline_p90.is_zero()).then(|| recent_p90.as_secs_f64() / baseline_p90.as_secs_f64())
    });

    let from_baseline = ratio.and_then(|ratio| {
        if ratio >= degradation.critical_multiple {
            Some(Severity::Critical)
        } else if ratio >= degradation.warn_multiple {
            Some(Severity::Warn)
        } else {
            None
        }
    });

    // Crossing the ceiling is always worth at least a warning; the multiples
    // can raise it but never lower it.
    let severity = match (over_ceiling, from_baseline) {
        (true, Some(from_baseline)) => Some(from_baseline.max(Severity::Warn)),
        (true, None) => Some(Severity::Warn),
        (false, from_baseline) => from_baseline,
    };

    if let Some(severity) = severity {
        let trigger = match (over_ceiling, ratio, from_baseline.is_some()) {
            (true, Some(ratio), true) => Trigger::Both { ratio },
            (true, _, _) => Trigger::Ceiling,
            (false, Some(ratio), _) => Trigger::Baseline { ratio },
            // Unreachable: severity is only Some when one of the two fired.
            (false, None, _) => Trigger::Ceiling,
        };

        return (
            CheckHealth::Degraded,
            Some(Assessment {
                subject,
                severity,
                reason: Reason::Degraded {
                    recent_p90,
                    recent_window: degradation.recent_window,
                    recent_samples,
                    baseline,
                    baseline_window: degradation.baseline_window,
                    absolute_ceiling: degradation.absolute_ceiling,
                    trigger,
                },
            }),
        );
    }

    // -- nothing is wrong right now, but is the baseline worth anything? ----
    if let Baseline::NotCredible { p90, samples } = baseline {
        return (
            CheckHealth::Degraded,
            Some(Assessment {
                subject,
                severity: Severity::Warn,
                reason: Reason::BaselineNotCredible {
                    baseline_p90: p90,
                    baseline_samples: samples,
                    baseline_window: degradation.baseline_window,
                    absolute_ceiling: degradation.absolute_ceiling,
                },
            }),
        );
    }

    // A baseline far worse than current reality was learned during a slow
    // stretch. Worth saying, not worth paging: this is the ordinary state for a
    // window's length after any real incident clears.
    if let Baseline::Ready { p90, .. } = baseline {
        let stale = !p90.is_zero()
            && recent_p90.as_secs_f64() * degradation.warn_multiple <= p90.as_secs_f64();
        if stale {
            return (
                CheckHealth::OkWithStaleBaseline {
                    baseline_p90: p90,
                    recent_p90,
                },
                None,
            );
        }
    }

    (health_without_verdict(baseline), None)
}

fn health_without_verdict(baseline: Baseline) -> CheckHealth {
    match baseline {
        Baseline::Warming { samples, needed } => CheckHealth::NotJudged(Unjudged::Warming {
            have: samples as u64,
            needed: needed as u64,
        }),
        _ => CheckHealth::Ok,
    }
}

/// Builds the baseline from the window *before* the recent one.
fn build_baseline(
    check: &CheckState,
    degradation: &DegradationConfig,
    now: SystemTime,
) -> Baseline {
    let observations: Vec<&Observation> = check
        .window_between(degradation.baseline_window, degradation.recent_window, now)
        .collect();

    let samples = observations
        .iter()
        .filter(|observation| matches!(observation.outcome, Outcome::Responded(_)))
        .count();

    if samples < degradation.min_samples {
        return Baseline::Warming {
            samples,
            needed: degradation.min_samples,
        };
    }

    match percentile(observations.iter().copied(), JUDGED_PERCENTILE) {
        Some(p90) if p90 >= degradation.absolute_ceiling => Baseline::NotCredible { p90, samples },
        Some(p90) => Baseline::Ready { p90, samples },
        None => Baseline::Warming {
            samples,
            needed: degradation.min_samples,
        },
    }
}

struct FailingRun {
    start: Option<SystemTime>,
    count: usize,
    last_error: String,
}

/// Walks back from the newest observation for as long as probes were failing.
fn failing_run(check: &CheckState) -> FailingRun {
    let mut run = FailingRun {
        start: None,
        count: 0,
        last_error: "no detail recorded".to_string(),
    };

    for observation in check.newest_first() {
        match &observation.outcome {
            Outcome::Failed(error) => {
                if run.count == 0 {
                    run.last_error = error.clone();
                }
                run.count += 1;
                run.start = Some(observation.at);
            }
            Outcome::Responded(_) => break,
        }
    }

    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AliveConfig, JobConfig};

    const STARTED: u64 = 1_000;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// A continuously-running job: expected every 60s, warn at 5m, critical at 15m.
    fn scraper() -> JobConfig {
        JobConfig {
            alive: Some(AliveConfig {
                expect_every: Duration::from_secs(60),
                warn_after: Duration::from_secs(300),
                critical_after: Duration::from_secs(900),
            }),
            ..JobConfig::named("product-scraper")
        }
    }

    /// A job that is quiet by design and declares no liveness expectation.
    fn nightly() -> JobConfig {
        JobConfig {
            alive: None,
            ..JobConfig::named("nightly-sync")
        }
    }

    fn state(jobs: &[JobConfig]) -> State {
        State::new(at(STARTED), jobs, &[])
    }

    fn severities(assessments: &[Assessment]) -> Vec<(&str, Severity)> {
        assessments
            .iter()
            .map(|a| (a.subject.as_str(), a.severity))
            .collect()
    }

    // -- the dead-on-arrival case -----------------------------------------

    /// The failure the spec's "absent history means unknown" wording would have
    /// hidden: a job that was already dead when stillwatch started has no beat
    /// history at all. Measured from process start, it is still reported.
    #[test]
    fn a_job_that_was_already_dead_when_the_watch_began_is_still_reported() {
        let state = state(&[scraper()]);

        let at_warn = evaluate(&state, at(STARTED + 300));
        let at_critical = evaluate(&state, at(STARTED + 900));

        assert_eq!(severities(&at_warn), [("product-scraper", Severity::Warn)]);
        assert_eq!(
            severities(&at_critical),
            [("product-scraper", Severity::Critical)]
        );
    }

    /// ...and it says so honestly, rather than inventing a beat it never saw.
    #[test]
    fn a_never_seen_job_is_measured_from_process_start_not_a_fabricated_beat() {
        let state = state(&[scraper()]);

        let assessments = evaluate(&state, at(STARTED + 312));
        let Reason::NoHeartbeat {
            silent_for, since, ..
        } = &assessments[0].reason
        else {
            panic!(
                "expected a heartbeat reason, got {:?}",
                assessments[0].reason
            )
        };

        assert_eq!(*since, LastSeen::WatchdogStart(at(STARTED)));
        assert_eq!(*silent_for, Duration::from_secs(312));
    }

    // -- a restart is not an outage ---------------------------------------

    #[test]
    fn a_restart_with_no_history_produces_no_alert_before_the_first_threshold() {
        let state = state(&[scraper(), nightly()]);

        assert!(evaluate(&state, at(STARTED)).is_empty());
        assert!(evaluate(&state, at(STARTED + 60)).is_empty());
        assert!(evaluate(&state, at(STARTED + 299)).is_empty());
    }

    // -- alive is not the same as working ---------------------------------

    /// The test that proves the tool understands the problem. A job that is
    /// quiet by design and declares no liveness rule is never late, no matter
    /// how long the silence runs.
    #[test]
    fn a_quiet_by_design_job_never_pages_on_liveness() {
        let mut state = state(&[nightly()]);
        state.record_beat("nightly-sync", at(STARTED + 10));

        for elapsed in [3_600, 26 * 3_600, 50 * 3_600, 365 * 24 * 3_600] {
            assert!(
                evaluate(&state, at(STARTED + 10 + elapsed)).is_empty(),
                "a job with no [job.alive] block must never produce a liveness \
                 alert, but it did after {elapsed}s"
            );
        }
    }

    #[test]
    fn a_quiet_by_design_job_that_never_beats_at_all_still_never_pages() {
        let state = state(&[nightly()]);
        assert!(evaluate(&state, at(STARTED + 50 * 3_600)).is_empty());
    }

    // -- thresholds, both sides of every boundary -------------------------

    #[test]
    fn silence_shorter_than_warn_after_is_not_an_alert() {
        let mut state = state(&[scraper()]);
        state.record_beat("product-scraper", at(2_000));

        assert!(evaluate(&state, at(2_000 + 299)).is_empty());
    }

    #[test]
    fn warn_fires_exactly_at_warn_after() {
        let mut state = state(&[scraper()]);
        state.record_beat("product-scraper", at(2_000));

        assert_eq!(
            severities(&evaluate(&state, at(2_000 + 300))),
            [("product-scraper", Severity::Warn)]
        );
    }

    #[test]
    fn it_stays_a_warn_right_up_to_critical_after() {
        let mut state = state(&[scraper()]);
        state.record_beat("product-scraper", at(2_000));

        assert_eq!(
            severities(&evaluate(&state, at(2_000 + 899))),
            [("product-scraper", Severity::Warn)]
        );
    }

    #[test]
    fn critical_fires_exactly_at_critical_after() {
        let mut state = state(&[scraper()]);
        state.record_beat("product-scraper", at(2_000));

        assert_eq!(
            severities(&evaluate(&state, at(2_000 + 900))),
            [("product-scraper", Severity::Critical)]
        );
    }

    #[test]
    fn a_beating_job_produces_nothing() {
        let mut state = state(&[scraper()]);

        for tick in 0..10 {
            let now = at(2_000 + tick * 60);
            state.record_beat("product-scraper", now);
            assert!(evaluate(&state, now).is_empty());
        }
    }

    #[test]
    fn a_beat_resets_the_silence() {
        let mut state = state(&[scraper()]);
        state.record_beat("product-scraper", at(2_000));
        assert!(!evaluate(&state, at(2_400)).is_empty());

        state.record_beat("product-scraper", at(2_400));
        assert!(evaluate(&state, at(2_400)).is_empty());
    }

    #[test]
    fn silence_is_measured_from_the_last_beat_once_there_is_one() {
        let mut state = state(&[scraper()]);
        state.record_beat("product-scraper", at(5_000));

        let assessments = evaluate(&state, at(5_312));
        let Reason::NoHeartbeat {
            silent_for, since, ..
        } = &assessments[0].reason
        else {
            panic!(
                "expected a heartbeat reason, got {:?}",
                assessments[0].reason
            )
        };

        assert_eq!(*since, LastSeen::Beat(at(5_000)));
        assert_eq!(*silent_for, Duration::from_secs(312));
    }

    // -- purity ------------------------------------------------------------

    #[test]
    fn evaluation_depends_only_on_state_and_the_clock_it_is_given() {
        let mut state = state(&[scraper()]);
        state.record_beat("product-scraper", at(2_000));

        assert_eq!(evaluate(&state, at(2_500)), evaluate(&state, at(2_500)));
        assert_ne!(evaluate(&state, at(2_500)), evaluate(&state, at(3_000)));
    }

    #[test]
    fn a_clock_that_steps_backwards_does_not_invent_negative_silence() {
        let mut state = state(&[scraper()]);
        state.record_beat("product-scraper", at(5_000));

        assert!(evaluate(&state, at(4_000)).is_empty());
    }

    // -- two unrelated workload shapes together ---------------------------

    #[test]
    fn a_dead_continuous_job_and_a_quiet_daily_one_are_told_apart() {
        let mut state = state(&[scraper(), nightly()]);
        state.record_beat("product-scraper", at(2_000));
        state.record_beat("nightly-sync", at(2_000));

        // A day later: the scraper has been dead for hours, the nightly job is
        // just doing what nightly jobs do.
        let assessments = evaluate(&state, at(2_000 + 26 * 3_600));

        assert_eq!(
            severities(&assessments),
            [("product-scraper", Severity::Critical)]
        );
    }

    // -- checks: fixtures --------------------------------------------------

    use crate::config::{CheckConfig, DegradationConfig, ProbeConfig};
    use crate::state::{Observation, Outcome};

    const INTERVAL: u64 = 30;
    const CEILING_MS: u64 = 2_000;

    /// `interval` 30s, `recent_window` 10m (20 probes), `baseline_window` 1h,
    /// warn at 3x, critical at 8x, ceiling 2s, 30 samples needed.
    fn vendor_api() -> CheckConfig {
        CheckConfig {
            name: "vendor-api".into(),
            probe: ProbeConfig::Http {
                url: "https://api.vendor.com/health".parse().expect("valid url"),
            },
            interval: Duration::from_secs(INTERVAL),
            timeout: Duration::from_secs(3),
            down_after: Duration::from_secs(60),
            degradation: Some(DegradationConfig {
                baseline_window: Duration::from_secs(3_600),
                recent_window: Duration::from_secs(600),
                warn_multiple: 3.0,
                critical_multiple: 8.0,
                absolute_ceiling: Duration::from_millis(CEILING_MS),
                min_samples: 30,
            }),
        }
    }

    /// A check with no `[check.degradation]` block: up/down only.
    fn ping_only() -> CheckConfig {
        CheckConfig {
            name: "queue-broker".into(),
            degradation: None,
            ..vendor_api()
        }
    }

    fn with_checks(checks: &[CheckConfig]) -> State {
        State::new(at(STARTED), &[], checks)
    }

    /// Probes `check` every interval from `from` to `to`, each taking `millis`.
    fn probe_steadily(state: &mut State, check: &str, from: u64, to: u64, millis: u64) {
        let mut t = from;
        while t <= to {
            state.record_probe(
                check,
                Observation {
                    at: at(t),
                    outcome: Outcome::Responded(Duration::from_millis(millis)),
                },
            );
            t += INTERVAL;
        }
    }

    fn probe_failing(state: &mut State, check: &str, from: u64, to: u64, error: &str) {
        let mut t = from;
        while t <= to {
            state.record_probe(
                check,
                Observation {
                    at: at(t),
                    outcome: Outcome::Failed(error.to_string()),
                },
            );
            t += INTERVAL;
        }
    }

    fn health_of(state: &State, check: &str, now: SystemTime) -> CheckHealth {
        check_health(state, now)
            .into_iter()
            .find(|(name, _)| name == check)
            .map(|(_, health)| health)
            .expect("check should be present")
    }

    // -- cold start --------------------------------------------------------

    /// A check with no baseline yet must not produce a degradation verdict, and
    /// must not be reported as healthy either. "Not judged yet" is its own fact.
    #[test]
    fn a_check_with_no_baseline_yet_is_warming_not_ok() {
        let mut state = with_checks(&[vendor_api()]);
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 300, 90);

        let now = at(STARTED + 300);
        assert!(evaluate(&state, now).is_empty(), "nothing to alert on yet");
        assert!(
            matches!(
                health_of(&state, "vendor-api", now),
                CheckHealth::NotJudged(Unjudged::Warming { needed: 30, .. })
            ),
            "got {:?}",
            health_of(&state, "vendor-api", now)
        );
    }

    #[test]
    fn a_check_that_has_never_been_probed_is_warming_with_no_samples() {
        let state = with_checks(&[vendor_api()]);
        let now = at(STARTED + 10);

        assert!(evaluate(&state, now).is_empty());
        assert_eq!(
            health_of(&state, "vendor-api", now),
            CheckHealth::NotJudged(Unjudged::Warming {
                have: 0,
                needed: 30
            })
        );
    }

    #[test]
    fn a_check_becomes_ok_once_the_baseline_is_populated() {
        let mut state = with_checks(&[vendor_api()]);
        // An hour and a half of healthy probes.
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 5_400, 90);

        let now = at(STARTED + 5_400);
        assert!(evaluate(&state, now).is_empty());
        assert_eq!(health_of(&state, "vendor-api", now), CheckHealth::Ok);
    }

    /// The ceiling does not wait for a baseline. A dependency that is already
    /// unacceptably slow on the very first probes is reported immediately.
    #[test]
    fn the_ceiling_fires_during_warmup_before_any_baseline_exists() {
        let mut state = with_checks(&[vendor_api()]);
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 300, 3_000);

        let now = at(STARTED + 300);
        let assessments = evaluate(&state, now);

        assert_eq!(assessments.len(), 1);
        assert_eq!(assessments[0].subject, "vendor-api");
        assert_eq!(assessments[0].severity, Severity::Warn);

        let Reason::Degraded {
            trigger, baseline, ..
        } = &assessments[0].reason
        else {
            panic!("expected a degradation, got {:?}", assessments[0].reason)
        };
        assert_eq!(*trigger, Trigger::Ceiling);
        assert!(
            matches!(baseline, Baseline::Warming { .. }),
            "the alert must admit it has no baseline: {baseline:?}"
        );
    }

    // -- baseline poisoning ------------------------------------------------

    /// The failure mode that matters: stillwatch starts while the dependency is
    /// already degraded. The baseline learns that 3s is normal, so the multiples
    /// can never fire — and the ceiling is the only thing left. It must fire.
    #[test]
    fn a_baseline_learned_while_already_degraded_still_gets_caught_by_the_ceiling() {
        let mut state = with_checks(&[vendor_api()]);
        // Two hours of uniformly slow probes: the baseline is fully poisoned.
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 7_200, 3_000);

        let now = at(STARTED + 7_200);
        let assessments = evaluate(&state, now);

        assert_eq!(
            assessments.len(),
            1,
            "a poisoned baseline must not go quiet"
        );

        let Reason::Degraded {
            trigger, baseline, ..
        } = &assessments[0].reason
        else {
            panic!("expected a degradation, got {:?}", assessments[0].reason)
        };

        assert_eq!(
            *trigger,
            Trigger::Ceiling,
            "the multiples cannot fire against a baseline this bad; the ceiling must"
        );
        assert!(
            matches!(baseline, Baseline::NotCredible { .. }),
            "the alert must say the baseline is worthless, not quote a reassuring \
             ratio: {baseline:?}"
        );
    }

    /// ...and it says so in words, rather than reporting a 1.0x ratio as if that
    /// were reassuring.
    #[test]
    fn a_poisoned_baseline_is_named_in_the_alert_text() {
        let mut state = with_checks(&[vendor_api()]);
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 7_200, 3_000);

        let now = at(STARTED + 7_200);
        let notification = crate::notify::render(&evaluate(&state, now)[0], now);

        assert!(
            notification.text.contains("learned that slow is normal"),
            "{}",
            notification.text
        );
    }

    /// A dependency that recovered leaves a baseline worse than reality behind
    /// it. That is worth knowing but is not an incident — it is the ordinary
    /// state after every real slowdown clears, and paging on it would add a
    /// message to every incident.
    #[test]
    fn a_baseline_much_worse_than_reality_is_reported_but_never_paged() {
        let mut state = with_checks(&[vendor_api()]);
        // An hour slow, then back to normal for the recent window.
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 3_600, 1_500);
        probe_steadily(
            &mut state,
            "vendor-api",
            STARTED + 3_630,
            STARTED + 4_200,
            90,
        );

        let now = at(STARTED + 4_200);

        assert!(
            evaluate(&state, now).is_empty(),
            "a dependency getting faster is not an incident"
        );
        assert!(
            matches!(
                health_of(&state, "vendor-api", now),
                CheckHealth::OkWithStaleBaseline { .. }
            ),
            "but it must not be reported as plainly ok: {:?}",
            health_of(&state, "vendor-api", now)
        );
    }

    /// A baseline sitting above the ceiling means the check cannot protect
    /// anything, even while nothing is currently slow. That is the tool being
    /// confidently wrong, and it gets said out loud.
    #[test]
    fn a_baseline_above_the_ceiling_is_alerted_even_when_nothing_is_slow_now() {
        let mut state = with_checks(&[vendor_api()]);
        // Baseline window learned at 2.5s — above the 2s ceiling.
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 3_600, 2_500);
        // Recent window comfortably under the ceiling and near the baseline, so
        // neither the ceiling nor the multiples fire.
        probe_steadily(
            &mut state,
            "vendor-api",
            STARTED + 3_630,
            STARTED + 4_200,
            1_900,
        );

        let now = at(STARTED + 4_200);
        let assessments = evaluate(&state, now);

        assert_eq!(assessments.len(), 1);
        assert_eq!(assessments[0].severity, Severity::Warn);
        assert!(
            matches!(assessments[0].reason, Reason::BaselineNotCredible { .. }),
            "got {:?}",
            assessments[0].reason
        );
    }

    /// One subject may only produce one assessment per cycle: the dispatcher
    /// dedups on subject, so a second would be silently swallowed.
    #[test]
    fn a_check_never_produces_two_assessments_in_one_cycle() {
        let mut state = with_checks(&[vendor_api()]);
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 7_200, 3_000);

        let assessments = evaluate(&state, at(STARTED + 7_200));
        assert_eq!(assessments.len(), 1);
    }

    // -- degradation against a healthy baseline ----------------------------

    #[test]
    fn latency_rising_against_its_own_baseline_warns_while_every_probe_succeeds() {
        let mut state = with_checks(&[vendor_api()]);
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 3_600, 140);
        // 4x the baseline: past warn, short of critical, and still 200 OK.
        probe_steadily(
            &mut state,
            "vendor-api",
            STARTED + 3_630,
            STARTED + 4_200,
            560,
        );

        let now = at(STARTED + 4_200);
        let assessments = evaluate(&state, now);

        assert_eq!(assessments.len(), 1);
        assert_eq!(assessments[0].severity, Severity::Warn);

        let Reason::Degraded { trigger, .. } = &assessments[0].reason else {
            panic!("expected a degradation")
        };
        assert!(
            matches!(trigger, Trigger::Baseline { ratio } if (*ratio - 4.0).abs() < 0.01),
            "got {trigger:?}"
        );
    }

    #[test]
    fn a_big_enough_multiple_is_critical() {
        let mut state = with_checks(&[vendor_api()]);
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 3_600, 140);
        // 10x the baseline, still under the 2s ceiling.
        probe_steadily(
            &mut state,
            "vendor-api",
            STARTED + 3_630,
            STARTED + 4_200,
            1_400,
        );

        let assessments = evaluate(&state, at(STARTED + 4_200));

        assert_eq!(assessments.len(), 1);
        assert_eq!(assessments[0].severity, Severity::Critical);
    }

    #[test]
    fn latency_inside_the_multiples_and_under_the_ceiling_is_not_an_alert() {
        let mut state = with_checks(&[vendor_api()]);
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 3_600, 140);
        // Doubled, which is noise for most dependencies and below warn_multiple.
        probe_steadily(
            &mut state,
            "vendor-api",
            STARTED + 3_630,
            STARTED + 4_200,
            280,
        );

        assert!(evaluate(&state, at(STARTED + 4_200)).is_empty());
    }

    /// The ceiling raises a verdict the multiples would have missed, and never
    /// lowers one they caught.
    #[test]
    fn the_ceiling_and_the_multiples_take_the_worse_of_the_two() {
        let mut state = with_checks(&[vendor_api()]);
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 3_600, 900);
        // 2.5x the baseline — under warn_multiple — but past the 2s ceiling.
        probe_steadily(
            &mut state,
            "vendor-api",
            STARTED + 3_630,
            STARTED + 4_200,
            2_250,
        );

        let assessments = evaluate(&state, at(STARTED + 4_200));

        assert_eq!(assessments.len(), 1);
        assert_eq!(
            assessments[0].severity,
            Severity::Warn,
            "the ceiling alone is worth a warning"
        );
    }

    // -- down --------------------------------------------------------------

    #[test]
    fn a_single_failed_probe_is_not_an_outage() {
        let mut state = with_checks(&[ping_only()]);
        probe_steadily(&mut state, "queue-broker", STARTED, STARTED + 600, 90);
        probe_failing(
            &mut state,
            "queue-broker",
            STARTED + 630,
            STARTED + 630,
            "connection refused",
        );

        assert!(
            evaluate(&state, at(STARTED + 630)).is_empty(),
            "one blip must not page"
        );
    }

    /// Regression, found by running it: a check whose very first probe fails was
    /// called down on the spot, because one failure was the only thing inside a
    /// `down_after`-long window and therefore trivially "all" of it. The
    /// failures have to have lasted `down_after`, not just be alone in it.
    #[test]
    fn a_check_that_fails_its_very_first_probe_is_not_instantly_down() {
        let mut state = with_checks(&[ping_only()]);
        probe_failing(
            &mut state,
            "queue-broker",
            STARTED,
            STARTED,
            "connection refused",
        );

        assert!(
            evaluate(&state, at(STARTED)).is_empty(),
            "down_after must still be honoured on a cold start"
        );
        assert!(evaluate(&state, at(STARTED + 30)).is_empty());
    }

    #[test]
    fn a_cold_start_against_a_dead_dependency_is_reported_once_down_after_passes() {
        let mut state = with_checks(&[ping_only()]);
        probe_failing(
            &mut state,
            "queue-broker",
            STARTED,
            STARTED + 90,
            "connection refused",
        );

        // `down_after` is 60s and the failures now span 90s.
        let assessments = evaluate(&state, at(STARTED + 90));
        assert_eq!(assessments.len(), 1);
        assert_eq!(assessments[0].severity, Severity::Critical);
    }

    #[test]
    fn unbroken_failure_for_down_after_is_critical() {
        let mut state = with_checks(&[ping_only()]);
        probe_steadily(&mut state, "queue-broker", STARTED, STARTED + 600, 90);
        probe_failing(
            &mut state,
            "queue-broker",
            STARTED + 630,
            STARTED + 720,
            "connection refused",
        );

        let now = at(STARTED + 720);
        let assessments = evaluate(&state, now);

        assert_eq!(assessments.len(), 1);
        assert_eq!(assessments[0].severity, Severity::Critical);
        assert_eq!(health_of(&state, "queue-broker", now), CheckHealth::Down);

        let Reason::CheckDown {
            last_error,
            failed_probes,
            ..
        } = &assessments[0].reason
        else {
            panic!("expected a down reason")
        };
        assert_eq!(*failed_probes, 4);
        assert_eq!(last_error, "connection refused");
    }

    /// A down check is down, not slow. Latency is not reported for something
    /// that is not answering.
    #[test]
    fn a_down_check_reports_being_down_rather_than_degraded() {
        let mut state = with_checks(&[vendor_api()]);
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 3_600, 140);
        probe_failing(
            &mut state,
            "vendor-api",
            STARTED + 3_630,
            STARTED + 3_720,
            "no response within 3s",
        );

        let assessments = evaluate(&state, at(STARTED + 3_720));

        assert_eq!(assessments.len(), 1);
        assert!(matches!(assessments[0].reason, Reason::CheckDown { .. }));
    }

    #[test]
    fn a_check_with_no_degradation_block_is_never_judged_on_latency() {
        let mut state = with_checks(&[ping_only()]);
        // Two hours of appallingly slow but successful probes.
        probe_steadily(&mut state, "queue-broker", STARTED, STARTED + 7_200, 30_000);

        let now = at(STARTED + 7_200);
        assert!(
            evaluate(&state, now).is_empty(),
            "a check that declared no degradation rules must never produce one"
        );
        assert_eq!(health_of(&state, "queue-broker", now), CheckHealth::Ok);
    }

    // -- purity ------------------------------------------------------------

    #[test]
    fn check_evaluation_depends_only_on_state_and_the_given_clock() {
        let mut state = with_checks(&[vendor_api()]);
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 3_600, 140);
        probe_steadily(
            &mut state,
            "vendor-api",
            STARTED + 3_630,
            STARTED + 4_200,
            560,
        );

        let now = at(STARTED + 4_200);
        assert_eq!(evaluate(&state, now), evaluate(&state, now));
    }

    #[test]
    fn jobs_and_checks_are_assessed_together_in_a_fixed_order() {
        let mut state = State::new(at(STARTED), &[scraper()], &[vendor_api()]);
        probe_steadily(&mut state, "vendor-api", STARTED, STARTED + 7_200, 3_000);

        let subjects: Vec<_> = evaluate(&state, at(STARTED + 7_200))
            .into_iter()
            .map(|a| a.subject)
            .collect();

        assert_eq!(subjects, ["product-scraper", "vendor-api"]);
    }

    #[test]
    fn assessments_come_out_in_a_fixed_order() {
        let alive_nightly = JobConfig {
            alive: Some(AliveConfig {
                expect_every: Duration::from_secs(60),
                warn_after: Duration::from_secs(300),
                critical_after: Duration::from_secs(900),
            }),
            ..JobConfig::named("nightly-sync")
        };
        let state = state(&[scraper(), alive_nightly]);

        let subjects: Vec<_> = evaluate(&state, at(STARTED + 1_000))
            .into_iter()
            .map(|a| a.subject)
            .collect();

        assert_eq!(subjects, ["nightly-sync", "product-scraper"]);
    }
}

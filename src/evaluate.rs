//! Liveness evaluation.
//!
//! A pure function of state plus the time it is told. Nothing here reads the
//! system clock, opens a socket, or mutates anything: given the same state and
//! the same `now`, it returns the same answer. Tests drive time directly and
//! never sleep.

use std::time::{Duration, SystemTime};

use crate::state::{JobState, State};

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
}

impl Reason {
    /// A few words naming the condition, reused verbatim in the all-clear so
    /// that "recovered — no heartbeat for 18m4s" lines up with the alert that
    /// opened the incident.
    pub fn headline(&self) -> &'static str {
        match self {
            Reason::NoHeartbeat { .. } => "no heartbeat",
        }
    }
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
    state
        .jobs()
        .filter_map(|job| assess_alive(job, state.started_at(), now))
        .collect()
}

fn assess_alive(job: &JobState, watch_started: SystemTime, now: SystemTime) -> Option<Assessment> {
    // No `[job.alive]` block means the job never claimed a cadence, so there is
    // no such thing as it being late. A nightly sync that is legitimately quiet
    // for twenty-three hours must not be judged against a rule it never agreed
    // to.
    let alive = job.alive?;

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
        subject: job.name.clone(),
        severity,
        reason: Reason::NoHeartbeat {
            silent_for,
            since,
            expect_every: alive.expect_every,
        },
    })
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
            name: "product-scraper".into(),
            alive: Some(AliveConfig {
                expect_every: Duration::from_secs(60),
                warn_after: Duration::from_secs(300),
                critical_after: Duration::from_secs(900),
            }),
        }
    }

    /// A job that is quiet by design and declares no liveness expectation.
    fn nightly() -> JobConfig {
        JobConfig {
            name: "nightly-sync".into(),
            alive: None,
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
        } = &assessments[0].reason;

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
        } = &assessments[0].reason;

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

    #[test]
    fn assessments_come_out_in_a_fixed_order() {
        let alive_nightly = JobConfig {
            name: "nightly-sync".into(),
            alive: Some(AliveConfig {
                expect_every: Duration::from_secs(60),
                warn_after: Duration::from_secs(300),
                critical_after: Duration::from_secs(900),
            }),
        };
        let state = state(&[scraper(), alive_nightly]);

        let subjects: Vec<_> = evaluate(&state, at(STARTED + 1_000))
            .into_iter()
            .map(|a| a.subject)
            .collect();

        assert_eq!(subjects, ["nightly-sync", "product-scraper"]);
    }
}

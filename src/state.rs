//! In-memory state. No database — a watchdog that needs its own datastore is a
//! second thing that can fail.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use crate::config::{AliveConfig, JobConfig};

/// Everything the evaluator is allowed to look at.
#[derive(Debug)]
pub struct State {
    /// When this process started watching.
    ///
    /// Load-bearing: a job with no beats yet is measured from here, so a job
    /// that was already dead when stillwatch started is still reported.
    started_at: SystemTime,

    /// Keyed by job name. A `BTreeMap` rather than a `HashMap` so that
    /// evaluation visits jobs in a fixed order and alerts come out in a
    /// predictable sequence.
    jobs: BTreeMap<String, JobState>,
}

#[derive(Debug, Clone)]
pub struct JobState {
    pub name: String,

    /// `None` for a job that declared no liveness expectation.
    pub alive: Option<AliveConfig>,

    /// `None` until the first beat arrives. Not a stand-in for "long ago" —
    /// absence of history is its own fact.
    pub last_beat: Option<SystemTime>,

    pub beats: u64,
}

impl State {
    pub fn new(started_at: SystemTime, jobs: &[JobConfig]) -> Self {
        let jobs = jobs
            .iter()
            .map(|job| {
                let state = JobState {
                    name: job.name.clone(),
                    alive: job.alive,
                    last_beat: None,
                    beats: 0,
                };
                (job.name.clone(), state)
            })
            .collect();

        Self { started_at, jobs }
    }

    pub fn started_at(&self) -> SystemTime {
        self.started_at
    }

    /// Records a beat. Returns `false` if no job by that name is configured.
    ///
    /// Beats never create jobs. A beat for an unknown name means someone
    /// mistyped it, and inventing a job to match would leave the real one
    /// unwatched and nobody told.
    pub fn record_beat(&mut self, job: &str, at: SystemTime) -> bool {
        let Some(state) = self.jobs.get_mut(job) else {
            return false;
        };

        state.beats += 1;
        // A backwards clock step must not make a live job look stale, so the
        // last beat only ever moves forward.
        state.last_beat = Some(match state.last_beat {
            Some(previous) if previous > at => previous,
            _ => at,
        });
        true
    }

    pub fn job(&self, name: &str) -> Option<&JobState> {
        self.jobs.get(name)
    }

    pub fn jobs(&self) -> impl Iterator<Item = &JobState> {
        self.jobs.values()
    }
}

/// A handle to the shared state, cloneable into the receiver and the evaluation
/// loop.
#[derive(Clone, Debug)]
pub struct SharedState(Arc<Mutex<State>>);

impl SharedState {
    pub fn new(state: State) -> Self {
        Self(Arc::new(Mutex::new(state)))
    }

    /// Records a beat. Returns `false` if no job by that name is configured.
    pub fn record_beat(&self, job: &str, at: SystemTime) -> bool {
        self.lock().record_beat(job, at)
    }

    /// Runs `f` against the state while holding the lock.
    pub fn read<R>(&self, f: impl FnOnce(&State) -> R) -> R {
        f(&self.lock())
    }

    /// Poisoning is recovered from rather than propagated. Every mutation here
    /// is a single field write, so a panic elsewhere in the process cannot have
    /// left the state half-updated — and a watchdog that stops watching because
    /// an unrelated task panicked is worse than one working from slightly stale
    /// state.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn jobs() -> Vec<JobConfig> {
        vec![
            JobConfig {
                name: "product-scraper".into(),
                alive: Some(AliveConfig {
                    expect_every: Duration::from_secs(60),
                    warn_after: Duration::from_secs(300),
                    critical_after: Duration::from_secs(900),
                }),
            },
            JobConfig {
                name: "nightly-sync".into(),
                alive: None,
            },
        ]
    }

    #[test]
    fn a_fresh_state_has_no_history() {
        let state = State::new(at(1_000), &jobs());

        let job = state.job("product-scraper").expect("job");
        assert!(job.last_beat.is_none());
        assert_eq!(job.beats, 0);
        assert_eq!(state.started_at(), at(1_000));
    }

    #[test]
    fn recording_a_beat_updates_the_job() {
        let mut state = State::new(at(1_000), &jobs());

        assert!(state.record_beat("product-scraper", at(1_060)));
        assert!(state.record_beat("product-scraper", at(1_120)));

        let job = state.job("product-scraper").expect("job");
        assert_eq!(job.last_beat, Some(at(1_120)));
        assert_eq!(job.beats, 2);
    }

    #[test]
    fn a_beat_for_an_unknown_job_is_rejected_and_creates_nothing() {
        let mut state = State::new(at(1_000), &jobs());

        assert!(!state.record_beat("product-scrapper", at(1_060)));
        assert!(state.job("product-scrapper").is_none());
        assert_eq!(state.jobs().count(), 2);
    }

    #[test]
    fn a_backwards_clock_step_does_not_age_the_last_beat() {
        let mut state = State::new(at(1_000), &jobs());

        state.record_beat("nightly-sync", at(2_000));
        state.record_beat("nightly-sync", at(1_500));

        let job = state.job("nightly-sync").expect("job");
        assert_eq!(job.last_beat, Some(at(2_000)));
        assert_eq!(
            job.beats, 2,
            "the beat still counts, it just does not rewind"
        );
    }

    #[test]
    fn jobs_are_visited_in_a_fixed_order() {
        let state = State::new(at(1_000), &jobs());
        let names: Vec<_> = state.jobs().map(|j| j.name.as_str()).collect();
        assert_eq!(names, ["nightly-sync", "product-scraper"]);
    }

    #[test]
    fn the_shared_handle_sees_writes_from_its_clones() {
        let shared = SharedState::new(State::new(at(1_000), &jobs()));
        let clone = shared.clone();

        assert!(clone.record_beat("product-scraper", at(1_060)));

        let last = shared.read(|s| s.job("product-scraper").and_then(|j| j.last_beat));
        assert_eq!(last, Some(at(1_060)));
    }
}

//! In-memory state. No database — a watchdog that needs its own datastore is a
//! second thing that can fail.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use crate::config::{CheckConfig, JobConfig};

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

    /// Keyed by check name, ordered for the same reason.
    checks: BTreeMap<String, CheckState>,
}

#[derive(Debug, Clone)]
pub struct JobState {
    pub config: JobConfig,

    /// `None` until the first beat arrives. Not a stand-in for "long ago" —
    /// absence of history is its own fact.
    pub last_beat: Option<SystemTime>,

    /// When the job last reported that it actually did something.
    ///
    /// `None` means it never has, which is a different fact from "not for a
    /// while" and leads to a different alert. Only an explicit `worked: true`
    /// sets this — a bare beat says the loop ran, not that it accomplished
    /// anything, and that distinction is the whole point of the two signals.
    pub last_worked: Option<SystemTime>,

    /// How fresh the data was, as of the last beat that said so.
    ///
    /// `None` means no beat has ever carried `data_ts`. Such a job is not stale;
    /// it is unjudged, and reporting it as fresh would be a lie in the direction
    /// that costs the most.
    pub last_data_ts: Option<SystemTime>,

    pub beats: u64,

    /// Per-beat counter snapshots, oldest first. Empty when the job has no
    /// ratio rules, so a job nobody asked about costs nothing to remember.
    counters: VecDeque<CounterSample>,

    /// Every counter name this job has ever reported.
    ///
    /// Kept beyond the sample window so that a ratio naming a counter that has
    /// *never* arrived can be told apart from one whose counter simply has not
    /// arrived lately. The first is a typo; the second is a quiet hour.
    seen_counters: BTreeSet<String>,
}

/// The counters carried by one beat.
#[derive(Debug, Clone, PartialEq)]
pub struct CounterSample {
    pub at: SystemTime,
    pub counters: BTreeMap<String, f64>,
}

/// The optional detail a beat may carry beyond "the loop ran".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BeatDetail {
    pub worked: Option<bool>,
    pub data_ts: Option<SystemTime>,
    pub counters: BTreeMap<String, f64>,
}

impl JobState {
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// How far back counter samples are kept: the longest ratio window, plus a
    /// margin so a sum taken right at the edge is not short a beat.
    fn counter_retention(&self) -> Option<Duration> {
        let longest = self.config.ratios.iter().map(|ratio| ratio.window).max()?;
        Some(longest + longest / 10)
    }

    /// Counter samples in `(now - window, now]`, oldest first.
    pub fn counter_window(
        &self,
        window: Duration,
        now: SystemTime,
    ) -> impl Iterator<Item = &CounterSample> {
        let start = now.checked_sub(window);
        self.counters
            .iter()
            .filter(move |sample| start.is_none_or(|start| sample.at > start) && sample.at <= now)
    }

    /// Whether this counter name has ever appeared in any beat.
    pub fn has_ever_seen(&self, counter: &str) -> bool {
        self.seen_counters.contains(counter)
    }

    fn record(&mut self, at: SystemTime, detail: &BeatDetail) {
        self.beats += 1;

        // A backwards clock step must not make a live job look stale, so these
        // only ever move forward.
        self.last_beat = Some(forward(self.last_beat, at));

        if detail.worked == Some(true) {
            self.last_worked = Some(forward(self.last_worked, at));
        }

        if let Some(data_ts) = detail.data_ts {
            self.last_data_ts = Some(data_ts);
        }

        for name in detail.counters.keys() {
            if !self.seen_counters.contains(name) {
                self.seen_counters.insert(name.clone());
            }
        }

        let Some(retention) = self.counter_retention() else {
            return;
        };
        if detail.counters.is_empty() {
            return;
        }

        self.counters.push_back(CounterSample {
            at,
            counters: detail.counters.clone(),
        });

        if let Some(cutoff) = at.checked_sub(retention) {
            while self
                .counters
                .front()
                .is_some_and(|oldest| oldest.at < cutoff)
            {
                self.counters.pop_front();
            }
        }
    }
}

fn forward(previous: Option<SystemTime>, candidate: SystemTime) -> SystemTime {
    match previous {
        Some(previous) if previous > candidate => previous,
        _ => candidate,
    }
}

/// What one probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The dependency answered, and this is how long it took.
    Responded(Duration),

    /// The probe failed. The text is what to put in front of a person, so it is
    /// kept rather than reduced to a boolean.
    Failed(String),
}

/// One probe result, with the time it was taken.
///
/// Raw samples are kept rather than folded into a histogram. The window is only
/// a few thousand observations at any realistic interval, and keeping them means
/// a percentile over *any* time range can be computed on demand — which is what
/// lets the evaluator stay a pure function of state and the clock it is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub at: SystemTime,
    pub outcome: Outcome,
}

impl Observation {
    fn latency(&self) -> Option<Duration> {
        match self.outcome {
            Outcome::Responded(latency) => Some(latency),
            Outcome::Failed(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckState {
    pub config: CheckConfig,

    /// Oldest first. Pruned to the longest window anything asks about.
    observations: VecDeque<Observation>,
}

impl CheckState {
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// How far back observations are kept: whichever window is longer, plus a
    /// margin so a percentile taken right at the edge is not short a sample.
    fn retention(&self) -> Duration {
        let degradation = self
            .config
            .degradation
            .map(|d| d.baseline_window)
            .unwrap_or_default();

        degradation.max(self.config.down_after) + self.config.interval * 2
    }

    fn record(&mut self, observation: Observation) {
        let cutoff = observation.at.checked_sub(self.retention());
        self.observations.push_back(observation);

        if let Some(cutoff) = cutoff {
            while self
                .observations
                .front()
                .is_some_and(|oldest| oldest.at < cutoff)
            {
                self.observations.pop_front();
            }
        }
    }

    /// Observations in `(now - window, now]`, oldest first.
    pub fn window(&self, window: Duration, now: SystemTime) -> impl Iterator<Item = &Observation> {
        let start = now.checked_sub(window);
        self.observations.iter().filter(move |observation| {
            start.is_none_or(|start| observation.at > start) && observation.at <= now
        })
    }

    /// Observations in `[now - outer, now - inner)`, oldest first.
    ///
    /// Used for the baseline, which deliberately excludes the most recent
    /// stretch so that a slowdown still in progress does not get to teach the
    /// baseline that it is normal.
    pub fn window_between(
        &self,
        outer: Duration,
        inner: Duration,
        now: SystemTime,
    ) -> impl Iterator<Item = &Observation> {
        let start = now.checked_sub(outer);
        let end = now.checked_sub(inner);
        self.observations.iter().filter(move |observation| {
            start.is_none_or(|start| observation.at >= start)
                && end.is_some_and(|end| observation.at < end)
        })
    }

    /// The most recent observation, whatever it was.
    pub fn last(&self) -> Option<&Observation> {
        self.observations.back()
    }

    /// Every observation, most recent first.
    pub fn newest_first(&self) -> impl Iterator<Item = &Observation> {
        self.observations.iter().rev()
    }

    pub fn observation_count(&self) -> usize {
        self.observations.len()
    }
}

/// Nearest-rank percentile over successful probes.
///
/// `p` is a fraction: `0.9` for p90. Returns `None` when there is nothing to
/// compute from — an empty sample is not zero latency.
pub fn percentile<'a>(
    observations: impl Iterator<Item = &'a Observation>,
    p: f64,
) -> Option<Duration> {
    let mut latencies: Vec<Duration> = observations.filter_map(Observation::latency).collect();
    if latencies.is_empty() {
        return None;
    }

    latencies.sort_unstable();
    let rank = (p * latencies.len() as f64).ceil() as usize;
    let index = rank.clamp(1, latencies.len()) - 1;
    latencies.get(index).copied()
}

impl State {
    pub fn new(started_at: SystemTime, jobs: &[JobConfig], checks: &[CheckConfig]) -> Self {
        let jobs = jobs
            .iter()
            .map(|job| {
                let state = JobState {
                    config: job.clone(),
                    last_beat: None,
                    last_worked: None,
                    last_data_ts: None,
                    beats: 0,
                    counters: VecDeque::new(),
                    seen_counters: BTreeSet::new(),
                };
                (job.name.clone(), state)
            })
            .collect();

        let checks = checks
            .iter()
            .map(|check| {
                let state = CheckState {
                    config: check.clone(),
                    observations: VecDeque::new(),
                };
                (check.name.clone(), state)
            })
            .collect();

        Self {
            started_at,
            jobs,
            checks,
        }
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
        self.record_beat_with(job, at, &BeatDetail::default())
    }

    /// Records a beat carrying optional detail. Returns `false` for an unknown job.
    pub fn record_beat_with(&mut self, job: &str, at: SystemTime, detail: &BeatDetail) -> bool {
        match self.jobs.get_mut(job) {
            Some(state) => {
                state.record(at, detail);
                true
            }
            None => false,
        }
    }

    pub fn job(&self, name: &str) -> Option<&JobState> {
        self.jobs.get(name)
    }

    pub fn jobs(&self) -> impl Iterator<Item = &JobState> {
        self.jobs.values()
    }

    /// Records a probe result. Returns `false` if no check by that name exists.
    pub fn record_probe(&mut self, check: &str, observation: Observation) -> bool {
        match self.checks.get_mut(check) {
            Some(state) => {
                state.record(observation);
                true
            }
            None => false,
        }
    }

    pub fn check(&self, name: &str) -> Option<&CheckState> {
        self.checks.get(name)
    }

    pub fn checks(&self) -> impl Iterator<Item = &CheckState> {
        self.checks.values()
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

    /// Records a beat carrying optional detail. Returns `false` for an unknown job.
    pub fn record_beat_with(&self, job: &str, at: SystemTime, detail: &BeatDetail) -> bool {
        self.lock().record_beat_with(job, at, detail)
    }

    /// Records a probe result. Returns `false` if no check by that name exists.
    pub fn record_probe(&self, check: &str, observation: Observation) -> bool {
        self.lock().record_probe(check, observation)
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
    use crate::config::AliveConfig;
    use crate::config::{DegradationConfig, ProbeConfig};

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn jobs() -> Vec<JobConfig> {
        vec![
            JobConfig {
                alive: Some(AliveConfig {
                    expect_every: Duration::from_secs(60),
                    warn_after: Duration::from_secs(300),
                    critical_after: Duration::from_secs(900),
                }),
                ..JobConfig::named("product-scraper")
            },
            JobConfig {
                alive: None,
                ..JobConfig::named("nightly-sync")
            },
        ]
    }

    #[test]
    fn a_fresh_state_has_no_history() {
        let state = State::new(at(1_000), &jobs(), &[]);

        let job = state.job("product-scraper").expect("job");
        assert!(job.last_beat.is_none());
        assert_eq!(job.beats, 0);
        assert_eq!(state.started_at(), at(1_000));
    }

    #[test]
    fn recording_a_beat_updates_the_job() {
        let mut state = State::new(at(1_000), &jobs(), &[]);

        assert!(state.record_beat("product-scraper", at(1_060)));
        assert!(state.record_beat("product-scraper", at(1_120)));

        let job = state.job("product-scraper").expect("job");
        assert_eq!(job.last_beat, Some(at(1_120)));
        assert_eq!(job.beats, 2);
    }

    #[test]
    fn a_beat_for_an_unknown_job_is_rejected_and_creates_nothing() {
        let mut state = State::new(at(1_000), &jobs(), &[]);

        assert!(!state.record_beat("product-scrapper", at(1_060)));
        assert!(state.job("product-scrapper").is_none());
        assert_eq!(state.jobs().count(), 2);
    }

    #[test]
    fn a_backwards_clock_step_does_not_age_the_last_beat() {
        let mut state = State::new(at(1_000), &jobs(), &[]);

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
        let state = State::new(at(1_000), &jobs(), &[]);
        let names: Vec<_> = state.jobs().map(|j| j.name()).collect();
        assert_eq!(names, ["nightly-sync", "product-scraper"]);
    }

    // -- checks ------------------------------------------------------------

    fn check_config(interval_secs: u64, baseline_secs: u64) -> CheckConfig {
        let interval = Duration::from_secs(interval_secs);
        CheckConfig {
            name: "vendor-api".into(),
            probe: ProbeConfig::Http {
                url: "https://api.vendor.com/health".parse().expect("valid url"),
            },
            interval,
            timeout: interval / 2,
            down_after: interval * 2,
            degradation: Some(DegradationConfig {
                baseline_window: Duration::from_secs(baseline_secs),
                recent_window: interval * 20,
                warn_multiple: 3.0,
                critical_multiple: 8.0,
                absolute_ceiling: Duration::from_secs(2),
                min_samples: 30,
            }),
        }
    }

    fn responded(secs: u64, millis: u64) -> Observation {
        Observation {
            at: at(secs),
            outcome: Outcome::Responded(Duration::from_millis(millis)),
        }
    }

    #[test]
    fn a_probe_for_an_unknown_check_is_rejected() {
        let mut state = State::new(at(1_000), &[], &[check_config(30, 3_600)]);

        assert!(!state.record_probe("vendor-apy", responded(1_030, 90)));
        assert!(state.record_probe("vendor-api", responded(1_030, 90)));
    }

    #[test]
    fn observations_older_than_the_retention_window_are_dropped() {
        let mut state = State::new(at(0), &[], &[check_config(30, 600)]);

        // Twelve hours of probes at 30s against a 10m baseline window: only the
        // recent handful should survive.
        for tick in 0..1_440 {
            state.record_probe("vendor-api", responded(tick * 30, 90));
        }

        let check = state.check("vendor-api").expect("check");
        assert!(
            check.observation_count() < 40,
            "expected pruning, kept {}",
            check.observation_count()
        );
        assert!(check.observation_count() > 20, "pruned too aggressively");
    }

    #[test]
    fn the_recent_window_is_the_samples_inside_it() {
        let mut state = State::new(at(0), &[], &[check_config(30, 3_600)]);
        for tick in 0..20 {
            state.record_probe("vendor-api", responded(tick * 30, 90));
        }

        let check = state.check("vendor-api").expect("check");
        let now = at(19 * 30);
        assert_eq!(check.window(Duration::from_secs(150), now).count(), 5);
    }

    /// The baseline deliberately excludes the most recent stretch, so a slowdown
    /// still in progress cannot teach the baseline that it is normal.
    #[test]
    fn the_baseline_window_excludes_the_recent_window() {
        let mut state = State::new(at(0), &[], &[check_config(30, 3_600)]);
        for tick in 0..40 {
            state.record_probe("vendor-api", responded(tick * 30, 90));
        }

        let check = state.check("vendor-api").expect("check");
        let now = at(39 * 30);

        let baseline: Vec<_> = check
            .window_between(Duration::from_secs(1_200), Duration::from_secs(300), now)
            .collect();
        let recent: Vec<_> = check.window(Duration::from_secs(300), now).collect();

        assert!(!baseline.is_empty() && !recent.is_empty());
        for observation in &baseline {
            assert!(
                !recent.contains(observation),
                "the two windows must not overlap"
            );
        }
    }

    #[test]
    fn percentiles_are_nearest_rank_over_successful_probes_only() {
        let observations: Vec<Observation> = (1..=10)
            .map(|n| responded(n, n * 100))
            .chain(std::iter::once(Observation {
                at: at(11),
                outcome: Outcome::Failed("timed out".into()),
            }))
            .collect();

        // A failed probe has no latency and must not be counted as fast.
        assert_eq!(
            percentile(observations.iter(), 0.9),
            Some(Duration::from_millis(900))
        );
        assert_eq!(
            percentile(observations.iter(), 0.5),
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn a_percentile_of_nothing_is_none_not_zero() {
        let nothing: Vec<Observation> = Vec::new();
        assert_eq!(percentile(nothing.iter(), 0.9), None);

        let only_failures = [Observation {
            at: at(1),
            outcome: Outcome::Failed("connection refused".into()),
        }];
        assert_eq!(
            percentile(only_failures.iter(), 0.9),
            None,
            "an empty sample is not zero latency"
        );
    }

    #[test]
    fn the_shared_handle_sees_writes_from_its_clones() {
        let shared = SharedState::new(State::new(at(1_000), &jobs(), &[]));
        let clone = shared.clone();

        assert!(clone.record_beat("product-scraper", at(1_060)));

        let last = shared.read(|s| s.job("product-scraper").and_then(|j| j.last_beat));
        assert_eq!(last, Some(at(1_060)));
    }
}

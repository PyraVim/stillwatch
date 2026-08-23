//! Deriving thresholds from observed reality.
//!
//! Nobody knows their job's real cadence off the top of their head, and a
//! watchdog with wrong thresholds either pages constantly until it is muted or
//! never fires at all. `learn` watches for a while and then writes down what it
//! saw, with the evidence attached to every number so you can argue with it.
//!
//! The hazard this module is mostly built around: **if the observation window
//! contains an incident, naive derivation learns that broken is normal.** A
//! forty-minute outage during learning becomes a forty-minute "worst gap",
//! which becomes a threshold that the real failure can never cross. That is the
//! same poisoning a rolling baseline suffers, except worse — a baseline heals
//! as its window rolls, while a learned threshold gets pasted into a config
//! file and trusted for months.
//!
//! So nothing here derives silently. Suspected incidents are found, excluded,
//! and named in the output; a window that is more incident than cadence is
//! refused outright; and a window too short or too quiet to mean anything is
//! refused rather than turned into confident-looking numbers.

use std::fmt;
use std::time::{Duration, SystemTime};

use crate::config::{Config, RatioConfig};
use crate::state::{percentile, JobHistory, Journal, Observation};

/// A gap this many times the median is treated as an incident, not cadence.
///
/// Ordinary jitter does not reach 5x. A crashed loop, a missed schedule or a
/// restart comfortably does.
const ANOMALY_MULTIPLE: f64 = 5.0;

/// If more than this share of the observed time sits inside suspected
/// incidents, the window is not a picture of normal and nothing is derived.
const MAX_INCIDENT_SHARE: f64 = 0.25;

/// Fewer intervals than this and any percentile taken from them is noise.
const MIN_INTERVALS: usize = 20;

/// `warn_after` for a tight cadence is this many times the worst ordinary gap.
const ALIVE_WARN_MULTIPLE: u32 = 4;

/// `critical_after` is this many times `warn_after`.
const ALIVE_CRITICAL_MULTIPLE: u32 = 3;

/// Irregular signals get a margin over the worst gap rather than a multiple:
/// "one run late" and "two runs late".
const SILENCE_WARN_MARGIN: f64 = 1.25;
const SILENCE_CRITICAL_MARGIN: f64 = 2.5;

// ---------------------------------------------------------------------------
// refusals
// ---------------------------------------------------------------------------

/// Why nothing was derived.
///
/// Emitting a number anyway would be worse than emitting none: a threshold in a
/// config file looks equally authoritative whether it came from three hundred
/// samples or six.
/// `Eq` by hand: `share` is an `f64` computed from durations, never NaN.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// The signal never arrived during the window.
    NothingObserved,

    /// It arrived, but not enough of it.
    TooFewIntervals { have: usize, needed: usize },

    /// So much of the window sits inside suspected incidents that there is no
    /// "normal" left to learn from.
    MostlyIncident {
        incidents: usize,
        share: f64,
        longest: Duration,
    },
}

impl Eq for Refusal {}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NothingObserved => {
                write!(f, "nothing was observed during the window")
            }
            Refusal::TooFewIntervals { have, needed } => write!(
                f,
                "only {have} intervals observed, and {needed} are needed before a \
                 percentile means anything — watch for longer, or check the job is \
                 actually reporting"
            ),
            Refusal::MostlyIncident {
                incidents,
                share,
                longest,
            } => write!(
                f,
                "{incidents} suspected incidents account for {}% of the window (the \
                 longest was {}); this window is more outage than cadence, so nothing \
                 derived from it would describe normal",
                (share * 100.0).round(),
                crate::fmt::duration(*longest),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// cadence
// ---------------------------------------------------------------------------

/// The shape of a series of intervals, once suspected incidents are set aside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cadence {
    /// Intervals kept for derivation, sorted.
    pub ordinary: Vec<Duration>,

    /// Intervals judged to be incidents rather than cadence. Never derived
    /// from, always reported.
    pub incidents: Vec<Duration>,

    pub median: Duration,
    pub p99: Duration,

    /// The worst *ordinary* gap. Thresholds are never tighter than this.
    pub worst: Duration,

    /// Wall time from the first observation to the last.
    pub span: Duration,
}

impl Cadence {
    pub fn samples(&self) -> usize {
        self.ordinary.len()
    }
}

/// Derives the cadence of a series of event times.
pub fn cadence(times: &[SystemTime]) -> Result<Cadence, Refusal> {
    if times.len() < 2 {
        return Err(Refusal::NothingObserved);
    }

    let mut gaps: Vec<Duration> = times
        .windows(2)
        .map(|pair| pair[1].duration_since(pair[0]).unwrap_or(Duration::ZERO))
        .collect();
    gaps.sort_unstable();

    let span = times
        .last()
        .zip(times.first())
        .and_then(|(last, first)| last.duration_since(*first).ok())
        .unwrap_or(Duration::ZERO);

    let overall_median = median(&gaps);

    // The median is used as the yardstick because it is robust: a handful of
    // enormous gaps barely move it, which is exactly the property needed when
    // the enormous gaps are what we are trying to find. A mean would be dragged
    // toward the outage and quietly stop seeing it as one.
    let threshold = overall_median.mul_f64(ANOMALY_MULTIPLE);
    let (incidents, ordinary): (Vec<Duration>, Vec<Duration>) =
        gaps.iter().partition(|gap| **gap > threshold);

    if ordinary.len() < MIN_INTERVALS {
        // Distinguish "too little happened" from "too much of it was an
        // outage": they call for different actions.
        let incident_time: Duration = incidents.iter().sum();
        let share = share_of(incident_time, span);
        if !incidents.is_empty() && share > MAX_INCIDENT_SHARE {
            return Err(mostly_incident(&incidents, share));
        }
        return Err(Refusal::TooFewIntervals {
            have: ordinary.len(),
            needed: MIN_INTERVALS,
        });
    }

    let incident_time: Duration = incidents.iter().sum();
    let share = share_of(incident_time, span);
    if share > MAX_INCIDENT_SHARE {
        return Err(mostly_incident(&incidents, share));
    }

    Ok(Cadence {
        median: median(&ordinary),
        p99: quantile(&ordinary, 0.99),
        worst: ordinary.last().copied().unwrap_or_default(),
        ordinary,
        incidents,
        span,
    })
}

fn mostly_incident(incidents: &[Duration], share: f64) -> Refusal {
    Refusal::MostlyIncident {
        incidents: incidents.len(),
        share,
        longest: incidents.iter().max().copied().unwrap_or_default(),
    }
}

fn share_of(part: Duration, whole: Duration) -> f64 {
    if whole.is_zero() {
        return 1.0;
    }
    part.as_secs_f64() / whole.as_secs_f64()
}

/// Median of a sorted slice.
fn median(sorted: &[Duration]) -> Duration {
    quantile(sorted, 0.5)
}

/// Nearest-rank quantile of a sorted slice.
fn quantile(sorted: &[Duration], q: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    let index = rank.clamp(1, sorted.len()) - 1;
    sorted[index]
}

// ---------------------------------------------------------------------------
// thresholds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliveThresholds {
    pub expect_every: Duration,
    pub warn_after: Duration,
    pub critical_after: Duration,
}

/// Derives liveness thresholds from an observed cadence.
///
/// `expect_every` is the typical gap; the thresholds are multiples of the
/// *worst ordinary* gap, so a normal slow beat can never trip them.
pub fn alive_thresholds(cadence: &Cadence) -> AliveThresholds {
    let expect_every = round_nearest(cadence.median);

    // The `max` calls are not belt and braces — they are what makes the emitted
    // config valid by construction. stillwatch refuses a warn threshold inside
    // the beat interval, and refuses a critical that is not beyond the warn; at
    // a fast cadence, rounding alone can land on both. A learn mode that emits
    // a config stillwatch itself will not load is worse than none.
    let warn_after = round_up(cadence.worst * ALIVE_WARN_MULTIPLE).max(expect_every * 2);
    let critical_after =
        round_up(warn_after * ALIVE_CRITICAL_MULTIPLE).max(warn_after + Duration::from_secs(1));

    AliveThresholds {
        expect_every,
        warn_after,
        critical_after,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SilenceThresholds {
    pub warn_after: Duration,
    pub critical_after: Duration,
}

/// Derives thresholds for an irregular signal — `worked`, or freshness.
///
/// These are margins over the worst ordinary gap rather than multiples of it,
/// because the useful thresholds for something that happens nightly are "one
/// run late" and "two runs late", not "four times the usual wait".
pub fn silence_thresholds(worst: Duration) -> SilenceThresholds {
    let warn_after = round_up(worst.mul_f64(SILENCE_WARN_MARGIN)).max(Duration::from_secs(1));
    // As above: the ordering is guaranteed here rather than left to rounding,
    // because a config that does not load is not a suggestion, it is a bug.
    let critical_after =
        round_up(worst.mul_f64(SILENCE_CRITICAL_MARGIN)).max(warn_after + Duration::from_secs(1));

    SilenceThresholds {
        warn_after,
        critical_after,
    }
}

/// Rounds up to a duration a person would have written by hand.
///
/// Always upward: the spec's rule is that no emitted threshold is ever tighter
/// than the worst thing actually observed.
pub fn round_up(d: Duration) -> Duration {
    let step = step_for(d).max(1);
    // Ceiling the sub-second part first. Truncating it would quietly eat the
    // margin at fast cadences — a 1.25s threshold became 1s, exactly the worst
    // gap observed, so it would fire on the next ordinary slow beat.
    let secs = (d.as_secs_f64().ceil() as u64).max(1);
    Duration::from_secs(secs.div_ceil(step) * step)
}

/// Rounds to the nearest readable duration.
///
/// Used only for `expect_every`, which is a description of the job's cadence
/// rather than a threshold that fires — 60s reads better than 61s and nothing
/// is judged against it directly.
pub fn round_nearest(d: Duration) -> Duration {
    let step = step_for(d).max(1);
    let secs = d.as_secs();
    // Never round a real cadence away to nothing. A job beating every second
    // has a cadence of one second, and `expect_every = "0s"` is a config
    // stillwatch would refuse to load.
    Duration::from_secs((((secs + step / 2) / step) * step).max(1))
}

fn step_for(d: Duration) -> u64 {
    match d.as_secs() {
        // Fine-grained at the bottom: a job beating every second or two is a
        // real thing, and a five-second step would round its cadence to zero.
        0..=10 => 1,
        11..=90 => 5,
        91..=600 => 30,
        601..=3_600 => 60,
        3_601..=86_400 => 900,
        _ => 3_600,
    }
}

// ---------------------------------------------------------------------------
// the report
// ---------------------------------------------------------------------------

/// Writes the whole learned config block.
///
/// Always valid TOML, including when everything was refused: the output is
/// meant to be redirected to a file, and a file that is silently empty teaches
/// nothing. Refusals come out as comments explaining themselves.
pub fn report(journal: &Journal, config: &Config, only: Option<&str>, now: SystemTime) -> String {
    let mut out = String::new();

    let watched = journal
        .jobs
        .values()
        .map(|history| history.beats.len())
        .sum::<usize>();

    out.push_str("# ---------------------------------------------------------------\n");
    out.push_str("# learned by stillwatch — observe-only, nothing was alerted on\n");
    out.push_str(&format!(
        "# {watched} beats across {} job(s), {} check(s)\n",
        journal.jobs.len(),
        journal.checks.len()
    ));
    out.push_str("#\n");
    out.push_str("# Every number below has its evidence beside it. Thresholds are never\n");
    out.push_str("# tighter than the worst thing observed, and suspected incidents are\n");
    out.push_str("# excluded from the derivation and named where they were found.\n");
    out.push_str("# ---------------------------------------------------------------\n");

    for job in &config.jobs {
        if only.is_some_and(|wanted| wanted != job.name) {
            continue;
        }
        out.push('\n');
        out.push_str(&job_section(&job.name, journal.jobs.get(&job.name), now));

        if let Some(history) = journal.jobs.get(&job.name) {
            for rule in &job.ratios {
                out.push_str(&ratio_section(rule, history, now));
            }
        }
    }

    for check in &config.checks {
        if only.is_some_and(|wanted| wanted != check.name) {
            continue;
        }
        out.push('\n');
        out.push_str(&check_section(&check.name, journal.checks.get(&check.name)));
    }

    out
}

fn job_section(name: &str, history: Option<&JobHistory>, now: SystemTime) -> String {
    let mut out = format!("[[job]]\nname = {name:?}\n");

    let Some(history) = history else {
        out.push_str("# nothing was received from this job during the window; either it\n");
        out.push_str("# was not running, or it is not sending beats to this address\n");
        return out;
    };

    out.push_str(&alive_block(&history.beats));
    out.push_str(&worked_block(&history.worked));
    out.push_str(&freshness_block(&history.data, now));
    out
}

fn alive_block(beats: &[SystemTime]) -> String {
    match cadence(beats) {
        Err(refusal) => format!("\n  # no [job.alive] block derived: {refusal}\n",),
        Ok(cadence) => {
            let derived = alive_thresholds(&cadence);
            let mut out = String::from("\n  [job.alive]\n");

            out.push_str(&incident_note(&cadence, "beat"));
            out.push_str(&format!(
                "  expect_every   = {:?}    # observed p50 {}, p99 {}, worst {} over {} beats\n",
                spell(derived.expect_every),
                crate::fmt::duration(cadence.median),
                crate::fmt::duration(cadence.p99),
                crate::fmt::duration(cadence.worst),
                cadence.samples() + 1,
            ));
            out.push_str(&format!(
                "  warn_after     = {:?}    # {ALIVE_WARN_MULTIPLE}x the worst gap seen\n",
                spell(derived.warn_after),
            ));
            out.push_str(&format!(
                "  critical_after = {:?}    # {ALIVE_CRITICAL_MULTIPLE}x warn_after\n",
                spell(derived.critical_after),
            ));
            out
        }
    }
}

fn worked_block(worked: &[SystemTime]) -> String {
    match cadence(worked) {
        Err(Refusal::NothingObserved) => String::from(
            "\n  # no [job.worked] block derived: no beat reported worked:true, so there\n\
             \x20 # is no work cadence to learn. If this job does do work, it is not\n\
             \x20 # saying so.\n",
        ),
        Err(refusal) => format!("\n  # no [job.worked] block derived: {refusal}\n"),
        Ok(cadence) => {
            let derived = silence_thresholds(cadence.worst);
            let mut out = String::from("\n  [job.worked]\n");

            out.push_str(&incident_note(&cadence, "work"));
            out.push_str(&format!(
                "  warn_after     = {:?}   # worst ordinary gap between work was {}, \
                 over {} runs\n",
                spell(derived.warn_after),
                crate::fmt::duration(cadence.worst),
                cadence.samples() + 1,
            ));
            out.push_str(&format!(
                "  critical_after = {:?}   # roughly two of those in a row\n",
                spell(derived.critical_after),
            ));
            out
        }
    }
}

fn freshness_block(data: &[(SystemTime, SystemTime)], _now: SystemTime) -> String {
    if data.is_empty() {
        return String::from(
            "\n  # no [job.freshness] block derived: no beat carried data_ts, so there is\n\
             \x20 # no data age to learn from\n",
        );
    }

    // Staleness as the job itself saw it: how old its data was at the moment it
    // reported in.
    let mut stale: Vec<Duration> = data
        .iter()
        .map(|(beat, data_ts)| beat.duration_since(*data_ts).unwrap_or(Duration::ZERO))
        .collect();
    stale.sort_unstable();

    if stale.len() < MIN_INTERVALS {
        return format!(
            "\n  # no [job.freshness] block derived: only {} beats carried data_ts, and \
             {MIN_INTERVALS} are needed\n",
            stale.len()
        );
    }

    let worst = stale.last().copied().unwrap_or_default();
    let derived = silence_thresholds(worst);

    format!(
        "\n  [job.freshness]\n\
         \x20 warn_after     = {:?}   # data was p50 {} old, worst {} over {} beats\n\
         \x20 critical_after = {:?}\n",
        spell(derived.warn_after),
        crate::fmt::duration(median(&stale)),
        crate::fmt::duration(worst),
        stale.len(),
        spell(derived.critical_after),
    )
}

fn ratio_section(rule: &RatioConfig, history: &JobHistory, now: SystemTime) -> String {
    let start = now.checked_sub(rule.window);
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    let mut samples = 0usize;

    for sample in &history.counters {
        if start.is_some_and(|start| sample.at < start) {
            continue;
        }
        let Some(value) = sample.counters.get(&rule.denominator) else {
            continue;
        };
        denominator += value;
        numerator += sample.counters.get(&rule.numerator).copied().unwrap_or(0.0);
        samples += 1;
    }

    if samples == 0 || denominator <= 0.0 {
        return format!(
            "\n  # no floor derived for ratio {:?}: no beat in the window carried {:?}\n",
            rule.name, rule.denominator,
        );
    }

    let observed = numerator / denominator;
    // A floor a little under what was actually achieved, so ordinary variation
    // does not trip it on day one.
    let suggested = ((observed * 0.95) * 100.0).floor() / 100.0;

    format!(
        "\n  # ratio {:?}: observed {} over {} {} / {} {} in {} beats\n\
         \x20 # a floor of {:.2} sits just under that\n",
        rule.name,
        crate::fmt::percent(observed),
        crate::fmt::count(numerator),
        rule.numerator,
        crate::fmt::count(denominator),
        rule.denominator,
        samples,
        suggested.max(0.01),
    )
}

fn check_section(name: &str, observations: Option<&Vec<Observation>>) -> String {
    let Some(observations) = observations else {
        return format!("# {name}: never probed during the window\n");
    };

    let responded = observations
        .iter()
        .filter(|o| matches!(o.outcome, crate::state::Outcome::Responded(_)))
        .count();
    let failed = observations.len() - responded;

    if responded < MIN_INTERVALS {
        return format!(
            "# {name}: only {responded} successful probes, and {MIN_INTERVALS} are needed \
             before a latency percentile means anything\n"
        );
    }

    let p50 = percentile(observations.iter(), 0.5).unwrap_or_default();
    let p90 = percentile(observations.iter(), 0.9).unwrap_or_default();
    let p99 = percentile(observations.iter(), 0.99).unwrap_or_default();

    // The ceiling is the one degradation number worth suggesting: the multiples
    // are relative to a baseline the daemon rebuilds for itself at runtime, but
    // the ceiling has to be a real number somebody chooses.
    let ceiling = round_up_latency(p99.mul_f64(3.0));

    format!(
        "# {name}: p50 {}, p90 {}, p99 {} over {responded} probes ({failed} failed)\n\
         # the baseline rebuilds itself at runtime, so only the ceiling is worth\n\
         # writing down — 3x the observed p99 is {}\n\
         #\n\
         #   [check.degradation]\n\
         #   absolute_ceiling = {:?}\n",
        crate::fmt::latency(p50),
        crate::fmt::latency(p90),
        crate::fmt::latency(p99),
        crate::fmt::latency(ceiling),
        spell(ceiling),
    )
}

/// Latency ceilings are sub-second things; the duration ladder is too coarse.
fn round_up_latency(d: Duration) -> Duration {
    let millis = d.as_millis().max(1) as u64;
    let step = match millis {
        0..=1_000 => 50,
        1_001..=10_000 => 500,
        _ => 1_000,
    };
    Duration::from_millis(millis.div_ceil(step) * step)
}

/// Notes any suspected incidents, prominently, right above the numbers they
/// were kept out of.
fn incident_note(cadence: &Cadence, noun: &str) -> String {
    if cadence.incidents.is_empty() {
        return String::new();
    }

    let longest = cadence.incidents.iter().max().copied().unwrap_or_default();
    format!(
        "  # NOTE: {} {noun} gap(s) up to {} looked like incidents rather than cadence\n\
         \x20 # (over {}x the median) and were EXCLUDED from the numbers below.\n\
         \x20 # If those were normal for this job, widen the thresholds by hand.\n",
        cadence.incidents.len(),
        crate::fmt::duration(longest),
        ANOMALY_MULTIPLE as u64,
    )
}

/// Spells a duration the way the config parser reads it back.
fn spell(d: Duration) -> String {
    crate::fmt::duration(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// A steady series of beats every `every` seconds.
    fn steady(count: usize, every: u64) -> Vec<SystemTime> {
        (0..count).map(|i| at(1_000 + i as u64 * every)).collect()
    }

    // -- the spec's own example --------------------------------------------

    /// The spec's worked example: p50 60.2s, worst 71s becomes 60s / 5m / 15m.
    #[test]
    fn a_steady_minute_cadence_derives_the_thresholds_from_the_spec() {
        let mut beats = steady(300, 60);
        // One ordinary slow beat, well inside jitter.
        beats.push(*beats.last().expect("beats") + Duration::from_secs(71));

        let cadence = cadence(&beats).expect("should derive");
        let derived = alive_thresholds(&cadence);

        assert_eq!(derived.expect_every, Duration::from_secs(60));
        assert_eq!(
            derived.warn_after,
            Duration::from_secs(300),
            "4x71s, rounded"
        );
        assert_eq!(derived.critical_after, Duration::from_secs(900));
    }

    // -- the poisoning case ------------------------------------------------

    /// The failure this module exists for: an incident inside the observation
    /// window. Derived naively, the outage becomes the "worst gap" and the
    /// thresholds it produces can never fire.
    #[test]
    fn an_incident_in_the_window_is_excluded_rather_than_learned() {
        let mut beats = steady(200, 60);
        // A forty minute outage, then back to normal.
        let resumed = *beats.last().expect("beats") + Duration::from_secs(2_400);
        beats.push(resumed);
        for i in 1..=100 {
            beats.push(resumed + Duration::from_secs(i * 60));
        }

        let cadence = cadence(&beats).expect("should derive");

        assert_eq!(cadence.incidents.len(), 1);
        assert_eq!(cadence.incidents[0], Duration::from_secs(2_400));
        assert_eq!(
            cadence.worst,
            Duration::from_secs(60),
            "the outage must not become the worst ordinary gap"
        );

        let derived = alive_thresholds(&cadence);
        assert_eq!(derived.expect_every, Duration::from_secs(60));
        assert_eq!(
            derived.warn_after,
            Duration::from_secs(240),
            "4x a one-minute cadence, not 4x the outage"
        );
        assert!(
            derived.warn_after < Duration::from_secs(2_400),
            "a threshold wider than the outage that inspired it could never fire"
        );
    }

    /// ...and the exclusion is stated, not done quietly.
    #[test]
    fn excluded_incidents_are_named_in_the_output() {
        let mut beats = steady(200, 60);
        beats.push(*beats.last().expect("beats") + Duration::from_secs(2_400));
        for _ in 0..100 {
            beats.push(*beats.last().expect("beats") + Duration::from_secs(60));
        }

        let block = alive_block(&beats);

        assert!(block.contains("NOTE:"), "{block}");
        assert!(block.contains("EXCLUDED"), "{block}");
        assert!(block.contains("40m"), "{block}");
    }

    /// A window that is mostly outage has no "normal" in it to learn.
    #[test]
    fn a_window_that_is_mostly_incident_is_refused_outright() {
        // Twenty-five ordinary beats, then hours of nothing but long gaps.
        let mut beats = steady(25, 60);
        let mut t = *beats.last().expect("beats");
        for _ in 0..6 {
            t += Duration::from_secs(3_600);
            beats.push(t);
        }

        let refusal = cadence(&beats).expect_err("should refuse");
        assert!(
            matches!(refusal, Refusal::MostlyIncident { .. }),
            "got {refusal:?}"
        );
        assert!(refusal.to_string().contains("more outage than cadence"));
    }

    // -- too short, too quiet ----------------------------------------------

    #[test]
    fn six_samples_are_refused_rather_than_turned_into_numbers() {
        let refusal = cadence(&steady(6, 60)).expect_err("should refuse");

        assert_eq!(
            refusal,
            Refusal::TooFewIntervals {
                have: 5,
                needed: MIN_INTERVALS
            }
        );
        assert!(refusal.to_string().contains("watch for longer"));
    }

    #[test]
    fn a_signal_that_never_arrived_is_refused() {
        assert_eq!(cadence(&[]), Err(Refusal::NothingObserved));
        assert_eq!(cadence(&[at(1_000)]), Err(Refusal::NothingObserved));
    }

    #[test]
    fn a_refusal_explains_itself_in_the_emitted_block() {
        let block = alive_block(&steady(6, 60));

        assert!(block.contains("no [job.alive] block derived"), "{block}");
        assert!(block.contains("only 5 intervals"), "{block}");
        assert!(!block.contains("expect_every"), "{block}");
    }

    #[test]
    fn a_job_that_never_reported_work_says_so_rather_than_deriving() {
        let block = worked_block(&[]);

        assert!(block.contains("no beat reported worked:true"), "{block}");
        assert!(block.contains("it is not"), "{block}");
    }

    // -- thresholds are never tighter than reality -------------------------

    #[test]
    fn no_derived_threshold_is_tighter_than_the_worst_gap_observed() {
        for every in [1, 7, 60, 300, 3_600] {
            let cadence = cadence(&steady(60, every)).expect("should derive");
            let derived = alive_thresholds(&cadence);

            assert!(
                derived.warn_after > cadence.worst,
                "every={every}: warn {:?} vs worst {:?}",
                derived.warn_after,
                cadence.worst
            );
            assert!(derived.critical_after > derived.warn_after);
            assert!(
                derived.warn_after > derived.expect_every,
                "every={every}: a warn inside the beat interval would fire constantly"
            );
        }
    }

    #[test]
    fn irregular_signals_get_a_margin_rather_than_a_multiple() {
        // A nightly job: worst observed gap 25h. "One run late" and "two runs
        // late", not four times the usual wait.
        let worst = Duration::from_secs(25 * 3_600);
        let derived = silence_thresholds(worst);

        assert!(derived.warn_after > worst, "never tighter than reality");
        assert!(
            derived.warn_after < worst * 2,
            "a warning at two missed runs is too late to be a warning"
        );
        assert!(derived.critical_after >= worst * 2, "roughly two runs late");
        assert!(derived.critical_after > derived.warn_after);
    }

    /// Regression, found by running it at a one-second cadence: rounding
    /// truncated the sub-second part, so a 1.25s threshold came out as 1s —
    /// exactly the worst gap observed, and therefore certain to fire on the
    /// next ordinary slow beat.
    #[test]
    fn a_margin_survives_rounding_even_at_a_one_second_cadence() {
        for worst_secs in [1, 2, 3, 5] {
            let worst = Duration::from_secs(worst_secs);
            let derived = silence_thresholds(worst);

            assert!(
                derived.warn_after > worst,
                "worst {worst_secs}s: warn {:?} left no margin",
                derived.warn_after
            );
            assert!(
                derived.critical_after > derived.warn_after,
                "worst {worst_secs}s"
            );
        }
    }

    // -- rounding ----------------------------------------------------------

    #[test]
    fn durations_round_to_something_a_person_would_have_typed() {
        assert_eq!(
            round_nearest(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
        assert_eq!(
            round_nearest(Duration::from_secs(61)),
            Duration::from_secs(60)
        );
        assert_eq!(round_up(Duration::from_secs(284)), Duration::from_secs(300));
        assert_eq!(round_up(Duration::from_secs(301)), Duration::from_secs(330));
    }

    #[test]
    fn rounding_a_threshold_only_ever_widens_it() {
        for secs in [1, 3, 59, 61, 299, 301, 3_599, 90_000] {
            let d = Duration::from_secs(secs);
            assert!(round_up(d) >= d, "{secs}s rounded down");
        }
    }
}

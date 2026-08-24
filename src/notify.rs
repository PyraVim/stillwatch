//! Notification: rendering an assessment into something worth reading, and
//! getting it somewhere.
//!
//! `Notifier` is a trait so that adding a webhook or Discord later is a new
//! implementation and nothing else — the evaluator never learns where alerts go.
//!
//! Three rules govern the text, and they are the reason this module exists at
//! all rather than the evaluator formatting its own strings:
//!
//! 1. lead with the subject and what happened, never a check id
//! 2. say what is *not* wrong — ruling things out is half the value of being
//!    woken up
//! 3. end with the implication in plain language; never a bare "check failed"

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::config::TelegramConfig;
use crate::evaluate::{
    Assessment, Baseline, Condition, FileAbsence, LastSeen, ProcessAbsence, Reason, Severity,
    Trigger, WatchedFile,
};
use crate::fmt;
use crate::incidents::WATCHING_INTERVAL;

/// What a notification is about, from the reader's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Warn,
    Critical,
    Recovered,
}

impl Level {
    fn icon(self) -> &'static str {
        match self {
            Level::Warn => "⚠️",
            Level::Critical => "🔴",
            Level::Recovered => "✅",
        }
    }
}

impl From<Severity> for Level {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Warn => Level::Warn,
            Severity::Critical => Level::Critical,
        }
    }
}

/// A rendered message, ready to send anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub subject: String,
    pub level: Level,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// The notifier could not be reached at all.
    ///
    /// The URL is stripped from the underlying error before it gets here: for
    /// Telegram the bot token is *in* the URL, and a transport error that logs
    /// its own URL would put the token in the log file.
    #[error("could not reach {channel}")]
    Transport {
        channel: &'static str,
        #[source]
        source: reqwest::Error,
    },

    #[error("{channel} rejected the message with {status}: {detail}")]
    Rejected {
        channel: &'static str,
        status: u16,
        detail: String,
    },
}

impl NotifyError {
    /// Whether retrying this message could ever succeed.
    ///
    /// A wrong chat id or a malformed request will be refused identically
    /// forever, and retrying it blocks every alert queued behind it. A timeout,
    /// a 5xx or a rate limit will not.
    fn is_permanent(&self) -> bool {
        match self {
            NotifyError::Transport { .. } => false,
            NotifyError::Rejected { status, .. } => (400..500).contains(status) && *status != 429,
        }
    }
}

/// Somewhere an alert can go.
#[async_trait]
pub trait Notifier: Send + Sync {
    async fn send(&self, notification: &Notification) -> Result<(), NotifyError>;

    /// Used in logs when delivery fails.
    fn channel(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

/// Renders an assessment into the message a person will read.
pub fn render(assessment: &Assessment, now: SystemTime) -> Notification {
    let level = Level::from(assessment.severity);
    let subject = assessment.subject.clone();

    let text = match &assessment.reason {
        Reason::NoHeartbeat {
            silent_for,
            since,
            expect_every,
        } => render_no_heartbeat(&subject, level, *silent_for, since, *expect_every, now),

        Reason::NoWork {
            silent_for,
            since,
            warn_after,
            alive_and_quiet,
        } => render_no_work(
            &subject,
            level,
            *silent_for,
            since,
            *warn_after,
            *alive_and_quiet,
            now,
        ),

        Reason::Stale {
            stale_by,
            data_ts,
            warn_after,
        } => render_stale(&subject, level, *stale_by, *data_ts, *warn_after, now),

        Reason::FreshnessNeverReported {
            watching_since,
            waiting_for,
            beats,
        } => {
            render_freshness_unreported(&subject, level, *watching_since, *waiting_for, *beats, now)
        }

        Reason::RatioBelowMin { .. } => render_ratio(&subject, level, &assessment.reason),

        Reason::RatioCounterNeverReported {
            rule,
            counter,
            beats,
            waiting_for,
        } => render_ratio_unreported(&subject, level, rule, counter, *beats, *waiting_for),

        Reason::ProcessAbsent {
            absent_for,
            since,
            pidfile,
            detail,
        } => render_process_absent(&subject, level, *absent_for, since, pidfile, detail, now),

        Reason::FileStale {
            what,
            path,
            stale_for,
            since,
            stale_after,
            rotations,
        } => render_file_stale(
            &subject,
            level,
            *what,
            path,
            *stale_for,
            since,
            *stale_after,
            *rotations,
            now,
        ),

        Reason::FileMissing {
            what,
            path,
            missing_for,
            detail,
        } => render_file_missing(&subject, level, *what, path, *missing_for, detail),

        Reason::ArtifactTooSmall {
            path,
            size,
            min_bytes,
            modified,
        } => render_artifact_too_small(&subject, level, path, *size, *min_bytes, *modified, now),

        Reason::LoggedError { path, at, line } => {
            render_logged_error(&subject, level, path, *at, line, now)
        }

        Reason::CheckDown {
            failing_for,
            failed_probes,
            last_error,
        } => render_check_down(&subject, level, *failing_for, *failed_probes, last_error),

        Reason::Degraded {
            recent_p90,
            recent_window,
            recent_samples,
            baseline,
            baseline_window,
            absolute_ceiling,
            trigger,
        } => render_degraded(
            &subject,
            level,
            *recent_p90,
            *recent_window,
            *recent_samples,
            baseline,
            *baseline_window,
            *absolute_ceiling,
            trigger,
        ),

        Reason::BaselineNotCredible {
            baseline_p90,
            baseline_samples,
            baseline_window,
            absolute_ceiling,
        } => render_baseline_not_credible(
            &subject,
            level,
            *baseline_p90,
            *baseline_samples,
            *baseline_window,
            *absolute_ceiling,
        ),
    };

    Notification {
        subject,
        level,
        text,
    }
}

fn render_no_heartbeat(
    subject: &str,
    level: Level,
    silent_for: Duration,
    since: &LastSeen,
    expect_every: Duration,
    now: SystemTime,
) -> String {
    let every = fmt::duration(expect_every);

    match since {
        LastSeen::Observed(last) => format!(
            "{icon}  {subject} — no heartbeat for {silent}\n\
             \x20   last beat {last}, expected every {every}\n\
             \x20   stillwatch has been up for the whole gap — this is the job, not the watchdog\n\
             \x20   → the loop has stopped; the process has most likely exited or wedged",
            icon = level.icon(),
            silent = fmt::duration(silent_for),
            last = fmt::timestamp(*last, now),
        ),
        // Never claim a last-beat time that was never observed. "Nothing has
        // ever arrived" is a different fact from "it stopped arriving", and it
        // points at a different cause.
        LastSeen::WatchdogStart(started) => format!(
            "{icon}  {subject} — no heartbeat since stillwatch started, {silent} ago\n\
             \x20   watching since {started}, expected every {every}; nothing has ever arrived\n\
             \x20   → either the job was already stopped when the watch began, or it has \
             never been wired up to send beats",
            icon = level.icon(),
            silent = fmt::duration(silent_for),
            started = fmt::timestamp(*started, now),
        ),
    }
}

fn render_no_work(
    subject: &str,
    level: Level,
    silent_for: Duration,
    since: &LastSeen,
    warn_after: Duration,
    alive_and_quiet: bool,
    now: SystemTime,
) -> String {
    let expected = fmt::duration(warn_after);

    // Ruling the loop in or out is the whole difference between "the schedule
    // never fired" and "it is running and accomplishing nothing".
    let context = if alive_and_quiet {
        "still beating · the loop is running, it just has not reported any work"
    } else {
        "no liveness rule on this job, so nothing is vouching for the loop either"
    };

    match since {
        LastSeen::Observed(last) => {
            let implication = if alive_and_quiet {
                "→ this is the idle-dead case: up, looping, and producing nothing"
            } else {
                "→ the schedule did not fire, or it fired and died before doing anything"
            };

            format!(
                "{icon}  {subject} — no work in {silent}\n\
                 \x20   last work {last}, expected at least every {expected}\n\
                 \x20   {context}\n\
                 \x20   {implication}",
                icon = level.icon(),
                silent = fmt::duration(silent_for),
                last = fmt::timestamp(*last, now),
            )
        }

        // Never claim a last-work time that was never observed. A job that has
        // been beating since the watch began and has *still* never reported
        // work is the sharpest form of idle-dead, so saying "it has not run"
        // here would be flatly wrong — it plainly has.
        LastSeen::WatchdogStart(started) => {
            let implication = if alive_and_quiet {
                "→ it has been looping since the watch began without once reporting work: \
                 idle-dead, or never wired up to report worked:true"
            } else {
                "→ either it has not run since the watch began, or it has never been \
                 wired up to report worked:true"
            };

            format!(
                "{icon}  {subject} — no work since stillwatch started, {silent} ago\n\
                 \x20   watching since {started}, expected work at least every {expected}; \
                 none has ever been reported\n\
                 \x20   {context}\n\
                 \x20   {implication}",
                icon = level.icon(),
                silent = fmt::duration(silent_for),
                started = fmt::timestamp(*started, now),
            )
        }
    }
}

fn render_stale(
    subject: &str,
    level: Level,
    stale_by: Duration,
    data_ts: SystemTime,
    warn_after: Duration,
    now: SystemTime,
) -> String {
    format!(
        "{icon}  {subject} — acting on data {stale} old\n\
         \x20   data timestamped {stamped}, expected fresher than {expected}\n\
         \x20   the job itself is reporting in · this is the source, not the job\n\
         \x20   → whatever it produced since then was computed from numbers this old",
        icon = level.icon(),
        stale = fmt::duration(stale_by),
        stamped = fmt::timestamp(data_ts, now),
        expected = fmt::duration(warn_after),
    )
}

fn render_freshness_unreported(
    subject: &str,
    level: Level,
    watching_since: SystemTime,
    waiting_for: Duration,
    beats: u64,
    now: SystemTime,
) -> String {
    format!(
        "{icon}  {subject} — freshness is configured but nothing is feeding it\n\
         \x20   {beats} beats since {since} ({waited}), not one carrying data_ts\n\
         \x20   nothing is stale · there is no data age to be stale, which is the problem\n\
         \x20   → this job is not being watched for stale data at all; either send \
         data_ts with the beat or drop the [job.freshness] block",
        icon = level.icon(),
        since = fmt::timestamp(watching_since, now),
        waited = fmt::duration(waiting_for),
    )
}

/// The spec's headline ratio alert: what the rate is, the raw counts behind it,
/// what is demonstrably fine, and what the operator asked to be told.
fn render_ratio(subject: &str, level: Level, reason: &Reason) -> String {
    let Reason::RatioBelowMin {
        rule,
        numerator,
        denominator,
        numerator_total,
        denominator_total,
        ratio,
        min,
        window,
        message,
    } = reason
    else {
        return String::new();
    };

    // The operator's own words when they wrote them, because they know what the
    // counters mean and this tool deliberately does not.
    let implication = match message {
        Some(message) => format!("→ {message}"),
        None => format!(
            "→ {denominator} is happening and {numerator} is not keeping up; the step \
             between them is where to look"
        ),
    };

    format!(
        "{icon}  {subject} — {rule} {actual} (min {floor})\n\
         \x20   last {window}: {denominator_total_fmt} {denominator}, \
         {numerator_total_fmt} {numerator}\n\
         \x20   beats arriving ✓ · the loop is running, the work is not landing\n\
         \x20   {implication}",
        icon = level.icon(),
        actual = fmt::percent(*ratio),
        floor = fmt::percent(*min),
        window = fmt::duration(*window),
        denominator_total_fmt = fmt::count(*denominator_total),
        numerator_total_fmt = fmt::count(*numerator_total),
    )
}

fn render_ratio_unreported(
    subject: &str,
    level: Level,
    rule: &str,
    counter: &str,
    beats: u64,
    waiting_for: Duration,
) -> String {
    format!(
        "{icon}  {subject} — {rule} is configured against a counter that never arrives\n\
         \x20   {beats} beats in {waited}, not one carrying {counter:?}\n\
         \x20   nothing has failed this rule · it has never been able to run\n\
         \x20   → check the counter name against what the job actually sends; as it \
         stands this rule can never fire",
        icon = level.icon(),
        waited = fmt::duration(waiting_for),
    )
}

/// Carried inline on every passive alert.
///
/// Not just in the README, because nobody reads the README at three in the
/// morning. A pidfile says a process id exists; a log says bytes were written;
/// an artifact says a file has a size. None of them says the job did its work,
/// and somebody who has only ever seen these alerts will otherwise trust them
/// exactly as much as a heartbeat.
const WEAKER: &str = "watched from outside · a weaker signal than a heartbeat";

fn render_process_absent(
    subject: &str,
    level: Level,
    absent_for: Duration,
    since: &LastSeen,
    pidfile: &Path,
    detail: &ProcessAbsence,
    now: SystemTime,
) -> String {
    let pidfile = pidfile.display();

    let (headline, evidence, implication) = match detail {
        ProcessAbsence::ProcessGone { pid } => (
            format!("{subject} — process {pid} is gone"),
            format!("{pidfile} still names pid {pid}, but no such process exists"),
            "→ it exited without cleaning up its pidfile, which usually means it did \
             not exit on purpose",
        ),
        ProcessAbsence::PidfileMissing {
            existed_before: true,
        } => (
            format!("{subject} — the pidfile has gone"),
            format!("{pidfile} was there and is not any more"),
            "→ the process shut down, or something cleaned up after it",
        ),
        // Never seen at all: a wrong path and a job that never started look
        // identical from here, so the alert says both rather than picking one.
        ProcessAbsence::PidfileMissing {
            existed_before: false,
        } => (
            format!("{subject} — no pidfile has ever appeared"),
            format!("nothing at {pidfile} since the watch began"),
            "→ either the job has not started since stillwatch did, or the pidfile \
             path in the config is wrong",
        ),
        ProcessAbsence::PidfileUnreadable(why) => (
            format!("{subject} — the pidfile cannot be read"),
            format!("{pidfile}: {why}"),
            "→ this is stillwatch's problem, not the job's: fix the permissions or the \
             path and the real signal comes back",
        ),
    };

    let measured = match since {
        LastSeen::Observed(last) => format!(
            "last seen running {}, {} ago",
            fmt::timestamp(*last, now),
            fmt::duration(absent_for)
        ),
        LastSeen::WatchdogStart(started) => format!(
            "never seen running since the watch began at {}, {} ago",
            fmt::timestamp(*started, now),
            fmt::duration(absent_for)
        ),
    };

    format!(
        "{icon}  {headline}\n\
         \x20   {evidence}\n\
         \x20   {measured}\n\
         \x20   {WEAKER}\n\
         \x20   {implication}",
        icon = level.icon(),
    )
}

#[allow(clippy::too_many_arguments)]
fn render_file_stale(
    subject: &str,
    level: Level,
    what: WatchedFile,
    path: &Path,
    stale_for: Duration,
    since: &LastSeen,
    stale_after: Duration,
    rotations: u64,
    now: SystemTime,
) -> String {
    let last_change = match since {
        LastSeen::Observed(last) => fmt::timestamp(*last, now),
        LastSeen::WatchdogStart(started) => fmt::timestamp(*started, now),
    };

    // A rotation count worth mentioning explains what the tail did and did not
    // get to read.
    let rotation_note = match rotations {
        0 => String::new(),
        1 => String::from("\n\x20   the file was rotated once while being watched"),
        n => format!("\n\x20   the file was rotated {n} times while being watched"),
    };

    let implication = match what {
        WatchedFile::Log => {
            "→ the job has written nothing for that long; it is wedged, idle, or logging \
             somewhere else"
        }
        WatchedFile::Artifact => {
            "→ nothing new has been produced; the run did not happen, or happened and \
             wrote nowhere"
        }
    };

    format!(
        "{icon}  {subject} — the {noun} has not changed in {stale}\n\
         \x20   {path}, last written {last_change}, expected within {expected}{rotation_note}\n\
         \x20   {WEAKER}\n\
         \x20   {implication}",
        icon = level.icon(),
        noun = what.noun(),
        stale = fmt::duration(stale_for),
        path = path.display(),
        expected = fmt::duration(stale_after),
    )
}

fn render_file_missing(
    subject: &str,
    level: Level,
    what: WatchedFile,
    path: &Path,
    missing_for: Duration,
    detail: &FileAbsence,
) -> String {
    let noun = what.noun();
    let path = path.display();

    // A file that has never existed and one that vanished send you to different
    // places, so they never share a message.
    let (headline, evidence, implication) = match detail {
        FileAbsence::NotThere {
            existed_before: true,
        } => (
            format!("{subject} — the {noun} has disappeared"),
            format!("{path} was there and is not any more"),
            "→ something deleted or moved it; whatever reads it downstream is now \
             reading nothing",
        ),
        FileAbsence::NotThere {
            existed_before: false,
        } => (
            format!("{subject} — the {noun} has never appeared"),
            format!(
                "nothing at {path} in the {} since the watch began",
                fmt::duration(missing_for)
            ),
            "→ either it has never been produced, or the path in the config is wrong; \
             until one of those is true this rule is watching nothing",
        ),
        FileAbsence::Unreadable(why) => (
            format!("{subject} — the {noun} cannot be read"),
            format!("{path}: {why}"),
            "→ this is stillwatch's problem, not the job's: fix the permissions and the \
             real signal comes back",
        ),
    };

    format!(
        "{icon}  {headline}\n\
         \x20   {evidence}\n\
         \x20   {WEAKER}\n\
         \x20   {implication}",
        icon = level.icon(),
    )
}

fn render_artifact_too_small(
    subject: &str,
    level: Level,
    path: &Path,
    size: u64,
    min_bytes: u64,
    modified: Option<SystemTime>,
    now: SystemTime,
) -> String {
    let written = match modified {
        Some(modified) => format!("written {}", fmt::timestamp(modified, now)),
        None => String::from("modification time unavailable"),
    };

    format!(
        "{icon}  {subject} — the output is being produced but is nearly empty\n\
         \x20   {path} is {size} bytes, {written}; anything real is at least {min_bytes}\n\
         \x20   the file is fresh · this is not a stale artifact, it is an empty one\n\
         \x20   {WEAKER}\n\
         \x20   → the run completed and wrote almost nothing, which is the failure that \
         exits zero",
        icon = level.icon(),
        path = path.display(),
        size = fmt::count(size as f64),
        min_bytes = fmt::count(min_bytes as f64),
    )
}

fn render_logged_error(
    subject: &str,
    level: Level,
    path: &Path,
    at: SystemTime,
    line: &str,
    now: SystemTime,
) -> String {
    // The job's own words. Truncated only so one enormous line cannot fill the
    // message.
    let line = truncate(line, 300);

    format!(
        "{icon}  {subject} — its log is reporting an error\n\
         \x20   {path} at {when}:\n\
         \x20     {line}\n\
         \x20   the job said this about itself · a stronger signal than the others here\n\
         \x20   → whatever this says went wrong, went wrong; stillwatch is only \
         repeating it",
        icon = level.icon(),
        path = path.display(),
        when = fmt::timestamp(at, now),
    )
}

fn truncate(line: &str, limit: usize) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(limit).collect();
    format!("{kept}…")
}

fn render_check_down(
    subject: &str,
    level: Level,
    failing_for: Duration,
    failed_probes: usize,
    last_error: &str,
) -> String {
    format!(
        "{icon}  {subject} — down for {failing}\n\
         \x20   {failed_probes} probes in a row failed; the last said: {last_error}\n\
         \x20   → whatever depends on this is not getting answers, not getting slow answers",
        icon = level.icon(),
        failing = fmt::duration(failing_for),
    )
}

#[allow(clippy::too_many_arguments)]
fn render_degraded(
    subject: &str,
    level: Level,
    recent_p90: Duration,
    recent_window: Duration,
    recent_samples: usize,
    baseline: &Baseline,
    baseline_window: Duration,
    absolute_ceiling: Duration,
    trigger: &Trigger,
) -> String {
    let current = format!(
        "p90 {p90} over the last {window} ({recent_samples} probes)",
        p90 = fmt::latency(recent_p90),
        window = fmt::duration(recent_window),
    );

    // What it is being compared against, and how much that comparison is worth.
    let comparison = match baseline {
        Baseline::Ready { p90, samples } => format!(
            "baseline {baseline} over the {window} before that ({samples} probes)",
            baseline = fmt::latency(*p90),
            window = fmt::duration(baseline_window),
        ),
        Baseline::NotCredible { p90, samples } => format!(
            "baseline {baseline} over {samples} probes — but that is itself past the \
             {ceiling} ceiling, so the baseline has learned that slow is normal and \
             the multiples cannot fire",
            baseline = fmt::latency(*p90),
            ceiling = fmt::latency(absolute_ceiling),
        ),
        Baseline::Warming { samples, needed } => format!(
            "no baseline yet ({samples} of {needed} probes), so this is the \
             {ceiling} ceiling alone",
            ceiling = fmt::latency(absolute_ceiling),
        ),
    };

    let implication = match trigger {
        Trigger::Ceiling => format!(
            "→ past the {ceiling} you said was unacceptable; everything downstream is \
             waiting that long",
            ceiling = fmt::latency(absolute_ceiling),
        ),
        Trigger::Baseline { ratio } | Trigger::Both { ratio } => format!(
            "→ {ratio:.1}x its own normal; everything downstream is that much slower and \
             nothing else would have said so"
        ),
    };

    format!(
        "{icon}  {subject} — degraded\n\
         \x20   {current}, {comparison}\n\
         \x20   still responding · this is latency, not an outage\n\
         \x20   {implication}",
        icon = level.icon(),
    )
}

fn render_baseline_not_credible(
    subject: &str,
    level: Level,
    baseline_p90: Duration,
    baseline_samples: usize,
    baseline_window: Duration,
    absolute_ceiling: Duration,
) -> String {
    format!(
        "{icon}  {subject} — responding, but its baseline cannot be trusted\n\
         \x20   baseline p90 {baseline} over {samples} probes in the last {window}, \
         which is past the {ceiling} ceiling\n\
         \x20   nothing is failing · latency is under the ceiling right now\n\
         \x20   → this check learned its normal during a slow stretch, so a real \
         slowdown would not trip the multiples; widen the window, lower the ceiling, \
         or restart once the dependency is behaving",
        icon = level.icon(),
        baseline = fmt::latency(baseline_p90),
        samples = baseline_samples,
        window = fmt::duration(baseline_window),
        ceiling = fmt::latency(absolute_ceiling),
    )
}

/// Renders the all-clear for an incident that has ended.
///
/// Every alert gets one. An alert with no resolution teaches people to ignore
/// alerts.
pub fn render_recovery(subject: &str, headline: &str, lasted: Duration) -> Notification {
    Notification {
        subject: subject.to_string(),
        level: Level::Recovered,
        text: format!(
            "{icon}  {subject} recovered — {headline} for {lasted}",
            icon = Level::Recovered.icon(),
            lasted = fmt::duration(lasted),
        ),
    }
}

// ---------------------------------------------------------------------------
// dispatch: dedup, escalation, recovery, retry
// ---------------------------------------------------------------------------

/// How long to wait before the first delivery retry. Doubles from here.
const BASE_RETRY: Duration = Duration::from_secs(5);

/// Retries never back off further than this — a notifier that comes back after
/// an hour should not leave alerts sitting for another hour.
const MAX_RETRY: Duration = Duration::from_secs(300);

/// Dedup bounds the outbox to a few entries per subject in normal operation.
/// This cap only matters if a subject flaps for a long time while the notifier
/// is unreachable, and exists so that months of uptime cannot turn into
/// unbounded memory.
const MAX_OUTBOX: usize = 512;

/// What one open incident is keyed by: the subject, and what is wrong with it.
///
/// Both halves are needed. A job can be missing its heartbeat *and* have a
/// collapsed parse rate at the same time, and those are two things a person
/// needs told, not one.
type IncidentKey = (String, Condition);

/// A stable, greppable name for a condition, for the audit trail.
///
/// Deliberately not the `Debug` spelling: this ends up in a file people write
/// scripts against, and it should not shift because a variant was renamed.
fn condition_name(condition: &Condition) -> String {
    match condition {
        Condition::NoHeartbeat => "no-heartbeat".to_string(),
        Condition::NoWork => "no-work".to_string(),
        Condition::Stale => "stale-data".to_string(),
        Condition::FreshnessUnreported => "freshness-unreported".to_string(),
        Condition::Ratio(rule) => format!("ratio:{rule}"),
        Condition::RatioUnreported(rule) => format!("ratio-unreported:{rule}"),
        Condition::ProcessAbsent => "process-absent".to_string(),
        Condition::FileStale(what) => format!("{}-stale", what.noun()),
        Condition::FileMissing(what) => format!("{}-missing", what.noun()),
        Condition::ArtifactTooSmall => "output-too-small".to_string(),
        Condition::LoggedError => "logged-error".to_string(),
        Condition::Down => "down".to_string(),
        Condition::Degraded => "degraded".to_string(),
        Condition::UntrustworthyBaseline => "untrustworthy-baseline".to_string(),
    }
}

/// An incident that has been reported and has not yet cleared.
#[derive(Debug)]
struct Open {
    /// The worst severity reported so far. Escalation is one-way: once someone
    /// has been told it is critical, dropping back to warn is not news.
    severity: Severity,

    /// When the condition actually began — not when it was confirmed — so the
    /// all-clear reports the whole outage rather than the part of it that
    /// happened after the confirmation window elapsed.
    opened_at: SystemTime,

    headline: String,

    /// When the condition stopped being reported. `None` while it persists.
    ///
    /// Recovery is damped as well as firing. Something that clears for four
    /// seconds and comes back has not recovered, and an all-clear followed by a
    /// fresh alert is flapping in the other direction.
    clearing_since: Option<SystemTime>,
}

/// A condition seen but not yet confirmed.
///
/// The confirmation window gates the *first* firing of a condition and nothing
/// else. Once a condition is real and has been reported, an escalation is news
/// about something already established, so it goes out immediately — damping it
/// would add latency to the worse half of the story for no benefit.
#[derive(Debug)]
struct Pending {
    severity: Severity,
    first_seen: SystemTime,

    /// The most recent assessment, so that when it does fire the alert
    /// describes the condition now rather than when the window opened.
    latest: Assessment,
}

/// Turns a stream of per-cycle assessments into the alerts a person actually
/// receives.
///
/// This is where monitoring tools usually fail, so it is deliberately the
/// fussiest part of the codebase:
///
/// * **deduplicated** — one alert per incident, not one per evaluation cycle
/// * **escalating** — warn, then critical, then nothing; it does not nag
/// * **recovering** — every alert gets an all-clear with the duration
/// * **never silently dropped** — undelivered alerts queue in order and retry
///   with backoff
pub struct Dispatcher {
    notifier: Arc<dyn Notifier>,

    /// How long a condition must hold before it is reported at all.
    confirm_after: Duration,

    open: BTreeMap<IncidentKey, Open>,
    pending: BTreeMap<IncidentKey, Pending>,

    /// The audit trail, when one is configured.
    log: Option<crate::incidents::Log>,

    /// When it was last recorded that stillwatch is still watching.
    last_watching: Option<SystemTime>,

    /// Rendered but undelivered, oldest first. Order is preserved so that a
    /// recovery can never overtake the alert it resolves.
    outbox: VecDeque<Notification>,

    consecutive_failures: u32,
    retry_at: Option<SystemTime>,
    dropped: u64,
}

impl Dispatcher {
    /// A dispatcher that reports a condition the moment it is seen.
    pub fn new(notifier: Arc<dyn Notifier>) -> Self {
        Self::with_confirmation(notifier, Duration::ZERO)
    }

    /// A dispatcher that waits for a condition to hold before reporting it.
    ///
    /// The daemon always uses this one; the undamped constructor exists for
    /// tests that are about something else.
    pub fn with_confirmation(notifier: Arc<dyn Notifier>, confirm_after: Duration) -> Self {
        Self {
            notifier,
            confirm_after,
            open: BTreeMap::new(),
            pending: BTreeMap::new(),
            log: None,
            last_watching: None,
            outbox: VecDeque::new(),
            consecutive_failures: 0,
            retry_at: None,
            dropped: 0,
        }
    }

    /// Conditions seen but not yet held long enough to report.
    pub fn unconfirmed(&self) -> usize {
        self.pending.len()
    }

    /// Records every incident to the audit trail as well as sending it.
    ///
    /// Written at the same moment the incident is confirmed or resolved, not
    /// when the message is delivered: the log is a record of what happened, and
    /// what happened is not contingent on Telegram being reachable.
    pub fn recording_to(mut self, log: crate::incidents::Log) -> Self {
        self.log = Some(log);
        self
    }

    fn record(&mut self, event: crate::incidents::Event) {
        if let Some(log) = &mut self.log {
            log.append(&event);
        }
    }

    /// Records that stillwatch is still watching, if enough time has passed.
    ///
    /// Called every evaluation cycle; writes at most one record per interval.
    /// Without these, a log ending in a `started` cannot distinguish a running
    /// watchdog from one that died silently, and `report` would have to guess.
    pub fn note_still_watching(&mut self, now: SystemTime) {
        if self.log.is_none() {
            return;
        }

        let due = self
            .last_watching
            .is_none_or(|last| now.duration_since(last).unwrap_or_default() >= WATCHING_INTERVAL);

        if due {
            self.last_watching = Some(now);
            self.record(crate::incidents::watching(now));
        }
    }

    /// Takes one full evaluation cycle and sends whatever is genuinely new.
    ///
    /// `assessments` must be everything currently wrong; a subject that is open
    /// and absent from this slice is treated as recovered.
    pub async fn dispatch(&mut self, assessments: &[Assessment], now: SystemTime) {
        self.reconcile(assessments, now);
        self.flush(now).await;
    }

    /// Number of alerts rendered but not yet delivered.
    pub fn undelivered(&self) -> usize {
        self.outbox.len()
    }

    /// Number of incidents currently open.
    pub fn open_incidents(&self) -> usize {
        self.open.len()
    }

    /// Decides what is new. Pure with respect to the network — it only queues.
    fn reconcile(&mut self, assessments: &[Assessment], now: SystemTime) {
        let mut queued = Vec::new();
        let mut escalations = Vec::new();

        let still_wrong: BTreeSet<IncidentKey> = assessments
            .iter()
            .map(|a| (a.subject.clone(), a.reason.condition()))
            .collect();

        // A job whose heartbeat has stopped has its other rules suppressed by
        // the evaluator, because a dead loop explains all of them. That absence
        // must not be read as recovery: a collapsed parse rate on a job that
        // has since died did not get better, and saying it did would be exactly
        // the confidently-wrong message this whole module exists to avoid. Hold
        // those incidents open, silently, until the job is judgeable again.
        let unjudgeable: BTreeSet<&str> = assessments
            .iter()
            .filter(|a| a.reason.condition() == Condition::NoHeartbeat)
            .map(|a| a.subject.as_str())
            .collect();

        // -- conditions currently being reported ---------------------------
        for assessment in assessments {
            let key = (assessment.subject.clone(), assessment.reason.condition());

            if let Some(open) = self.open.get_mut(&key) {
                // Still here, so any recovery timer that had started is void.
                open.clearing_since = None;

                if assessment.severity > open.severity {
                    // Escalation goes out at once. The confirmation window is
                    // about whether a condition is real, and this one has
                    // already been established as real.
                    open.severity = assessment.severity;
                    let opened_at = open.opened_at;
                    queued.push(render(assessment, now));
                    escalations.push((key, assessment.severity, opened_at));
                }
                continue;
            }

            let pending = self.pending.entry(key.clone()).or_insert_with(|| Pending {
                severity: assessment.severity,
                first_seen: now,
                latest: assessment.clone(),
            });
            pending.severity = pending.severity.max(assessment.severity);
            pending.latest = assessment.clone();

            let held_for = now
                .duration_since(pending.first_seen)
                .unwrap_or(Duration::ZERO);
            if held_for < self.confirm_after {
                continue;
            }

            // Confirmed. The incident is dated from when the condition began,
            // not from now, so its eventual duration covers the whole thing.
            let pending = self.pending.remove(&key).expect("just looked it up");
            let notification = render(&pending.latest, now);

            self.record(crate::incidents::opened(
                &key.0,
                &condition_name(&key.1),
                pending.severity,
                pending.first_seen,
                &notification.text,
            ));

            queued.push(notification);
            self.open.insert(
                key,
                Open {
                    severity: pending.severity,
                    opened_at: pending.first_seen,
                    headline: pending.latest.reason.headline(),
                    clearing_since: None,
                },
            );
        }

        // Something that went away before it was ever confirmed is exactly what
        // the window is for. Nothing was sent, so nothing needs retracting.
        self.pending.retain(|key, _| still_wrong.contains(key));

        // -- conditions that have stopped being reported --------------------
        let mut recovered = Vec::new();
        for (key, open) in self.open.iter_mut() {
            if still_wrong.contains(key) || unjudgeable.contains(key.0.as_str()) {
                continue;
            }

            let clearing_since = *open.clearing_since.get_or_insert(now);
            let clear_for = now.duration_since(clearing_since).unwrap_or(Duration::ZERO);
            if clear_for >= self.confirm_after {
                recovered.push(key.clone());
            }
        }

        for key in recovered {
            if let Some(open) = self.open.remove(&key) {
                // The incident ended when the condition stopped, not when the
                // confirmation window for the recovery finished elapsing.
                let ended_at = open.clearing_since.unwrap_or(now);
                let lasted = ended_at
                    .duration_since(open.opened_at)
                    .unwrap_or(Duration::ZERO);

                self.record(crate::incidents::resolved(
                    &key.0,
                    &condition_name(&key.1),
                    open.opened_at,
                    ended_at,
                ));

                queued.push(render_recovery(&key.0, &open.headline, lasted));
            }
        }

        for (key, severity, opened_at) in escalations {
            self.record(crate::incidents::escalated(
                &key.0,
                &condition_name(&key.1),
                severity,
                opened_at,
                now,
            ));
        }

        for notification in queued {
            self.enqueue(notification);
        }
    }

    fn enqueue(&mut self, notification: Notification) {
        if self.outbox.len() >= MAX_OUTBOX {
            if let Some(lost) = self.outbox.pop_front() {
                self.dropped += 1;
                tracing::error!(
                    subject = %lost.subject,
                    dropped_total = self.dropped,
                    "outbox is full; dropping the oldest undelivered alert"
                );
            }
        }
        self.outbox.push_back(notification);
    }

    /// Delivers what is queued, in order, stopping at the first failure that is
    /// worth retrying.
    async fn flush(&mut self, now: SystemTime) {
        if self.retry_at.is_some_and(|retry_at| now < retry_at) {
            return;
        }

        while let Some(notification) = self.outbox.front().cloned() {
            match self.notifier.send(&notification).await {
                Ok(()) => {
                    self.outbox.pop_front();
                    self.consecutive_failures = 0;
                    self.retry_at = None;
                    tracing::info!(
                        subject = %notification.subject,
                        level = ?notification.level,
                        "alert sent"
                    );
                }

                Err(err) if err.is_permanent() => {
                    // Retrying forever would block every alert behind it, so
                    // this one goes — loudly, and with the reason attached.
                    self.outbox.pop_front();
                    self.dropped += 1;
                    tracing::error!(
                        subject = %notification.subject,
                        error = %err,
                        "dropping an alert the notifier will never accept; fix the config"
                    );
                }

                Err(err) => {
                    self.consecutive_failures += 1;
                    let delay = backoff(self.consecutive_failures);
                    self.retry_at = Some(now + delay);
                    tracing::error!(
                        subject = %notification.subject,
                        error = %err,
                        queued = self.outbox.len(),
                        retry_in = %fmt::duration(delay),
                        "could not deliver an alert; it stays queued"
                    );
                    return;
                }
            }
        }
    }
}

fn backoff(consecutive_failures: u32) -> Duration {
    let doublings = consecutive_failures.saturating_sub(1).min(8);
    (BASE_RETRY * (1u32 << doublings)).min(MAX_RETRY)
}

// ---------------------------------------------------------------------------
// telegram
// ---------------------------------------------------------------------------

pub struct Telegram {
    client: reqwest::Client,
    token: String,
    chat_id: String,
}

impl Telegram {
    pub fn new(config: &TelegramConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            token: config.token.clone(),
            chat_id: config.chat_id.clone(),
        }
    }
}

/// Deliberately omits `Debug` on the token by not deriving `Debug` at all.
impl std::fmt::Debug for Telegram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telegram")
            .field("chat_id", &self.chat_id)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Notifier for Telegram {
    async fn send(&self, notification: &Notification) -> Result<(), NotifyError> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);

        // No `parse_mode`. Job names contain underscores and the alert body
        // contains `—`, `·` and `→`; asking Telegram to parse it as Markdown
        // turns an ordinary job name into a delivery failure.
        let response = self
            .client
            .post(url)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": notification.text,
            }))
            .send()
            .await
            .map_err(|source| NotifyError::Transport {
                channel: self.channel(),
                // The bot token is in the URL.
                source: source.without_url(),
            })?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let detail = response
            .text()
            .await
            .unwrap_or_else(|_| "<no response body>".to_string());

        Err(NotifyError::Rejected {
            channel: self.channel(),
            status: status.as_u16(),
            detail,
        })
    }

    fn channel(&self) -> &'static str {
        "telegram"
    }
}

// ---------------------------------------------------------------------------
// no notifier configured
// ---------------------------------------------------------------------------

/// Stands in for the real notifier under `--dry-run`.
///
/// Deliberately sits in the same place as a real notifier, *behind* the
/// dispatcher, so what it prints has already been through deduplication,
/// escalation and recovery. A dry run that logged every evaluation cycle would
/// read as far noisier than the daemon actually is, and nobody would trust it
/// enough to switch the real thing on — which is the entire purpose of having
/// a dry run.
#[derive(Debug, Default)]
pub struct DryRun;

#[async_trait]
impl Notifier for DryRun {
    async fn send(&self, notification: &Notification) -> Result<(), NotifyError> {
        tracing::info!(
            subject = %notification.subject,
            level = ?notification.level,
            "would have sent:\n{}",
            notification.text
        );
        Ok(())
    }

    fn channel(&self) -> &'static str {
        "dry-run"
    }
}

/// Used when the config names no notifier.
///
/// Writing alerts to the log is not a substitute for delivering them, and
/// startup says so out loud — but it is much better than a watchdog that
/// silently evaluates into the void.
#[derive(Debug, Default)]
pub struct LogOnly;

#[async_trait]
impl Notifier for LogOnly {
    async fn send(&self, notification: &Notification) -> Result<(), NotifyError> {
        tracing::warn!(
            subject = %notification.subject,
            "no notifier configured, logging instead:\n{}",
            notification.text
        );
        Ok(())
    }

    fn channel(&self) -> &'static str {
        "log"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn no_heartbeat(subject: &str, severity: Severity, since: LastSeen, silent: u64) -> Assessment {
        Assessment {
            subject: subject.into(),
            severity,
            reason: Reason::NoHeartbeat {
                silent_for: Duration::from_secs(silent),
                since,
                expect_every: Duration::from_secs(60),
            },
        }
    }

    #[test]
    fn a_missed_beat_reads_as_subject_evidence_and_implication() {
        let now = at(1_755_000_000);
        let notification = render(
            &no_heartbeat(
                "product-scraper",
                Severity::Warn,
                LastSeen::Observed(now - Duration::from_secs(312)),
                312,
            ),
            now,
        );

        let lines: Vec<_> = notification.text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(
            lines[0].starts_with("⚠️  product-scraper — no heartbeat for 5m12s"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("last beat"), "{}", lines[1]);
        assert!(lines[1].contains("expected every 1m"), "{}", lines[1]);
        assert!(
            lines[2].contains("not the watchdog"),
            "an alert should rule something out: {}",
            lines[2]
        );
        assert!(lines[3].trim_start().starts_with('→'), "{}", lines[3]);
    }

    #[test]
    fn a_critical_uses_the_critical_icon() {
        let now = at(1_755_000_000);
        let notification = render(
            &no_heartbeat(
                "clients-etl",
                Severity::Critical,
                LastSeen::Observed(now - Duration::from_secs(900)),
                900,
            ),
            now,
        );

        assert_eq!(notification.level, Level::Critical);
        assert!(notification.text.starts_with("🔴  clients-etl"));
    }

    /// The dead-on-arrival wording must not imply a beat that was never seen.
    #[test]
    fn a_never_seen_job_is_not_described_as_having_a_last_beat() {
        let now = at(1_755_000_000);
        let notification = render(
            &no_heartbeat(
                "clients-etl",
                Severity::Critical,
                LastSeen::WatchdogStart(now - Duration::from_secs(903)),
                903,
            ),
            now,
        );

        assert!(
            !notification.text.contains("last beat"),
            "must not invent a last beat: {}",
            notification.text
        );
        assert!(notification.text.contains("since stillwatch started"));
        assert!(notification.text.contains("nothing has ever arrived"));
        assert!(notification.text.contains("15m3s"));
    }

    #[test]
    fn recovery_names_what_ended_and_how_long_it_lasted() {
        let notification = render_recovery(
            "product-scraper",
            "no heartbeat",
            Duration::from_secs(1_084),
        );

        assert_eq!(notification.level, Level::Recovered);
        assert_eq!(
            notification.text,
            "✅  product-scraper recovered — no heartbeat for 18m4s"
        );
    }

    /// The alert body is sent to Telegram as plain text, so nothing in it needs
    /// escaping — but it must also not accidentally look like a bare status dump.
    #[test]
    fn every_alert_ends_with_an_implication() {
        let now = at(1_755_000_000);
        for since in [
            LastSeen::Observed(now - Duration::from_secs(400)),
            LastSeen::WatchdogStart(now - Duration::from_secs(400)),
        ] {
            let notification = render(
                &no_heartbeat("nightly-sync", Severity::Warn, since, 400),
                now,
            );
            let last = notification.text.lines().last().unwrap_or_default();
            assert!(last.trim_start().starts_with('→'), "{last}");
        }
    }

    // -- check alerts ------------------------------------------------------

    fn degraded(
        severity: Severity,
        recent_ms: u64,
        baseline: Baseline,
        trigger: Trigger,
    ) -> Assessment {
        Assessment {
            subject: "vendor-api".into(),
            severity,
            reason: Reason::Degraded {
                recent_p90: Duration::from_millis(recent_ms),
                recent_window: Duration::from_secs(600),
                recent_samples: 20,
                baseline,
                baseline_window: Duration::from_secs(3_600),
                absolute_ceiling: Duration::from_secs(2),
                trigger,
            },
        }
    }

    #[test]
    fn a_degradation_reads_as_current_baseline_and_implication() {
        let now = at(1_755_000_000);
        let notification = render(
            &degraded(
                Severity::Critical,
                1_400,
                Baseline::Ready {
                    p90: Duration::from_millis(140),
                    samples: 118,
                },
                Trigger::Baseline { ratio: 10.0 },
            ),
            now,
        );

        let lines: Vec<_> = notification.text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(
            lines[0].starts_with("🔴  vendor-api — degraded"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("p90 1.4s"), "{}", lines[1]);
        assert!(lines[1].contains("baseline 140ms"), "{}", lines[1]);
        assert!(
            lines[2].contains("still responding"),
            "an alert should rule something out: {}",
            lines[2]
        );
        assert!(lines[3].contains("10.0x its own normal"), "{}", lines[3]);
    }

    /// The alert must never quote a comforting ratio off a baseline that has
    /// learned an unacceptable normal.
    #[test]
    fn a_degradation_on_a_poisoned_baseline_says_the_baseline_is_worthless() {
        let now = at(1_755_000_000);
        let notification = render(
            &degraded(
                Severity::Warn,
                3_000,
                Baseline::NotCredible {
                    p90: Duration::from_millis(3_000),
                    samples: 118,
                },
                Trigger::Ceiling,
            ),
            now,
        );

        assert!(
            notification.text.contains("learned that slow is normal"),
            "{}",
            notification.text
        );
        assert!(
            notification.text.contains("multiples cannot fire"),
            "{}",
            notification.text
        );
    }

    /// During warmup the alert has to admit it has no baseline rather than
    /// implying the ceiling breach was measured against one.
    #[test]
    fn a_degradation_during_warmup_says_there_is_no_baseline_yet() {
        let now = at(1_755_000_000);
        let notification = render(
            &degraded(
                Severity::Warn,
                3_000,
                Baseline::Warming {
                    samples: 4,
                    needed: 30,
                },
                Trigger::Ceiling,
            ),
            now,
        );

        assert!(
            notification
                .text
                .contains("no baseline yet (4 of 30 probes)"),
            "{}",
            notification.text
        );
        assert!(
            notification.text.contains("ceiling alone"),
            "{}",
            notification.text
        );
    }

    #[test]
    fn an_untrustworthy_baseline_alert_says_what_to_do_about_it() {
        let now = at(1_755_000_000);
        let notification = render(
            &Assessment {
                subject: "vendor-api".into(),
                severity: Severity::Warn,
                reason: Reason::BaselineNotCredible {
                    baseline_p90: Duration::from_millis(2_500),
                    baseline_samples: 118,
                    baseline_window: Duration::from_secs(3_600),
                    absolute_ceiling: Duration::from_secs(2),
                },
            },
            now,
        );

        assert!(
            notification.text.contains("cannot be trusted"),
            "{}",
            notification.text
        );
        assert!(
            notification.text.contains("nothing is failing"),
            "it must rule out an actual outage: {}",
            notification.text
        );
        assert!(
            notification.text.contains("would not trip the multiples"),
            "{}",
            notification.text
        );
    }

    #[test]
    fn a_down_check_names_the_error_it_last_saw() {
        let now = at(1_755_000_000);
        let notification = render(
            &Assessment {
                subject: "queue-broker".into(),
                severity: Severity::Critical,
                reason: Reason::CheckDown {
                    failing_for: Duration::from_secs(124),
                    failed_probes: 4,
                    last_error: "connection refused".into(),
                },
            },
            now,
        );

        assert!(
            notification
                .text
                .starts_with("🔴  queue-broker — down for 2m4s"),
            "{}",
            notification.text
        );
        assert!(
            notification.text.contains("connection refused"),
            "{}",
            notification.text
        );
        assert!(
            notification.text.contains("not getting slow answers"),
            "{}",
            notification.text
        );
    }

    /// Every alert this tool can produce ends with a plain-language implication.
    #[test]
    fn every_check_alert_ends_with_an_implication() {
        let now = at(1_755_000_000);
        let assessments = [
            degraded(
                Severity::Warn,
                1_400,
                Baseline::Ready {
                    p90: Duration::from_millis(140),
                    samples: 118,
                },
                Trigger::Both { ratio: 10.0 },
            ),
            Assessment {
                subject: "vendor-api".into(),
                severity: Severity::Warn,
                reason: Reason::BaselineNotCredible {
                    baseline_p90: Duration::from_millis(2_500),
                    baseline_samples: 118,
                    baseline_window: Duration::from_secs(3_600),
                    absolute_ceiling: Duration::from_secs(2),
                },
            },
            Assessment {
                subject: "queue-broker".into(),
                severity: Severity::Critical,
                reason: Reason::CheckDown {
                    failing_for: Duration::from_secs(124),
                    failed_probes: 4,
                    last_error: "connection refused".into(),
                },
            },
        ];

        for assessment in assessments {
            let text = render(&assessment, now).text;
            let last = text.lines().last().unwrap_or_default();
            assert!(last.trim_start().starts_with('→'), "{text}");
        }
    }

    /// The all-clear reuses the headline that opened the incident, so a
    /// degradation resolves as "degraded for ..." and not as something else.
    #[test]
    fn check_recoveries_reuse_the_reason_headline() {
        let degraded = degraded(
            Severity::Warn,
            1_400,
            Baseline::Ready {
                p90: Duration::from_millis(140),
                samples: 118,
            },
            Trigger::Ceiling,
        );

        let notification = render_recovery(
            "vendor-api",
            &degraded.reason.headline(),
            Duration::from_secs(1_084),
        );

        assert_eq!(
            notification.text,
            "✅  vendor-api recovered — degraded for 18m4s"
        );
    }

    // -- dispatch ----------------------------------------------------------

    /// A notifier that records what it was given and can be told to reject
    /// everything with a chosen status until it is repaired.
    #[derive(Default)]
    struct Fake {
        sent: std::sync::Mutex<Vec<Notification>>,
        rejecting_with: std::sync::Mutex<Option<u16>>,
        attempts: std::sync::atomic::AtomicUsize,
    }

    impl Fake {
        fn shared() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn break_with(&self, status: u16) {
            *self.rejecting_with.lock().expect("lock") = Some(status);
        }

        fn repair(&self) {
            *self.rejecting_with.lock().expect("lock") = None;
        }

        fn delivered(&self) -> Vec<Notification> {
            self.sent.lock().expect("lock").clone()
        }

        fn levels(&self) -> Vec<Level> {
            self.delivered().iter().map(|n| n.level).collect()
        }

        fn attempts(&self) -> usize {
            self.attempts.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Notifier for Fake {
        async fn send(&self, notification: &Notification) -> Result<(), NotifyError> {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

            // Read the guard out in its own statement: an `if let` scrutinee
            // holds its temporaries for the whole block in edition 2021, which
            // would deadlock against the second lock below.
            let rejecting_with = *self.rejecting_with.lock().expect("lock");
            if let Some(status) = rejecting_with {
                return Err(NotifyError::Rejected {
                    channel: self.channel(),
                    status,
                    detail: "injected by the test".into(),
                });
            }

            self.sent.lock().expect("lock").push(notification.clone());
            Ok(())
        }

        fn channel(&self) -> &'static str {
            "fake"
        }
    }

    fn silent(subject: &str, severity: Severity, silent_secs: u64) -> Assessment {
        no_heartbeat(
            subject,
            severity,
            LastSeen::Observed(at(1_000) - Duration::from_secs(silent_secs)),
            silent_secs,
        )
    }

    #[tokio::test]
    async fn the_same_condition_alerts_once_not_once_per_cycle() {
        let fake = Fake::shared();
        let mut dispatcher = Dispatcher::new(fake.clone());
        let warn = [silent("product-scraper", Severity::Warn, 312)];

        for tick in 0..20 {
            dispatcher.dispatch(&warn, at(2_000 + tick * 5)).await;
        }

        assert_eq!(fake.delivered().len(), 1, "one alert per incident");
        assert_eq!(fake.levels(), [Level::Warn]);
    }

    #[tokio::test]
    async fn warn_escalates_to_critical_exactly_once() {
        let fake = Fake::shared();
        let mut dispatcher = Dispatcher::new(fake.clone());

        for tick in 0..5 {
            dispatcher
                .dispatch(
                    &[silent("product-scraper", Severity::Warn, 312)],
                    at(2_000 + tick),
                )
                .await;
        }
        for tick in 0..5 {
            dispatcher
                .dispatch(
                    &[silent("product-scraper", Severity::Critical, 950)],
                    at(2_100 + tick),
                )
                .await;
        }

        assert_eq!(fake.levels(), [Level::Warn, Level::Critical]);
    }

    /// Warn, then critical, then nothing. It does not nag, and it does not
    /// walk back down either.
    #[tokio::test]
    async fn dropping_back_to_warn_is_not_a_new_alert() {
        let fake = Fake::shared();
        let mut dispatcher = Dispatcher::new(fake.clone());

        dispatcher
            .dispatch(&[silent("clients-etl", Severity::Critical, 950)], at(2_000))
            .await;
        dispatcher
            .dispatch(&[silent("clients-etl", Severity::Warn, 312)], at(2_005))
            .await;

        assert_eq!(fake.levels(), [Level::Critical]);
    }

    #[tokio::test]
    async fn every_alert_gets_an_all_clear_with_the_full_incident_duration() {
        let fake = Fake::shared();
        let mut dispatcher = Dispatcher::new(fake.clone());

        dispatcher
            .dispatch(&[silent("product-scraper", Severity::Warn, 312)], at(2_000))
            .await;
        dispatcher
            .dispatch(
                &[silent("product-scraper", Severity::Critical, 950)],
                at(2_500),
            )
            .await;
        dispatcher.dispatch(&[], at(2_000 + 1_084)).await;

        assert_eq!(
            fake.levels(),
            [Level::Warn, Level::Critical, Level::Recovered]
        );
        // Measured from the first alert, not the escalation.
        assert_eq!(
            fake.delivered()[2].text,
            "✅  product-scraper recovered — no heartbeat for 18m4s"
        );
        assert_eq!(dispatcher.open_incidents(), 0);
    }

    #[tokio::test]
    async fn a_fresh_incident_after_a_recovery_alerts_again() {
        let fake = Fake::shared();
        let mut dispatcher = Dispatcher::new(fake.clone());
        let warn = [silent("nightly-sync", Severity::Warn, 312)];

        dispatcher.dispatch(&warn, at(2_000)).await;
        dispatcher.dispatch(&[], at(2_100)).await;
        dispatcher.dispatch(&warn, at(2_200)).await;

        assert_eq!(
            fake.levels(),
            [Level::Warn, Level::Recovered, Level::Warn],
            "dedup must not swallow the next real incident"
        );
    }

    #[tokio::test]
    async fn nothing_wrong_sends_nothing() {
        let fake = Fake::shared();
        let mut dispatcher = Dispatcher::new(fake.clone());

        for tick in 0..10 {
            dispatcher.dispatch(&[], at(2_000 + tick)).await;
        }

        assert!(fake.delivered().is_empty());
        assert_eq!(fake.attempts(), 0);
    }

    #[tokio::test]
    async fn subjects_do_not_interfere_with_each_other() {
        let fake = Fake::shared();
        let mut dispatcher = Dispatcher::new(fake.clone());

        dispatcher
            .dispatch(
                &[
                    silent("nightly-sync", Severity::Warn, 312),
                    silent("product-scraper", Severity::Warn, 312),
                ],
                at(2_000),
            )
            .await;
        // Only the scraper is still wrong.
        dispatcher
            .dispatch(&[silent("product-scraper", Severity::Warn, 400)], at(2_100))
            .await;

        let subjects: Vec<_> = fake
            .delivered()
            .iter()
            .map(|n| (n.subject.clone(), n.level))
            .collect();

        assert_eq!(
            subjects,
            [
                ("nightly-sync".to_string(), Level::Warn),
                ("product-scraper".to_string(), Level::Warn),
                ("nightly-sync".to_string(), Level::Recovered),
            ]
        );
    }

    /// Deduplication is per condition, not per subject. One subject can have
    /// several things wrong with it at once, and the first must not silence the
    /// rest.
    #[tokio::test]
    async fn two_different_conditions_on_one_subject_both_alert() {
        let fake = Fake::shared();
        let mut dispatcher = Dispatcher::new(fake.clone());

        let both = [
            silent("vendor-api", Severity::Warn, 312),
            degraded(
                Severity::Critical,
                1_400,
                Baseline::Ready {
                    p90: Duration::from_millis(140),
                    samples: 118,
                },
                Trigger::Baseline { ratio: 10.0 },
            ),
        ];

        for tick in 0..10 {
            dispatcher.dispatch(&both, at(2_000 + tick)).await;
        }

        assert_eq!(
            fake.levels(),
            [Level::Warn, Level::Critical],
            "both conditions should be reported, each exactly once"
        );
        assert_eq!(dispatcher.open_incidents(), 2);
    }

    /// ...and each recovers on its own, without disturbing the other.
    #[tokio::test]
    async fn one_condition_recovering_leaves_the_other_open() {
        let fake = Fake::shared();
        let mut dispatcher = Dispatcher::new(fake.clone());

        let heartbeat = silent("vendor-api", Severity::Warn, 312);
        let slow = degraded(
            Severity::Critical,
            1_400,
            Baseline::Ready {
                p90: Duration::from_millis(140),
                samples: 118,
            },
            Trigger::Baseline { ratio: 10.0 },
        );

        dispatcher
            .dispatch(&[heartbeat.clone(), slow.clone()], at(2_000))
            .await;
        // The heartbeat comes back; the latency does not.
        dispatcher.dispatch(&[slow], at(2_100)).await;

        assert_eq!(
            fake.levels(),
            [Level::Warn, Level::Critical, Level::Recovered]
        );
        assert_eq!(
            fake.delivered()[2].text,
            "✅  vendor-api recovered — no heartbeat for 1m40s",
            "the all-clear must name the condition that ended, not the one still open"
        );
        assert_eq!(dispatcher.open_incidents(), 1);
    }

    /// A job that dies while one of its other rules is already failing must not
    /// be told that rule recovered. It did not; it stopped being observable.
    #[tokio::test]
    async fn a_condition_suppressed_by_a_dead_loop_is_not_reported_as_recovered() {
        let fake = Fake::shared();
        let mut dispatcher = Dispatcher::new(fake.clone());

        let bad_ratio = Assessment {
            subject: "product-scraper".into(),
            severity: Severity::Warn,
            reason: Reason::RatioBelowMin {
                rule: "parse rate".into(),
                numerator: "parsed".into(),
                denominator: "fetched".into(),
                numerator_total: 10.0,
                denominator_total: 1_200.0,
                ratio: 0.008,
                min: 0.9,
                window: Duration::from_secs(3_600),
                message: None,
            },
        };
        let no_heartbeat = silent("product-scraper", Severity::Critical, 950);

        dispatcher.dispatch(&[bad_ratio], at(2_000)).await;
        assert_eq!(fake.levels(), [Level::Warn]);

        // The loop stops. The evaluator now reports only the heartbeat.
        for tick in 0..5 {
            dispatcher
                .dispatch(std::slice::from_ref(&no_heartbeat), at(2_100 + tick))
                .await;
        }

        assert_eq!(
            fake.levels(),
            [Level::Warn, Level::Critical],
            "no all-clear for a rule that merely stopped being observable"
        );
        assert_eq!(
            dispatcher.open_incidents(),
            2,
            "the ratio incident stays open while the job is unjudgeable"
        );
    }

    /// ...and once the job is judgeable again, a rule that really did clear
    /// gets its all-clear as normal.
    #[tokio::test]
    async fn a_suppressed_condition_recovers_once_the_job_is_judgeable_again() {
        let fake = Fake::shared();
        let mut dispatcher = Dispatcher::new(fake.clone());

        let bad_ratio = Assessment {
            subject: "product-scraper".into(),
            severity: Severity::Warn,
            reason: Reason::RatioBelowMin {
                rule: "parse rate".into(),
                numerator: "parsed".into(),
                denominator: "fetched".into(),
                numerator_total: 10.0,
                denominator_total: 1_200.0,
                ratio: 0.008,
                min: 0.9,
                window: Duration::from_secs(3_600),
                message: None,
            },
        };

        dispatcher.dispatch(&[bad_ratio], at(2_000)).await;
        dispatcher
            .dispatch(
                &[silent("product-scraper", Severity::Critical, 950)],
                at(2_100),
            )
            .await;

        // The job comes back and everything is fine.
        dispatcher.dispatch(&[], at(2_200)).await;

        assert_eq!(
            fake.levels(),
            [
                Level::Warn,
                Level::Critical,
                Level::Recovered,
                Level::Recovered
            ]
        );
        assert_eq!(dispatcher.open_incidents(), 0);
    }

    // -- flap damping ------------------------------------------------------

    const CONFIRM: Duration = Duration::from_secs(30);

    fn damped(fake: Arc<Fake>) -> Dispatcher {
        Dispatcher::with_confirmation(fake, CONFIRM)
    }

    /// Something that fixes itself in four seconds is not an incident.
    #[tokio::test]
    async fn a_condition_that_clears_inside_the_window_is_never_reported() {
        let fake = Fake::shared();
        let mut dispatcher = damped(fake.clone());
        let warn = [silent("product-scraper", Severity::Warn, 312)];

        dispatcher.dispatch(&warn, at(2_000)).await;
        dispatcher.dispatch(&warn, at(2_004)).await;
        assert_eq!(dispatcher.unconfirmed(), 1);
        assert!(fake.delivered().is_empty());

        // Gone before it was ever confirmed.
        dispatcher.dispatch(&[], at(2_008)).await;

        assert!(
            fake.delivered().is_empty(),
            "nothing was sent, so there is nothing to retract"
        );
        assert_eq!(dispatcher.unconfirmed(), 0);
        assert_eq!(dispatcher.open_incidents(), 0);
    }

    /// A genuine outage must not be damped into invisibility — only delayed by
    /// the window, and then dated from when it actually began.
    #[tokio::test]
    async fn a_condition_that_holds_is_reported_and_dated_from_when_it_began() {
        let fake = Fake::shared();
        let mut dispatcher = damped(fake.clone());
        let warn = [silent("product-scraper", Severity::Warn, 312)];

        for tick in 0..6 {
            dispatcher.dispatch(&warn, at(2_000 + tick * 5)).await;
        }
        assert!(fake.delivered().is_empty(), "still inside the window");

        dispatcher.dispatch(&warn, at(2_030)).await;
        assert_eq!(fake.levels(), [Level::Warn], "confirmed and reported");

        // The incident is dated from 2_000, not from when it was confirmed.
        dispatcher.dispatch(&[], at(2_100)).await;
        dispatcher.dispatch(&[], at(2_130)).await;

        let recovery = fake.delivered().pop().expect("a recovery");
        assert_eq!(
            recovery.text, "✅  product-scraper recovered — no heartbeat for 1m40s",
            "the duration must cover the confirmation window too"
        );
    }

    /// The window gates the first firing of a condition, not escalation. Once
    /// something is established as real, worse news goes out at once.
    #[tokio::test]
    async fn escalation_is_immediate_once_a_condition_has_been_confirmed() {
        let fake = Fake::shared();
        let mut dispatcher = damped(fake.clone());

        for tick in 0..8 {
            dispatcher
                .dispatch(
                    &[silent("product-scraper", Severity::Warn, 312)],
                    at(2_000 + tick * 5),
                )
                .await;
        }
        assert_eq!(fake.levels(), [Level::Warn]);

        // It gets worse on the very next cycle.
        dispatcher
            .dispatch(
                &[silent("product-scraper", Severity::Critical, 950)],
                at(2_045),
            )
            .await;

        assert_eq!(
            fake.levels(),
            [Level::Warn, Level::Critical],
            "a confirmed condition getting worse must not wait out another window"
        );
    }

    /// Recovery is damped too. Clearing for four seconds and coming back is not
    /// a recovery, and an all-clear followed by a fresh alert is flapping in the
    /// other direction.
    #[tokio::test]
    async fn a_condition_that_blinks_clear_and_returns_sends_no_all_clear() {
        let fake = Fake::shared();
        let mut dispatcher = damped(fake.clone());
        let warn = [silent("product-scraper", Severity::Warn, 312)];

        for tick in 0..8 {
            dispatcher.dispatch(&warn, at(2_000 + tick * 5)).await;
        }
        assert_eq!(fake.levels(), [Level::Warn]);

        // Clear for one cycle, then back.
        dispatcher.dispatch(&[], at(2_045)).await;
        dispatcher.dispatch(&warn, at(2_050)).await;
        dispatcher.dispatch(&warn, at(2_100)).await;

        assert_eq!(
            fake.levels(),
            [Level::Warn],
            "it never really recovered, so nothing more should have been said"
        );
        assert_eq!(dispatcher.open_incidents(), 1);
    }

    #[tokio::test]
    async fn a_condition_that_stays_clear_recovers_after_the_window() {
        let fake = Fake::shared();
        let mut dispatcher = damped(fake.clone());
        let warn = [silent("product-scraper", Severity::Warn, 312)];

        for tick in 0..8 {
            dispatcher.dispatch(&warn, at(2_000 + tick * 5)).await;
        }

        dispatcher.dispatch(&[], at(2_045)).await;
        assert_eq!(fake.levels(), [Level::Warn], "still inside the window");

        dispatcher.dispatch(&[], at(2_080)).await;
        assert_eq!(fake.levels(), [Level::Warn, Level::Recovered]);
    }

    /// Severity seen while waiting is not lost: if it is already critical by the
    /// time the window elapses, the first message says so.
    #[tokio::test]
    async fn a_condition_that_worsens_before_confirming_fires_at_its_worst() {
        let fake = Fake::shared();
        let mut dispatcher = damped(fake.clone());

        dispatcher
            .dispatch(&[silent("product-scraper", Severity::Warn, 312)], at(2_000))
            .await;
        dispatcher
            .dispatch(
                &[silent("product-scraper", Severity::Critical, 950)],
                at(2_010),
            )
            .await;
        dispatcher
            .dispatch(
                &[silent("product-scraper", Severity::Critical, 950)],
                at(2_040),
            )
            .await;

        assert_eq!(
            fake.levels(),
            [Level::Critical],
            "one message, at the severity it had reached"
        );
    }

    #[tokio::test]
    async fn damping_is_per_condition_not_per_subject() {
        let fake = Fake::shared();
        let mut dispatcher = damped(fake.clone());

        let heartbeat = silent("vendor-api", Severity::Warn, 312);
        let slow = degraded(
            Severity::Warn,
            1_400,
            Baseline::Ready {
                p90: Duration::from_millis(140),
                samples: 118,
            },
            Trigger::Ceiling,
        );

        // The heartbeat holds; the latency blinks.
        dispatcher
            .dispatch(&[heartbeat.clone(), slow], at(2_000))
            .await;
        dispatcher
            .dispatch(std::slice::from_ref(&heartbeat), at(2_040))
            .await;

        assert_eq!(
            fake.levels(),
            [Level::Warn],
            "only the condition that held should have fired"
        );
        assert_eq!(dispatcher.open_incidents(), 1);
    }

    // -- delivery failures -------------------------------------------------

    #[tokio::test]
    async fn an_unreachable_notifier_queues_and_loses_nothing() {
        let fake = Fake::shared();
        fake.break_with(503);
        let mut dispatcher = Dispatcher::new(fake.clone());
        let warn = [silent("product-scraper", Severity::Warn, 312)];

        dispatcher.dispatch(&warn, at(2_000)).await;
        assert!(fake.delivered().is_empty());
        assert_eq!(dispatcher.undelivered(), 1);

        // Backoff holds it briefly, then it retries and gets through.
        fake.repair();
        dispatcher.dispatch(&warn, at(2_001)).await;
        assert!(
            fake.delivered().is_empty(),
            "backoff should still be holding"
        );

        dispatcher.dispatch(&warn, at(2_010)).await;

        assert_eq!(fake.levels(), [Level::Warn], "the alert must not be lost");
        assert_eq!(dispatcher.undelivered(), 0);
    }

    #[tokio::test]
    async fn a_retry_delivers_exactly_one_copy() {
        let fake = Fake::shared();
        fake.break_with(500);
        let mut dispatcher = Dispatcher::new(fake.clone());
        let warn = [silent("clients-etl", Severity::Warn, 312)];

        for tick in 0..30 {
            if tick == 15 {
                fake.repair();
            }
            dispatcher.dispatch(&warn, at(2_000 + tick * 10)).await;
        }

        assert_eq!(fake.delivered().len(), 1, "no duplicate on retry");
        assert_eq!(dispatcher.undelivered(), 0);
    }

    #[tokio::test]
    async fn backoff_grows_rather_than_hammering_every_cycle() {
        let fake = Fake::shared();
        fake.break_with(503);
        let mut dispatcher = Dispatcher::new(fake.clone());
        let warn = [silent("product-scraper", Severity::Warn, 312)];

        // One evaluation cycle a second for a minute against a dead notifier.
        for tick in 0..60 {
            dispatcher.dispatch(&warn, at(2_000 + tick)).await;
        }

        assert!(
            fake.attempts() < 10,
            "expected backoff to throttle retries, saw {} attempts",
            fake.attempts()
        );
        assert_eq!(dispatcher.undelivered(), 1);
    }

    /// A wrong chat id fails the same way forever. Retrying it would block
    /// every later alert behind it, so it is dropped — loudly, not silently.
    #[tokio::test]
    async fn a_permanently_rejected_alert_does_not_block_the_queue() {
        let fake = Fake::shared();
        fake.break_with(400);
        let mut dispatcher = Dispatcher::new(fake.clone());

        dispatcher
            .dispatch(&[silent("product-scraper", Severity::Warn, 312)], at(2_000))
            .await;

        assert!(fake.delivered().is_empty());
        assert_eq!(
            dispatcher.undelivered(),
            0,
            "a 400 will be refused identically forever; it must not sit in the \
             queue holding up everything behind it"
        );

        fake.repair();
        dispatcher.dispatch(&[], at(2_010)).await;

        assert_eq!(
            fake.levels(),
            [Level::Recovered],
            "later alerts must still get through"
        );
    }

    #[tokio::test]
    async fn rate_limiting_is_retried_rather_than_dropped() {
        let fake = Fake::shared();
        fake.break_with(429);
        let mut dispatcher = Dispatcher::new(fake.clone());
        let warn = [silent("product-scraper", Severity::Warn, 312)];

        dispatcher.dispatch(&warn, at(2_000)).await;
        assert_eq!(dispatcher.undelivered(), 1);

        fake.repair();
        dispatcher.dispatch(&warn, at(2_060)).await;

        assert_eq!(fake.levels(), [Level::Warn]);
    }

    #[test]
    fn backoff_doubles_and_then_stops() {
        assert_eq!(backoff(1), Duration::from_secs(5));
        assert_eq!(backoff(2), Duration::from_secs(10));
        assert_eq!(backoff(3), Duration::from_secs(20));
        assert_eq!(backoff(50), MAX_RETRY);
    }

    #[test]
    fn a_telegram_notifier_does_not_print_its_token() {
        let telegram = Telegram::new(&TelegramConfig {
            token: "12345:super-secret".into(),
            chat_id: "-100777".into(),
        });

        let rendered = format!("{telegram:?}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }
}

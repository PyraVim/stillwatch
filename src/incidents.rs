//! The audit trail: one JSON object per line, appended, never rewritten.
//!
//! No database. A watchdog that needs its own datastore is a second thing that
//! can fail, and the thing it is most likely to fail at is starting up at three
//! in the morning after the disk filled.
//!
//! Two kinds of record go in here. Incidents, which are the point. And
//! stillwatch's own start and stop, which are what let `report` say *"I was not
//! watching for these two hours"* instead of quietly claiming a hundred percent
//! uptime for a window it slept through. A monitoring tool that overstates its
//! own coverage is committing the exact failure it exists to catch.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::evaluate::Severity;
use crate::notify::Notification;

/// How often stillwatch records that it is still watching.
///
/// Short enough that a crash is noticed within one interval, cheap enough to
/// ignore: about eleven kilobytes a day against a default eight megabyte cap.
pub const WATCHING_INTERVAL: Duration = Duration::from_secs(300);

/// What a single line in the log says happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// stillwatch began watching.
    Started { ts: i64, version: String },

    /// stillwatch stopped watching, on purpose.
    ///
    /// Its *absence* before the next `started` is the interesting case: it
    /// means the process went away without saying so, and `report` cannot know
    /// what happened in between.
    Stopped { ts: i64 },

    /// stillwatch was still watching at this moment.
    ///
    /// Written periodically for one reason: without it, a log ending in a
    /// `started` is ambiguous between "still running" and "died silently", and
    /// the only way to resolve it would be to assume. Assuming it is still
    /// running claims coverage for time nobody watched — which is exactly the
    /// overstatement this file exists to prevent.
    Watching { ts: i64 },

    Opened {
        ts: i64,
        subject: String,
        condition: String,
        severity: String,
        reason: String,
    },

    /// An open incident got worse.
    ///
    /// Recorded separately rather than by rewriting the `opened` line, because
    /// this file is append-only. Without it an incident that opened as a
    /// warning and became critical would be filed forever as a warning, which
    /// is the sort of quiet inaccuracy an audit trail exists to prevent.
    /// `report` does not count these as separate incidents.
    Escalated {
        ts: i64,
        subject: String,
        condition: String,
        severity: String,
        opened_ts: i64,
    },

    Resolved {
        ts: i64,
        subject: String,
        condition: String,
        /// When the incident began, so an open and its resolution can be paired.
        opened_ts: i64,
        duration_secs: u64,
    },
}

impl Event {
    pub fn ts(&self) -> i64 {
        match self {
            Event::Started { ts, .. }
            | Event::Stopped { ts }
            | Event::Watching { ts }
            | Event::Opened { ts, .. }
            | Event::Escalated { ts, .. }
            | Event::Resolved { ts, .. } => *ts,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IncidentError {
    #[error("could not open the incident log at {path}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not read the incident log at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Appends to the incident log, rotating it when it gets large.
#[derive(Debug)]
pub struct Log {
    path: PathBuf,
    max_bytes: u64,
    file: File,
    written: u64,
}

impl Log {
    /// Opens the log for appending, creating it if needed.
    ///
    /// Fails loudly rather than degrading. A watchdog running happily with no
    /// audit trail is a rule that cannot fire, pointed at itself: everything
    /// looks fine, and the record that would have proved otherwise was never
    /// written.
    pub fn open(path: &Path, max_bytes: u64) -> Result<Self, IncidentError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| IncidentError::Open {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|source| IncidentError::Open {
                path: path.to_path_buf(),
                source,
            })?;

        let written = file.metadata().map(|m| m.len()).unwrap_or(0);

        Ok(Self {
            path: path.to_path_buf(),
            max_bytes,
            file,
            written,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one event.
    ///
    /// Failures are logged rather than propagated: losing an audit line is bad,
    /// but stopping the watchdog because a disk filled would be worse, and the
    /// alerts themselves do not depend on this file.
    pub fn append(&mut self, event: &Event) {
        let Ok(mut line) = serde_json::to_string(event) else {
            tracing::error!(?event, "could not serialise an incident record");
            return;
        };
        line.push('\n');

        if let Err(err) = self.file.write_all(line.as_bytes()) {
            tracing::error!(path = %self.path.display(), %err, "could not write to the incident log");
            return;
        }
        self.written += line.len() as u64;

        if self.written >= self.max_bytes {
            self.rotate();
        }
    }

    /// Moves the log aside and starts a fresh one.
    ///
    /// Exactly one generation is kept, so the total on disk is bounded at twice
    /// `max_bytes` and cannot creep. Retention here is by size and not by time:
    /// a very busy month may push older incidents out before `report --since`
    /// would have reached them, and the README says so rather than leaving
    /// somebody to discover it from a gap.
    fn rotate(&mut self) {
        let previous = self.path.with_extension("jsonl.1");

        if let Err(err) = std::fs::rename(&self.path, &previous) {
            tracing::error!(
                path = %self.path.display(),
                %err,
                "could not rotate the incident log; it will go on growing"
            );
            return;
        }

        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.file = file;
                self.written = 0;
                tracing::info!(
                    path = %self.path.display(),
                    previous = %previous.display(),
                    "incident log rotated"
                );
            }
            Err(err) => {
                tracing::error!(
                    path = %self.path.display(),
                    %err,
                    "rotated the incident log but could not reopen it; nothing more \
                     will be recorded"
                );
            }
        }
    }
}

/// Turns a delivered notification into the record of it, if it is one.
pub fn opened(
    subject: &str,
    condition: &str,
    severity: Severity,
    at: SystemTime,
    reason: &str,
) -> Event {
    Event::Opened {
        ts: unix(at),
        subject: subject.to_string(),
        condition: condition.to_string(),
        severity: severity_name(severity),
        reason: first_line(reason),
    }
}

pub fn resolved(subject: &str, condition: &str, opened_at: SystemTime, at: SystemTime) -> Event {
    Event::Resolved {
        ts: unix(at),
        subject: subject.to_string(),
        condition: condition.to_string(),
        opened_ts: unix(opened_at),
        duration_secs: at.duration_since(opened_at).unwrap_or_default().as_secs(),
    }
}

pub fn started(at: SystemTime) -> Event {
    Event::Started {
        ts: unix(at),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

pub fn stopped(at: SystemTime) -> Event {
    Event::Stopped { ts: unix(at) }
}

pub fn escalated(
    subject: &str,
    condition: &str,
    severity: Severity,
    opened_at: SystemTime,
    at: SystemTime,
) -> Event {
    Event::Escalated {
        ts: unix(at),
        subject: subject.to_string(),
        condition: condition.to_string(),
        severity: severity_name(severity),
        opened_ts: unix(opened_at),
    }
}

pub fn watching(at: SystemTime) -> Event {
    Event::Watching { ts: unix(at) }
}

fn severity_name(severity: Severity) -> String {
    match severity {
        Severity::Warn => "warn".to_string(),
        Severity::Critical => "critical".to_string(),
    }
}

/// The alert's headline, which is what a person reading the log wants.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

/// A notification's first line, for recording alongside an incident.
pub fn headline_of(notification: &Notification) -> String {
    first_line(&notification.text)
}

pub fn unix(at: SystemTime) -> i64 {
    match at.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs() as i64,
        // Before 1970. Not a real timestamp, but not worth panicking over.
        Err(err) => -(err.duration().as_secs() as i64),
    }
}

pub fn from_unix(ts: i64) -> SystemTime {
    if ts >= 0 {
        UNIX_EPOCH + Duration::from_secs(ts as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(ts.unsigned_abs())
    }
}

/// Reads every event from the log and its one rotated generation, oldest first.
///
/// A line that will not parse is skipped with a warning rather than aborting
/// the read: a half-written final line after a hard kill must not make the
/// whole history unreadable.
pub fn read_all(path: &Path) -> Result<Vec<Event>, IncidentError> {
    let mut events = Vec::new();

    // Oldest generation first, so the result is in order.
    for candidate in [path.with_extension("jsonl.1"), path.to_path_buf()] {
        let file = match File::open(&candidate) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(IncidentError::Read {
                    path: candidate,
                    source,
                })
            }
        };

        for (number, line) in BufReader::new(file).lines().enumerate() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(&line) {
                Ok(event) => events.push(event),
                Err(err) => tracing::warn!(
                    path = %candidate.display(),
                    line = number + 1,
                    %err,
                    "skipping an incident record that will not parse"
                ),
            }
        }
    }

    events.sort_by_key(Event::ts);
    Ok(events)
}

// ---------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------

/// What happened to one subject over a window.
#[derive(Debug, Clone, PartialEq)]
pub struct SubjectReport {
    pub subject: String,
    pub incidents: usize,

    /// Time inside the window during which this subject had an open incident.
    pub down: Duration,

    pub longest: Duration,

    /// `None` when nothing was watching for any of the window, so there is no
    /// honest percentage to give.
    pub uptime: Option<f64>,
}

/// What happened over a window, and how much of it stillwatch actually saw.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub since: SystemTime,
    pub until: SystemTime,

    /// How much of the window stillwatch was demonstrably watching.
    pub watched: Duration,

    /// How much of it nobody can account for.
    ///
    /// Either stillwatch was not running, or it stopped without recording the
    /// fact. Reported separately and excluded from every percentage, because
    /// both alternatives — counting it as up or counting it as down — would be
    /// the tool asserting something it does not know.
    pub unknown: Duration,

    /// True when at least one run ended without a `stopped` record.
    pub unclean_shutdowns: usize,

    pub subjects: Vec<SubjectReport>,
}

impl Report {
    pub fn window(&self) -> Duration {
        self.until
            .duration_since(self.since)
            .unwrap_or(Duration::ZERO)
    }
}

/// A stretch of time stillwatch was demonstrably running.
#[derive(Debug, Clone, Copy)]
struct Coverage {
    from: SystemTime,
    to: SystemTime,
    /// Whether the run ended with a recorded `stopped`.
    clean: bool,
}

/// Works out which stretches of time were actually being watched.
///
/// A `started` with no `stopped` before the next `started` means the process
/// went away without saying so. All that can honestly be claimed for that run
/// is up to the last thing it recorded — beyond that, nobody knows.
fn coverage(events: &[Event], until: SystemTime) -> Vec<Coverage> {
    let mut spans = Vec::new();
    let mut open: Option<SystemTime> = None;
    let mut last_seen: Option<SystemTime> = None;

    for event in events {
        let at = from_unix(event.ts());
        match event {
            Event::Started { .. } => {
                if let Some(from) = open.take() {
                    // A previous run that never said goodbye.
                    spans.push(Coverage {
                        from,
                        to: last_seen.unwrap_or(from),
                        clean: false,
                    });
                }
                open = Some(at);
                last_seen = Some(at);
            }
            Event::Stopped { .. } => {
                if let Some(from) = open.take() {
                    spans.push(Coverage {
                        from,
                        to: at,
                        clean: true,
                    });
                }
                last_seen = Some(at);
            }
            _ => last_seen = Some(at),
        }
    }

    if let Some(from) = open {
        // A log that ends in a `started` means one of two things and cannot say
        // which: still running, or gone without a word. The `watching` records
        // settle it. If one is not yet due, the run is live and covered to now;
        // if one was due and never came, coverage ends at the last thing it
        // actually said and the rest belongs to nobody.
        let last = last_seen.unwrap_or(from).max(from);
        let silent_for = until.duration_since(last).unwrap_or(Duration::ZERO);

        if silent_for <= WATCHING_INTERVAL {
            spans.push(Coverage {
                from,
                to: until.max(from),
                clean: true,
            });
        } else {
            spans.push(Coverage {
                from,
                to: last,
                clean: false,
            });
        }
    }

    spans
}

/// How much of `[since, until]` the given spans cover.
fn covered(spans: &[Coverage], since: SystemTime, until: SystemTime) -> Duration {
    spans
        .iter()
        .filter_map(|span| overlap(span.from, span.to, since, until))
        .sum()
}

fn overlap(
    from: SystemTime,
    to: SystemTime,
    since: SystemTime,
    until: SystemTime,
) -> Option<Duration> {
    let start = from.max(since);
    let end = to.min(until);
    end.duration_since(start).ok().filter(|d| !d.is_zero())
}

/// Summarises a window of history.
///
/// `until` is passed in rather than read from the clock, so this is a pure
/// function of the log and the range asked for.
pub fn report(events: &[Event], since: SystemTime, until: SystemTime) -> Report {
    let spans = coverage(events, until);
    let watched = covered(&spans, since, until);
    let window = until.duration_since(since).unwrap_or(Duration::ZERO);
    let unknown = window.saturating_sub(watched);
    let unclean_shutdowns = spans.iter().filter(|span| !span.clean).count();

    // Pair each open with its resolution. An open with no resolution is still
    // running as far as the log knows, so it counts up to the end of the window.
    let mut opens: BTreeMap<(String, String, i64), SystemTime> = BTreeMap::new();
    let mut down: BTreeMap<String, Duration> = BTreeMap::new();
    let mut longest: BTreeMap<String, Duration> = BTreeMap::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut subjects: BTreeSet<String> = BTreeSet::new();

    for event in events {
        match event {
            Event::Opened {
                ts,
                subject,
                condition,
                ..
            } => {
                subjects.insert(subject.clone());
                opens.insert((subject.clone(), condition.clone(), *ts), from_unix(*ts));
            }
            Event::Resolved {
                subject,
                condition,
                opened_ts,
                ts,
                ..
            } => {
                subjects.insert(subject.clone());
                let key = (subject.clone(), condition.clone(), *opened_ts);
                let opened_at = opens.remove(&key).unwrap_or_else(|| from_unix(*opened_ts));
                record_incident(
                    subject,
                    opened_at,
                    from_unix(*ts),
                    since,
                    until,
                    &mut down,
                    &mut longest,
                    &mut counts,
                );
            }
            _ => {}
        }
    }

    // Whatever is left open never resolved within the log.
    for ((subject, _, _), opened_at) in opens {
        record_incident(
            &subject,
            opened_at,
            until,
            since,
            until,
            &mut down,
            &mut longest,
            &mut counts,
        );
    }

    let subjects = subjects
        .into_iter()
        .map(|subject| {
            let down = down.get(&subject).copied().unwrap_or_default();
            SubjectReport {
                incidents: counts.get(&subject).copied().unwrap_or_default(),
                longest: longest.get(&subject).copied().unwrap_or_default(),
                // The percentage is of the time actually watched. Spreading a
                // known outage across hours nobody was looking at would make
                // the number up.
                uptime: (!watched.is_zero()).then(|| {
                    let up = watched.saturating_sub(down);
                    (up.as_secs_f64() / watched.as_secs_f64()).clamp(0.0, 1.0)
                }),
                down,
                subject,
            }
        })
        .collect();

    Report {
        since,
        until,
        watched,
        unknown,
        unclean_shutdowns,
        subjects,
    }
}

#[allow(clippy::too_many_arguments)]
fn record_incident(
    subject: &str,
    opened_at: SystemTime,
    resolved_at: SystemTime,
    since: SystemTime,
    until: SystemTime,
    down: &mut BTreeMap<String, Duration>,
    longest: &mut BTreeMap<String, Duration>,
    counts: &mut BTreeMap<String, usize>,
) {
    let Some(inside) = overlap(opened_at, resolved_at, since, until) else {
        return;
    };

    *down.entry(subject.to_string()).or_default() += inside;
    *counts.entry(subject.to_string()).or_default() += 1;

    let full = resolved_at
        .duration_since(opened_at)
        .unwrap_or(Duration::ZERO);
    let entry = longest.entry(subject.to_string()).or_default();
    *entry = (*entry).max(full);
}

/// Renders a report the way it is meant to be read.
pub fn render_report(report: &Report) -> String {
    let mut out = String::new();

    if report.subjects.is_empty() {
        out.push_str("nothing was recorded for this window\n");
    }

    let width = report
        .subjects
        .iter()
        .map(|s| s.subject.len())
        .max()
        .unwrap_or(0)
        .max(7);

    for subject in &report.subjects {
        let uptime = match subject.uptime {
            // Not 0% and not 100%. Nothing was watching, so there is no answer.
            None => "uptime unknown".to_string(),
            Some(fraction) => format!("uptime {:>6}", crate::fmt::percent(fraction)),
        };

        let incidents = match subject.incidents {
            0 => "no incidents".to_string(),
            1 => "1 incident".to_string(),
            n => format!("{n} incidents"),
        };

        let longest = if subject.longest.is_zero() {
            String::new()
        } else {
            format!("   longest {}", crate::fmt::span(subject.longest))
        };

        out.push_str(&format!(
            "{:<width$}   {uptime}   {incidents}{longest}\n",
            subject.subject,
            width = width
        ));
    }

    out.push('\n');
    out.push_str(&coverage_line(report));
    out
}

/// The self-awareness line: how much of the window stillwatch could see.
fn coverage_line(report: &Report) -> String {
    let window = report.window();

    if report.watched.is_zero() {
        return format!(
            "stillwatch has no record of watching any of the last {} — every number \
             above is unknown rather than good\n",
            crate::fmt::span(window),
        );
    }

    if report.unknown.is_zero() {
        return format!("watched all {} of this window\n", crate::fmt::span(window));
    }

    let cause = if report.unclean_shutdowns > 0 {
        "stillwatch was not running, or stopped without recording it"
    } else {
        "stillwatch was not running"
    };

    format!(
        "watched {} of the last {} · {} unaccounted for ({cause})\n\
         percentages above are of the watched time only\n",
        crate::fmt::span(report.watched),
        crate::fmt::span(window),
        crate::fmt::span(report.unknown),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn events_round_trip_through_the_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("incidents.jsonl");

        let mut log = Log::open(&path, 1_000_000).expect("open");
        log.append(&started(at(1_000)));
        log.append(&opened(
            "product-scraper",
            "no-heartbeat",
            Severity::Warn,
            at(1_100),
            "⚠️  product-scraper — no heartbeat for 5m12s\n    last beat ...",
        ));
        log.append(&resolved(
            "product-scraper",
            "no-heartbeat",
            at(1_100),
            at(1_400),
        ));
        log.append(&stopped(at(2_000)));
        drop(log);

        let events = read_all(&path).expect("read");
        assert_eq!(events.len(), 4);

        assert!(matches!(events[0], Event::Started { ts: 1_000, .. }));
        match &events[1] {
            Event::Opened {
                subject,
                severity,
                reason,
                ..
            } => {
                assert_eq!(subject, "product-scraper");
                assert_eq!(severity, "warn");
                assert_eq!(
                    reason, "⚠️  product-scraper — no heartbeat for 5m12s",
                    "the headline is what a person reading the log wants"
                );
            }
            other => panic!("expected an opened record, got {other:?}"),
        }
        match &events[2] {
            Event::Resolved {
                opened_ts,
                duration_secs,
                ..
            } => {
                assert_eq!(*opened_ts, 1_100, "pairs with the open");
                assert_eq!(*duration_secs, 300);
            }
            other => panic!("expected a resolved record, got {other:?}"),
        }
        assert!(matches!(events[3], Event::Stopped { ts: 2_000 }));
    }

    /// A watchdog meant to run for months must not fill a disk.
    #[test]
    fn the_log_rotates_and_keeps_exactly_one_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("incidents.jsonl");

        let mut log = Log::open(&path, 400).expect("open");
        for tick in 0..200 {
            log.append(&started(at(1_000 + tick)));
        }
        drop(log);

        let live = std::fs::metadata(&path).expect("live log").len();
        let rotated = std::fs::metadata(path.with_extension("jsonl.1"))
            .expect("one rotated generation")
            .len();

        assert!(live <= 400 + 100, "the live log stays near its cap: {live}");
        assert!(rotated <= 400 + 100, "so does the one kept behind it");

        // And exactly one generation, not a growing pile.
        let generations = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .count();
        assert_eq!(generations, 2, "one live log and one rotated");
    }

    #[test]
    fn reading_covers_both_generations_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("incidents.jsonl");

        // Sized so exactly one rotation happens, which means everything written
        // is still reachable — across both generations.
        let mut log = Log::open(&path, 1_000).expect("open");
        for tick in 0..30 {
            log.append(&started(at(1_000 + tick)));
        }
        drop(log);

        assert!(
            path.with_extension("jsonl.1").exists(),
            "the test needs a rotation to have happened"
        );

        let events = read_all(&path).expect("read");
        assert_eq!(events.len(), 30, "nothing is lost to a single rotation");

        let timestamps: Vec<i64> = events.iter().map(Event::ts).collect();
        let mut sorted = timestamps.clone();
        sorted.sort_unstable();
        assert_eq!(timestamps, sorted, "oldest first");
    }

    /// A hard kill can leave a half-written final line. That must not make the
    /// whole history unreadable.
    #[test]
    fn a_truncated_final_line_does_not_poison_the_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("incidents.jsonl");

        let mut log = Log::open(&path, 1_000_000).expect("open");
        log.append(&started(at(1_000)));
        log.append(&stopped(at(1_500)));
        drop(log);

        let mut raw = std::fs::read_to_string(&path).expect("read");
        raw.push_str("{\"event\":\"star");
        std::fs::write(&path, raw).expect("write");

        let events = read_all(&path).expect("read");
        assert_eq!(events.len(), 2, "the intact records survive");
    }

    #[test]
    fn an_unwritable_path_is_an_error_rather_than_a_shrug() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory is never openable as a file for appending.
        let err = Log::open(dir.path(), 1_000).expect_err("should fail");

        assert!(matches!(err, IncidentError::Open { .. }), "{err}");
    }

    #[test]
    fn reading_a_log_that_does_not_exist_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let events = read_all(&dir.path().join("nothing.jsonl")).expect("read");
        assert!(events.is_empty());
    }

    // -- report ------------------------------------------------------------

    const DAY: u64 = 86_400;

    fn incident(subject: &str, from: u64, to: u64) -> Vec<Event> {
        vec![
            opened(
                subject,
                "no-heartbeat",
                Severity::Warn,
                at(from),
                "⚠️  down",
            ),
            resolved(subject, "no-heartbeat", at(from), at(to)),
        ]
    }

    /// The ordinary case: watching throughout, one outage.
    #[test]
    fn a_fully_watched_window_reports_a_real_percentage() {
        let mut events = vec![started(at(0))];
        events.extend(incident("product-scraper", 1_000, 2_000));
        events.push(stopped(at(DAY)));

        let report = report(&events, at(0), at(DAY));

        assert_eq!(report.watched, Duration::from_secs(DAY));
        assert_eq!(report.unknown, Duration::ZERO);
        assert_eq!(report.subjects.len(), 1);

        let subject = &report.subjects[0];
        assert_eq!(subject.incidents, 1);
        assert_eq!(subject.down, Duration::from_secs(1_000));
        assert_eq!(subject.longest, Duration::from_secs(1_000));

        let uptime = subject.uptime.expect("a real percentage");
        assert!((uptime - (1.0 - 1_000.0 / DAY as f64)).abs() < 1e-9);

        let rendered = render_report(&report);
        assert!(rendered.contains("watched all 1d"), "{rendered}");
    }

    /// The self-awareness requirement: a window nobody was watching must not be
    /// counted as uptime.
    #[test]
    fn time_when_stillwatch_was_not_running_is_unknown_not_uptime() {
        // Ran for the first six hours, stopped, restarted for the last six.
        let events = vec![
            started(at(0)),
            stopped(at(6 * 3_600)),
            started(at(18 * 3_600)),
            stopped(at(DAY)),
        ];

        let report = report(&events, at(0), at(DAY));

        assert_eq!(report.watched, Duration::from_secs(12 * 3_600));
        assert_eq!(
            report.unknown,
            Duration::from_secs(12 * 3_600),
            "the twelve hours in between belong to nobody"
        );

        let rendered = render_report(&report);
        assert!(rendered.contains("12h unaccounted for"), "{rendered}");
        assert!(
            rendered.contains("of the watched time only"),
            "the percentages must say what they are of: {rendered}"
        );
    }

    /// An unclean shutdown leaves no `stopped`. All that can honestly be claimed
    /// is up to the last thing that run recorded.
    #[test]
    fn an_unclean_shutdown_becomes_unknown_time_and_is_said_so() {
        let mut events = vec![started(at(0))];
        events.extend(incident("product-scraper", 1_000, 2_000));
        // No `stopped` — the process was killed. Then a restart much later.
        events.push(started(at(20 * 3_600)));
        events.push(stopped(at(DAY)));

        let report = report(&events, at(0), at(DAY));

        assert_eq!(report.unclean_shutdowns, 1);
        assert_eq!(
            report.watched,
            Duration::from_secs(2_000 + 4 * 3_600),
            "coverage for the killed run ends at its last record"
        );
        assert!(report.unknown > Duration::from_secs(17 * 3_600));

        let rendered = render_report(&report);
        assert!(
            rendered.contains("stopped without recording it"),
            "an unclean shutdown must be named as a possibility: {rendered}"
        );
    }

    /// Regression, found by running it: a log ending in a `started` was assumed
    /// to mean "still running", so `report` claimed coverage for the time after
    /// the process had already died. The periodic `watching` records settle it.
    #[test]
    fn a_run_that_went_silent_stops_counting_as_covered() {
        let events = vec![
            started(at(0)),
            watching(at(300)),
            watching(at(600)),
            // Then nothing. It died somewhere after 600.
        ];

        let report = report(&events, at(0), at(DAY));

        assert_eq!(
            report.watched,
            Duration::from_secs(600),
            "coverage ends at the last thing it actually said"
        );
        assert_eq!(report.unclean_shutdowns, 1);

        let rendered = render_report(&report);
        assert!(
            rendered.contains("stopped without recording it"),
            "a silent death must be named as a possibility: {rendered}"
        );
    }

    /// ...but a daemon that is simply running right now must not show a
    /// spurious gap at the end of every report.
    #[test]
    fn a_run_that_is_still_going_is_covered_to_now() {
        let events = vec![started(at(0)), watching(at(DAY - 60))];

        let report = report(&events, at(0), at(DAY));

        assert_eq!(report.watched, Duration::from_secs(DAY));
        assert_eq!(report.unknown, Duration::ZERO);
        assert_eq!(report.unclean_shutdowns, 0);
    }

    /// Neither 0% nor 100%. Nothing was watching, so there is no answer.
    #[test]
    fn a_window_with_no_coverage_at_all_reports_unknown_rather_than_a_number() {
        // History exists, but all of it predates the window asked about.
        let events = vec![started(at(0)), stopped(at(1_000))];

        let report = report(&events, at(10 * DAY), at(11 * DAY));

        assert_eq!(report.watched, Duration::ZERO);
        assert_eq!(report.unknown, Duration::from_secs(DAY));
        assert!(report.subjects.is_empty());

        let rendered = render_report(&report);
        assert!(rendered.contains("no record of watching"), "{rendered}");
        assert!(
            rendered.contains("unknown rather than good"),
            "silence must not read as health: {rendered}"
        );
        assert!(!rendered.contains("100%"), "{rendered}");
        assert!(!rendered.contains("0%"), "{rendered}");
    }

    #[test]
    fn an_empty_log_reports_nothing_rather_than_perfect_health() {
        let report = report(&[], at(0), at(DAY));

        assert!(report.subjects.is_empty());
        assert_eq!(report.watched, Duration::ZERO);

        let rendered = render_report(&report);
        assert!(rendered.contains("nothing was recorded"), "{rendered}");
        assert!(!rendered.contains("100%"), "{rendered}");
    }

    /// A subject with incidents but no coverage cannot be given a percentage
    /// either, even though something is plainly known about it.
    #[test]
    fn a_subject_seen_only_outside_any_coverage_has_no_percentage() {
        // Incidents recorded, but no started/stopped at all.
        let events = incident("product-scraper", 1_000, 2_000);

        let report = report(&events, at(0), at(DAY));

        assert_eq!(report.subjects[0].incidents, 1);
        assert_eq!(report.subjects[0].uptime, None);

        let rendered = render_report(&report);
        assert!(rendered.contains("uptime unknown"), "{rendered}");
    }

    #[test]
    fn only_the_part_of_an_incident_inside_the_window_counts() {
        let mut events = vec![started(at(0))];
        // Straddles the start of the window asked about.
        events.extend(incident("product-scraper", 500, 1_500));
        events.push(stopped(at(DAY)));

        let report = report(&events, at(1_000), at(DAY));

        assert_eq!(
            report.subjects[0].down,
            Duration::from_secs(500),
            "only the overlap counts toward downtime"
        );
        assert_eq!(
            report.subjects[0].longest,
            Duration::from_secs(1_000),
            "but the incident's real length is what gets reported as longest"
        );
    }

    /// An incident that never resolved is still open, and counts up to now.
    #[test]
    fn an_unresolved_incident_counts_to_the_end_of_the_window() {
        let events = vec![
            started(at(0)),
            opened(
                "product-scraper",
                "no-heartbeat",
                Severity::Critical,
                at(DAY / 2),
                "🔴  down",
            ),
        ];

        let report = report(&events, at(0), at(DAY));

        assert_eq!(report.subjects[0].incidents, 1);
        assert_eq!(report.subjects[0].down, Duration::from_secs(DAY / 2));
    }

    #[test]
    fn several_subjects_are_reported_side_by_side() {
        let mut events = vec![started(at(0))];
        events.extend(incident("product-scraper", 1_000, 2_000));
        events.extend(incident("product-scraper", 5_000, 5_100));
        events.extend(incident("vendor-api", 3_000, 6_000));
        events.push(stopped(at(DAY)));

        let report = report(&events, at(0), at(DAY));
        assert_eq!(report.subjects.len(), 2);

        let scraper = &report.subjects[0];
        assert_eq!(scraper.subject, "product-scraper");
        assert_eq!(scraper.incidents, 2);
        assert_eq!(scraper.longest, Duration::from_secs(1_000));

        let vendor = &report.subjects[1];
        assert_eq!(vendor.incidents, 1);
        assert_eq!(vendor.longest, Duration::from_secs(3_000));

        let rendered = render_report(&report);
        assert!(rendered.contains("product-scraper"), "{rendered}");
        assert!(rendered.contains("vendor-api"), "{rendered}");
        assert!(rendered.contains("2 incidents"), "{rendered}");
        assert!(rendered.contains("1 incident "), "{rendered}");
    }

    #[test]
    fn a_subject_that_never_failed_reads_as_such() {
        let mut events = vec![started(at(0))];
        events.extend(incident("product-scraper", 1_000, 2_000));
        events.push(stopped(at(DAY)));

        let report = report(&events, at(0), at(DAY));
        let rendered = render_report(&report);

        assert!(rendered.contains("1 incident"), "{rendered}");
        assert!(rendered.contains("longest 16m40s"), "{rendered}");
    }

    #[test]
    fn timestamps_survive_the_round_trip() {
        for secs in [0, 1, 1_755_000_000] {
            assert_eq!(from_unix(unix(at(secs))), at(secs));
        }
    }
}

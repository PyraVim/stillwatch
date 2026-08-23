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
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::config::TelegramConfig;
use crate::evaluate::{Assessment, Baseline, Condition, LastSeen, Reason, Severity, Trigger};
use crate::fmt;

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

/// An incident that has been reported and has not yet cleared.
#[derive(Debug)]
struct Open {
    /// The worst severity reported so far. Escalation is one-way: once someone
    /// has been told it is critical, dropping back to warn is not news.
    severity: Severity,

    /// When the incident started — the *first* alert, not the escalation, so
    /// the all-clear reports the whole outage.
    opened_at: SystemTime,

    headline: String,
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
    open: BTreeMap<IncidentKey, Open>,

    /// Rendered but undelivered, oldest first. Order is preserved so that a
    /// recovery can never overtake the alert it resolves.
    outbox: VecDeque<Notification>,

    consecutive_failures: u32,
    retry_at: Option<SystemTime>,
    dropped: u64,
}

impl Dispatcher {
    pub fn new(notifier: Arc<dyn Notifier>) -> Self {
        Self {
            notifier,
            open: BTreeMap::new(),
            outbox: VecDeque::new(),
            consecutive_failures: 0,
            retry_at: None,
            dropped: 0,
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

        let still_wrong: BTreeSet<IncidentKey> = assessments
            .iter()
            .map(|a| (a.subject.clone(), a.reason.condition()))
            .collect();

        let recovered: Vec<IncidentKey> = self
            .open
            .keys()
            .filter(|key| !still_wrong.contains(*key))
            .cloned()
            .collect();

        for key in recovered {
            if let Some(open) = self.open.remove(&key) {
                let lasted = now.duration_since(open.opened_at).unwrap_or(Duration::ZERO);
                queued.push(render_recovery(&key.0, &open.headline, lasted));
            }
        }

        for assessment in assessments {
            let key = (assessment.subject.clone(), assessment.reason.condition());

            match self.open.get_mut(&key) {
                // Already reported at this severity or worse. Saying it again
                // every cycle is how alerting gets muted.
                Some(open) if assessment.severity <= open.severity => {}

                // Escalation: warn became critical. Worth one more message,
                // and the incident keeps its original start time.
                Some(open) => {
                    open.severity = assessment.severity;
                    queued.push(render(assessment, now));
                }

                None => {
                    self.open.insert(
                        key,
                        Open {
                            severity: assessment.severity,
                            opened_at: now,
                            headline: assessment.reason.headline(),
                        },
                    );
                    queued.push(render(assessment, now));
                }
            }
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

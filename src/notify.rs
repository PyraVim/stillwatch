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

use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::config::TelegramConfig;
use crate::evaluate::{Assessment, LastSeen, Reason, Severity};
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
    let Reason::NoHeartbeat {
        silent_for,
        since,
        expect_every,
    } = &assessment.reason;

    let level = Level::from(assessment.severity);
    let subject = &assessment.subject;
    let every = fmt::duration(*expect_every);

    let text = match since {
        LastSeen::Beat(last) => format!(
            "{icon}  {subject} — no heartbeat for {silent}\n\
             \x20   last beat {last}, expected every {every}\n\
             \x20   stillwatch has been up for the whole gap — this is the job, not the watchdog\n\
             \x20   → the loop has stopped; the process has most likely exited or wedged",
            icon = level.icon(),
            silent = fmt::duration(*silent_for),
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
            silent = fmt::duration(*silent_for),
            started = fmt::timestamp(*started, now),
        ),
    };

    Notification {
        subject: subject.clone(),
        level,
        text,
    }
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
                LastSeen::Beat(now - Duration::from_secs(312)),
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
                LastSeen::Beat(now - Duration::from_secs(900)),
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
            LastSeen::Beat(now - Duration::from_secs(400)),
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

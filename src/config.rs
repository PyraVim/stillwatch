//! Configuration loading.
//!
//! The file is primary; the environment supplies secrets and overrides. Loading
//! is a four-step pipeline, in this order:
//!
//! 1. parse the TOML into a value tree
//! 2. interpolate `${VAR}` in every string value
//! 3. apply `STILLWATCH_*` overrides (these win over the file)
//! 4. deserialize, then fill defaults and validate
//!
//! Interpolation walks the *parsed* tree rather than the raw file text. Doing it
//! textually would let a secret containing a quote or a newline rewrite the
//! surrounding TOML.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer};
use toml::Value;

/// Where the config path comes from when `--config` is not given.
pub const CONFIG_PATH_VAR: &str = "STILLWATCH_CONFIG";

/// Used when neither `--config` nor `STILLWATCH_CONFIG` says otherwise.
pub const DEFAULT_CONFIG_PATH: &str = "stillwatch.toml";

const DEFAULT_LISTEN: &str = "127.0.0.1:9111";

/// `warn_after` defaults to this many times `expect_every`.
const WARN_MULTIPLE: u32 = 5;

/// `critical_after` defaults to this many times `expect_every`.
const CRITICAL_MULTIPLE: u32 = 15;

/// Scalar config paths that a `STILLWATCH_*` environment variable may override.
///
/// The mapping is: uppercase the dotted path and replace `.` with `_`. We
/// generate the expected variable name for each known path rather than parsing
/// arbitrary `STILLWATCH_*` names, because config keys contain underscores of
/// their own (`chat_id`) and the reverse mapping is ambiguous.
///
/// Per-job and per-check values are deliberately absent: array elements have no
/// stable key to name them by, and inventing an indexing scheme would be a
/// second config language.
const OVERRIDABLE_PATHS: &[&str] = &["listen", "notify.telegram.token", "notify.telegram.chat_id"];

// ---------------------------------------------------------------------------
// environment access
// ---------------------------------------------------------------------------

/// Read access to the environment.
///
/// A trait so tests can supply a fixed environment. Process environment
/// variables are global mutable state; tests that set them cannot run in
/// parallel and cannot be trusted to clean up after a panic.
pub trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

/// The real process environment.
pub struct SystemEnv;

impl EnvSource for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

impl EnvSource for HashMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        HashMap::get(self, key).cloned()
    }
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read config file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("config file is not valid TOML")]
    Toml(#[from] toml::de::Error),

    #[error("config refers to ${{{var}}}, which is not set in the environment")]
    UndefinedVariable { var: String },

    #[error("config contains an unterminated ${{ in the value {value:?}")]
    UnterminatedVariable { value: String },

    #[error("listen address {value:?} is not a valid host:port")]
    InvalidListen {
        value: String,
        #[source]
        source: std::net::AddrParseError,
    },

    #[error("two jobs are both named {name:?}; job names must be unique")]
    DuplicateJob { name: String },

    #[error("job name {name:?} is not usable: {reason}")]
    InvalidJobName { name: String, reason: &'static str },

    #[error("job {job:?}: {message}")]
    InvalidThreshold { job: String, message: String },
}

// ---------------------------------------------------------------------------
// the resolved config
// ---------------------------------------------------------------------------

/// Configuration after defaults are filled in and everything is validated.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub telegram: Option<TelegramConfig>,
    pub jobs: Vec<JobConfig>,
}

#[derive(Clone)]
pub struct TelegramConfig {
    pub token: String,
    pub chat_id: String,
}

/// Hand-written so a stray `{:?}` on the config never puts a bot token in a log.
impl fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("token", &"<redacted>")
            .field("chat_id", &self.chat_id)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct JobConfig {
    pub name: String,
    /// `None` when the job declares no `[job.alive]` block.
    ///
    /// A job that is quiet by design must not inherit a liveness expectation it
    /// never agreed to, so this stays `None` rather than defaulting.
    pub alive: Option<AliveConfig>,
}

#[derive(Debug, Clone, Copy)]
pub struct AliveConfig {
    pub expect_every: Duration,
    pub warn_after: Duration,
    pub critical_after: Duration,
}

impl Config {
    /// Loads and resolves the config at `path`.
    pub fn load(path: &Path, env: &dyn EnvSource) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml(&text, env)
    }

    /// Resolves a config from TOML text. The whole pipeline except reading the file.
    pub fn from_toml(text: &str, env: &dyn EnvSource) -> Result<Self, ConfigError> {
        // `Value: FromStr` parses a single TOML value, not a document, so the
        // document is parsed as a table and wrapped.
        let mut value = Value::Table(toml::from_str::<toml::Table>(text)?);
        interpolate(&mut value, env)?;
        apply_env_overrides(&mut value, env);

        if let Some(table) = value.as_table() {
            warn_unrecognized(table);
        }

        let raw = RawConfig::deserialize(value)?;
        raw.resolve()
    }
}

/// Resolves the config path: `--config`, then `STILLWATCH_CONFIG`, then the default.
pub fn resolve_path(flag: Option<PathBuf>, env: &dyn EnvSource) -> PathBuf {
    flag.or_else(|| env.get(CONFIG_PATH_VAR).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
}

// ---------------------------------------------------------------------------
// step 2: ${VAR} interpolation
// ---------------------------------------------------------------------------

fn interpolate(value: &mut Value, env: &dyn EnvSource) -> Result<(), ConfigError> {
    match value {
        Value::String(s) => {
            if let Some(expanded) = expand(s, env)? {
                *s = expanded;
            }
            Ok(())
        }
        Value::Array(items) => items.iter_mut().try_for_each(|v| interpolate(v, env)),
        // Keys are job names and section names, never secrets, so only values
        // are expanded.
        Value::Table(table) => table.iter_mut().try_for_each(|(_, v)| interpolate(v, env)),
        _ => Ok(()),
    }
}

/// Expands every `${VAR}` in `input`, or returns `None` if there were none.
fn expand(input: &str, env: &dyn EnvSource) -> Result<Option<String>, ConfigError> {
    if !input.contains("${") {
        return Ok(None);
    }

    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| ConfigError::UnterminatedVariable {
                value: input.to_string(),
            })?;

        let var = &after[..end];
        let resolved = env.get(var).ok_or_else(|| ConfigError::UndefinedVariable {
            var: var.to_string(),
        })?;
        out.push_str(&resolved);
        rest = &after[end + 1..];
    }

    out.push_str(rest);
    Ok(Some(out))
}

// ---------------------------------------------------------------------------
// step 3: STILLWATCH_* overrides
// ---------------------------------------------------------------------------

fn apply_env_overrides(value: &mut Value, env: &dyn EnvSource) {
    for path in OVERRIDABLE_PATHS {
        let var = env_var_for(path);
        let Some(supplied) = env.get(&var) else {
            continue;
        };
        tracing::debug!(%var, path, "config value overridden from the environment");
        set_path(value, path, Value::String(supplied));
    }
}

fn env_var_for(path: &str) -> String {
    format!("STILLWATCH_{}", path.replace('.', "_").to_uppercase())
}

/// Writes `new` at a dotted path, creating intermediate tables as needed.
///
/// Silently gives up if an intermediate key exists and is not a table; that
/// shape error surfaces with a better message during deserialization.
fn set_path(value: &mut Value, path: &str, new: Value) {
    let mut segments = path.split('.').peekable();
    let mut cursor = value;

    while let Some(segment) = segments.next() {
        let Some(table) = cursor.as_table_mut() else {
            return;
        };

        if segments.peek().is_none() {
            table.insert(segment.to_string(), new);
            return;
        }

        cursor = table
            .entry(segment.to_string())
            .or_insert_with(|| Value::Table(toml::Table::new()));
    }
}

// ---------------------------------------------------------------------------
// unrecognized keys
// ---------------------------------------------------------------------------

/// Warns about any key this version does not consume.
///
/// Covers both typos and config written against features that are not built
/// yet. Either way the user needs to know the line they wrote is doing nothing,
/// which is exactly the class of quiet failure this tool exists to catch.
fn warn_unrecognized(root: &toml::Table) {
    for (key, value) in root {
        match key.as_str() {
            "listen" => {}
            "notify" => warn_children(value, "notify", &["telegram"], |v| {
                warn_children(v, "notify.telegram", &["token", "chat_id"], |_| {})
            }),
            "job" => {
                let Some(jobs) = value.as_array() else {
                    continue;
                };
                for (index, job) in jobs.iter().enumerate() {
                    let path = format!("job[{index}]");
                    warn_children(job, &path, &["name", "alive"], |_| {});
                    if let Some(alive) = job.get("alive") {
                        warn_children(
                            alive,
                            &format!("{path}.alive"),
                            &["expect_every", "warn_after", "critical_after"],
                            |_| {},
                        );
                    }
                }
            }
            other => unrecognized(other),
        }
    }
}

fn warn_children(value: &Value, path: &str, known: &[&str], recurse: impl Fn(&Value)) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, child) in table {
        if known.contains(&key.as_str()) {
            recurse(child);
        } else {
            unrecognized(&format!("{path}.{key}"));
        }
    }
}

fn unrecognized(path: &str) {
    tracing::warn!(
        key = %path,
        "ignoring config key this version does not understand — check for a typo, \
         or it may be a feature that is not built yet"
    );
}

// ---------------------------------------------------------------------------
// step 4: deserialize, default, validate
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawConfig {
    listen: Option<String>,
    notify: Option<RawNotify>,
    #[serde(default, rename = "job")]
    jobs: Vec<RawJob>,
}

#[derive(Deserialize)]
struct RawNotify {
    telegram: Option<RawTelegram>,
}

#[derive(Deserialize)]
struct RawTelegram {
    token: String,
    #[serde(deserialize_with = "string_or_integer")]
    chat_id: String,
}

#[derive(Deserialize)]
struct RawJob {
    name: String,
    alive: Option<RawAlive>,
}

#[derive(Deserialize)]
struct RawAlive {
    #[serde(with = "humantime_serde")]
    expect_every: Duration,
    #[serde(default, with = "humantime_serde")]
    warn_after: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    critical_after: Option<Duration>,
}

/// Telegram chat ids are often written unquoted (`chat_id = -1001234567890`).
/// Accepting both spellings is cheaper than explaining the difference.
fn string_or_integer<'de, D: Deserializer<'de>>(de: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Text(String),
        Number(i64),
    }

    Ok(match Either::deserialize(de)? {
        Either::Text(s) => s,
        Either::Number(n) => n.to_string(),
    })
}

impl RawConfig {
    fn resolve(self) -> Result<Config, ConfigError> {
        let listen_text = self.listen.unwrap_or_else(|| DEFAULT_LISTEN.to_string());
        let listen = listen_text
            .parse()
            .map_err(|source| ConfigError::InvalidListen {
                value: listen_text.clone(),
                source,
            })?;

        let telegram = self
            .notify
            .and_then(|n| n.telegram)
            .map(|t| TelegramConfig {
                token: t.token,
                chat_id: t.chat_id,
            });

        let mut jobs = Vec::with_capacity(self.jobs.len());
        for raw in self.jobs {
            let job = raw.resolve()?;
            if jobs
                .iter()
                .any(|existing: &JobConfig| existing.name == job.name)
            {
                return Err(ConfigError::DuplicateJob { name: job.name });
            }
            jobs.push(job);
        }

        Ok(Config {
            listen,
            telegram,
            jobs,
        })
    }
}

impl RawJob {
    fn resolve(self) -> Result<JobConfig, ConfigError> {
        validate_job_name(&self.name)?;
        let alive = self.alive.map(|a| a.resolve(&self.name)).transpose()?;
        Ok(JobConfig {
            name: self.name,
            alive,
        })
    }
}

/// The job name is a path segment in `POST /beat/{job}` and the subject line of
/// every alert about it, so it has to survive both.
fn validate_job_name(name: &str) -> Result<(), ConfigError> {
    let reason = if name.is_empty() {
        Some("it is empty")
    } else if name.contains('/') {
        Some("it contains '/', which cannot appear in the /beat/{job} path")
    } else if name.chars().any(char::is_whitespace) {
        Some("it contains whitespace")
    } else if name.chars().any(char::is_control) {
        Some("it contains a control character")
    } else {
        None
    };

    match reason {
        Some(reason) => Err(ConfigError::InvalidJobName {
            name: name.to_string(),
            reason,
        }),
        None => Ok(()),
    }
}

impl RawAlive {
    fn resolve(self, job: &str) -> Result<AliveConfig, ConfigError> {
        let invalid = |message: String| ConfigError::InvalidThreshold {
            job: job.to_string(),
            message,
        };

        if self.expect_every.is_zero() {
            return Err(invalid(
                "alive.expect_every must be greater than zero".into(),
            ));
        }

        let warn_after = self.warn_after.unwrap_or(self.expect_every * WARN_MULTIPLE);
        let critical_after = self
            .critical_after
            .unwrap_or(self.expect_every * CRITICAL_MULTIPLE);

        // A warn threshold at or inside the expected beat interval fires on
        // every ordinary gap between beats, which trains people to mute it.
        if warn_after <= self.expect_every {
            return Err(invalid(format!(
                "alive.warn_after ({}) must be longer than alive.expect_every ({}), \
                 or it will fire between normal beats",
                crate::fmt::duration(warn_after),
                crate::fmt::duration(self.expect_every),
            )));
        }

        if critical_after <= warn_after {
            return Err(invalid(format!(
                "alive.critical_after ({}) must be longer than alive.warn_after ({})",
                crate::fmt::duration(critical_after),
                crate::fmt::duration(warn_after),
            )));
        }

        Ok(AliveConfig {
            expect_every: self.expect_every,
            warn_after,
            critical_after,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env() -> HashMap<String, String> {
        HashMap::new()
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn load(text: &str) -> Config {
        Config::from_toml(text, &no_env()).expect("config should load")
    }

    // -- defaults ----------------------------------------------------------

    #[test]
    fn minimal_config_is_four_lines() {
        let config = load(
            r#"
[[job]]
name = "product-scraper"
  [job.alive]
  expect_every = "60s"
"#,
        );

        assert_eq!(config.listen.to_string(), DEFAULT_LISTEN);
        assert!(config.telegram.is_none());

        let alive = config.jobs[0].alive.expect("alive rule");
        assert_eq!(alive.expect_every, Duration::from_secs(60));
        assert_eq!(alive.warn_after, Duration::from_secs(5 * 60));
        assert_eq!(alive.critical_after, Duration::from_secs(15 * 60));
    }

    #[test]
    fn explicit_thresholds_win_over_derived_ones() {
        let config = load(
            r#"
[[job]]
name = "clients-etl"
  [job.alive]
  expect_every = "30s"
  warn_after = "2m"
  critical_after = "9m"
"#,
        );

        let alive = config.jobs[0].alive.expect("alive rule");
        assert_eq!(alive.warn_after, Duration::from_secs(120));
        assert_eq!(alive.critical_after, Duration::from_secs(540));
    }

    /// A nightly job must never inherit a liveness expectation it never declared.
    #[test]
    fn job_without_alive_block_gets_no_alive_rule() {
        let config = load(
            r#"
[[job]]
name = "nightly-sync"
"#,
        );

        assert!(
            config.jobs[0].alive.is_none(),
            "a job with no [job.alive] block must have no alive rule at all"
        );
    }

    // -- interpolation -----------------------------------------------------

    #[test]
    fn interpolates_env_vars_into_string_values() {
        let config = Config::from_toml(
            r#"
[notify.telegram]
token = "${TELEGRAM_TOKEN}"
chat_id = "${TELEGRAM_CHAT}"
"#,
            &env(&[
                ("TELEGRAM_TOKEN", "12345:abc"),
                ("TELEGRAM_CHAT", "-100777"),
            ]),
        )
        .expect("config should load");

        let telegram = config.telegram.expect("telegram");
        assert_eq!(telegram.token, "12345:abc");
        assert_eq!(telegram.chat_id, "-100777");
    }

    #[test]
    fn interpolation_handles_surrounding_text_and_repeats() {
        let config = Config::from_toml(
            r#"listen = "${HOST}:${PORT}""#,
            &env(&[("HOST", "0.0.0.0"), ("PORT", "9111")]),
        )
        .expect("config should load");

        assert_eq!(config.listen.to_string(), "0.0.0.0:9111");
    }

    #[test]
    fn undefined_variable_is_a_startup_error_naming_the_variable() {
        let err = Config::from_toml(
            r#"
[notify.telegram]
token = "${TELEGRAM_TOKEN}"
chat_id = "1"
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(
            matches!(&err, ConfigError::UndefinedVariable { var } if var == "TELEGRAM_TOKEN"),
            "unexpected error: {err}"
        );
        assert!(err.to_string().contains("TELEGRAM_TOKEN"));
    }

    #[test]
    fn unterminated_variable_is_an_error() {
        let err = Config::from_toml(r#"listen = "${OOPS""#, &no_env()).expect_err("should fail");
        assert!(matches!(err, ConfigError::UnterminatedVariable { .. }));
    }

    /// `${VAR:-default}` is deliberately not supported; it would be the start of
    /// a second config language.
    #[test]
    fn default_value_syntax_is_not_interpreted() {
        let err = Config::from_toml(r#"listen = "${HOST:-127.0.0.1}""#, &no_env())
            .expect_err("should fail");

        assert!(
            matches!(&err, ConfigError::UndefinedVariable { var } if var == "HOST:-127.0.0.1"),
            "unexpected error: {err}"
        );
    }

    // -- env overrides -----------------------------------------------------

    #[test]
    fn env_override_beats_the_file() {
        let config = Config::from_toml(
            r#"
listen = "127.0.0.1:9111"

[notify.telegram]
token = "in-file"
chat_id = "1"
"#,
            &env(&[
                ("STILLWATCH_LISTEN", "0.0.0.0:8080"),
                ("STILLWATCH_NOTIFY_TELEGRAM_TOKEN", "from-env"),
            ]),
        )
        .expect("config should load");

        assert_eq!(config.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(config.telegram.expect("telegram").token, "from-env");
    }

    #[test]
    fn env_override_beats_interpolation() {
        let config = Config::from_toml(
            r#"
[notify.telegram]
token = "${TELEGRAM_TOKEN}"
chat_id = "1"
"#,
            &env(&[
                ("TELEGRAM_TOKEN", "interpolated"),
                ("STILLWATCH_NOTIFY_TELEGRAM_TOKEN", "overridden"),
            ]),
        )
        .expect("config should load");

        assert_eq!(config.telegram.expect("telegram").token, "overridden");
    }

    #[test]
    fn env_override_creates_missing_sections() {
        let config = Config::from_toml(
            "",
            &env(&[
                ("STILLWATCH_NOTIFY_TELEGRAM_TOKEN", "from-env"),
                ("STILLWATCH_NOTIFY_TELEGRAM_CHAT_ID", "-100777"),
            ]),
        )
        .expect("config should load");

        let telegram = config.telegram.expect("telegram");
        assert_eq!(telegram.token, "from-env");
        assert_eq!(telegram.chat_id, "-100777");
    }

    #[test]
    fn env_var_names_are_the_dotted_path_uppercased() {
        assert_eq!(env_var_for("listen"), "STILLWATCH_LISTEN");
        assert_eq!(
            env_var_for("notify.telegram.chat_id"),
            "STILLWATCH_NOTIFY_TELEGRAM_CHAT_ID"
        );
    }

    // -- validation --------------------------------------------------------

    #[test]
    fn warn_after_must_exceed_expect_every() {
        let err = Config::from_toml(
            r#"
[[job]]
name = "product-scraper"
  [job.alive]
  expect_every = "60s"
  warn_after = "60s"
  critical_after = "15m"
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(
            matches!(&err, ConfigError::InvalidThreshold { job, .. } if job == "product-scraper"),
            "unexpected error: {err}"
        );
        assert!(err.to_string().contains("warn_after"));
    }

    #[test]
    fn critical_after_must_exceed_warn_after() {
        let err = Config::from_toml(
            r#"
[[job]]
name = "clients-etl"
  [job.alive]
  expect_every = "60s"
  warn_after = "10m"
  critical_after = "5m"
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(err.to_string().contains("critical_after"));
    }

    #[test]
    fn duplicate_job_names_are_rejected() {
        let err = Config::from_toml(
            r#"
[[job]]
name = "nightly-sync"

[[job]]
name = "nightly-sync"
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(matches!(&err, ConfigError::DuplicateJob { name } if name == "nightly-sync"));
    }

    #[test]
    fn job_names_that_break_the_beat_url_are_rejected() {
        for bad in ["", "with space", "a/b"] {
            let text = format!("[[job]]\nname = \"{bad}\"\n");
            let err = Config::from_toml(&text, &no_env()).expect_err("should fail");
            assert!(
                matches!(err, ConfigError::InvalidJobName { .. }),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn chat_id_may_be_written_unquoted() {
        let config = load(
            r#"
[notify.telegram]
token = "t"
chat_id = -1001234567890
"#,
        );

        assert_eq!(config.telegram.expect("telegram").chat_id, "-1001234567890");
    }

    #[test]
    fn telegram_token_is_not_printed_by_debug() {
        let config = load(
            r#"
[notify.telegram]
token = "12345:super-secret"
chat_id = "-100777"
"#,
        );

        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("super-secret"),
            "token leaked: {rendered}"
        );
        assert!(rendered.contains("redacted"));
    }

    // -- shape -------------------------------------------------------------

    #[test]
    fn config_covering_two_workload_shapes_loads() {
        let config = load(
            r#"
listen = "127.0.0.1:9111"

# a continuously-running job
[[job]]
name = "product-scraper"
  [job.alive]
  expect_every = "60s"
  warn_after = "5m"
  critical_after = "15m"

# a job that is quiet by design
[[job]]
name = "nightly-sync"
"#,
        );

        assert_eq!(config.jobs.len(), 2);
        assert!(config.jobs[0].alive.is_some());
        assert!(config.jobs[1].alive.is_none());
    }

    #[test]
    fn keys_from_later_phases_are_ignored_rather_than_fatal() {
        let config = load(
            r#"
[[job]]
name = "product-scraper"
  [job.alive]
  expect_every = "60s"
  [job.freshness]
  warn_after = "10m"

[[check]]
name = "vendor-api"
type = "http"
url = "https://api.vendor.com/health"
"#,
        );

        assert_eq!(config.jobs.len(), 1);
        assert!(config.jobs[0].alive.is_some());
    }

    #[test]
    fn config_path_prefers_flag_then_env_then_default() {
        let with_var = env(&[(CONFIG_PATH_VAR, "/etc/stillwatch.toml")]);

        assert_eq!(
            resolve_path(Some(PathBuf::from("/flag.toml")), &with_var),
            PathBuf::from("/flag.toml")
        );
        assert_eq!(
            resolve_path(None, &with_var),
            PathBuf::from("/etc/stillwatch.toml")
        );
        assert_eq!(
            resolve_path(None, &no_env()),
            PathBuf::from(DEFAULT_CONFIG_PATH)
        );
    }
}

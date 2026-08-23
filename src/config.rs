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

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use reqwest::Url;
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

/// How often a dependency is probed when the check does not say.
const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// How long a single probe may take when the check does not say.
const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// `down_after` defaults to this many intervals of unbroken failure.
const DOWN_AFTER_INTERVALS: u32 = 2;

/// Observations needed before a baseline is trusted, when not configured.
const DEFAULT_MIN_SAMPLES: usize = 30;

/// "Current" latency is the last this many probes.
const RECENT_INTERVALS: u32 = 20;

/// Denominator total a ratio rule needs before it is judged, when not configured.
const DEFAULT_MIN_SAMPLE: u64 = 30;

/// How long a watched process may be absent before that is reported.
const DEFAULT_ABSENT_AFTER: Duration = Duration::from_secs(60);

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

    /// Jobs and checks share one namespace: both become the subject line of an
    /// alert, and the notifier deduplicates on that subject. Two subjects with
    /// the same name would silently suppress each other's alerts.
    #[error("{name:?} is used as the name of more than one job or check; names must be unique")]
    DuplicateSubject { name: String },

    #[error("job name {name:?} is not usable: {reason}")]
    InvalidJobName { name: String, reason: &'static str },

    #[error("job {job:?}: {message}")]
    InvalidThreshold { job: String, message: String },

    #[error("check {check:?} has type {kind:?}, which is not one of \"http\" or \"jsonrpc\"")]
    UnknownCheckType { check: String, kind: String },

    #[error("check {check:?}: url {url:?} could not be parsed")]
    InvalidCheckUrl {
        check: String,
        url: String,
        #[source]
        source: url::ParseError,
    },

    #[error("check {check:?}: {message}")]
    InvalidCheck { check: String, message: String },

    #[error("job {job:?}: log.error_regex {pattern:?} is not a valid regular expression")]
    InvalidRegex {
        job: String,
        pattern: String,
        #[source]
        source: Box<regex::Error>,
    },
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
    pub checks: Vec<CheckConfig>,
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
    /// never agreed to, so this stays `None` rather than defaulting. The same
    /// goes for every other signal below.
    pub alive: Option<AliveConfig>,

    /// How long the job may go without doing real work.
    pub worked: Option<SilenceConfig>,

    /// How stale the data the job acts on may get.
    pub freshness: Option<SilenceConfig>,

    /// Named counter ratios. Empty when the job declares none.
    pub ratios: Vec<RatioConfig>,

    /// What the job is expected to do about reporting in.
    pub mode: JobMode,

    /// Watch a pidfile rather than waiting to be told.
    pub process: Option<ProcessWatch>,

    /// Watch whether a log is still moving, and whether it is saying anything bad.
    pub log: Option<LogWatch>,

    /// Watch whether the output is actually being produced.
    pub artifact: Option<ArtifactWatch>,
}

/// Whether a job reports in, or is watched from outside.
///
/// Mostly a declaration of intent — the passive blocks work either way, and a
/// job that both beats *and* has its output watched is a perfectly good idea.
/// Declaring `passive` is what lets stillwatch catch the contradiction of a job
/// that is not sending heartbeats being judged on missing them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobMode {
    #[default]
    Active,
    Passive,
}

/// Is the process there at all?
///
/// The weakest of the three passive signals: a pidfile says a process with that
/// id exists, not that it is doing anything, and not even reliably that it is
/// *your* process. See the note on PID reuse in the README.
#[derive(Debug, Clone)]
pub struct ProcessWatch {
    pub pidfile: PathBuf,

    /// How long the process must be absent before that is reported.
    ///
    /// Defaults to a minute. Without it, an ordinary restart — during which the
    /// pidfile is briefly gone — would page.
    pub absent_after: Duration,
}

/// Is the log still moving, and is it saying anything bad?
#[derive(Debug, Clone)]
pub struct LogWatch {
    pub path: PathBuf,

    /// How long the file may go without growing before that is reported.
    pub stale_after: Duration,

    /// Lines matching this are reported as errors the job itself logged.
    pub error_regex: Option<Regex>,
}

/// Is the output actually being produced?
#[derive(Debug, Clone)]
pub struct ArtifactWatch {
    pub path: PathBuf,

    /// How old the file may be before that is reported.
    pub stale_after: Duration,

    /// A file smaller than this does not count as produced. An empty export is
    /// the classic "ran, exited zero, wrote nothing" outcome.
    pub min_bytes: u64,
}

impl JobConfig {
    /// A job with a name and no rules at all — watched for nothing.
    ///
    /// Every signal is opt-in, so this is the honest starting point rather than
    /// a degenerate case. Struct-update syntax builds the rest.
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            alive: None,
            worked: None,
            freshness: None,
            ratios: Vec::new(),
            mode: JobMode::Active,
            process: None,
            log: None,
            artifact: None,
        }
    }

    /// Whether this job is watched from outside rather than reporting in.
    pub fn is_passive(&self) -> bool {
        self.process.is_some() || self.log.is_some() || self.artifact.is_some()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AliveConfig {
    pub expect_every: Duration,
    pub warn_after: Duration,
    pub critical_after: Duration,
}

/// Thresholds on how long something may go unreported.
///
/// Shared by `worked` and `freshness` because they are the same shape: a
/// duration that is fine, then concerning, then bad. Unlike `alive` there is no
/// `expect_every` to derive defaults from — these signals are irregular by
/// nature, which is the whole reason they are separate from liveness.
#[derive(Debug, Clone, Copy)]
pub struct SilenceConfig {
    pub warn_after: Duration,

    /// Optional. A job may reasonably want "tell me" without "wake me".
    pub critical_after: Option<Duration>,
}

/// A rule over two named counters.
///
/// The job decides what the counters mean; this decides what is alarming. A
/// scraper reports `fetched`/`parsed`, an ETL reports `rows_read`/`rows_written`,
/// a bot reports `submitted`/`landed`. Same machinery, no domain knowledge.
#[derive(Debug, Clone)]
pub struct RatioConfig {
    /// What to call this rule in an alert: "parse rate", "write rate".
    pub name: String,

    pub numerator: String,
    pub denominator: String,

    /// How far back counters are summed.
    pub window: Duration,

    /// The floor the ratio must stay at or above.
    pub min: f64,

    /// The denominator total needed before the rule is judged at all.
    ///
    /// Guaranteed at least 1 by validation, which is what makes computing the
    /// ratio safe: the rule is skipped as unjudged long before the denominator
    /// could be zero.
    pub min_sample: u64,

    /// What the operator wants said when it fires.
    pub message: Option<String>,
}

/// A dependency probed directly, on its own schedule.
#[derive(Debug, Clone)]
pub struct CheckConfig {
    pub name: String,
    pub probe: ProbeConfig,
    pub interval: Duration,
    pub timeout: Duration,

    /// How long every probe must have been failing before the check is called
    /// down. Defaults to two intervals, so one blip is not a page.
    pub down_after: Duration,

    /// `None` when the check declares no `[check.degradation]` block, in which
    /// case it is watched for up/down only and never judged on latency.
    pub degradation: Option<DegradationConfig>,
}

#[derive(Debug, Clone)]
pub enum ProbeConfig {
    Http { url: Url },
    JsonRpc { url: Url, method: String },
}

impl ProbeConfig {
    pub fn url(&self) -> &Url {
        match self {
            ProbeConfig::Http { url } | ProbeConfig::JsonRpc { url, .. } => url,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            ProbeConfig::Http { .. } => "http",
            ProbeConfig::JsonRpc { .. } => "jsonrpc",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DegradationConfig {
    /// The span the baseline is drawn from. The most recent `recent_window` is
    /// excluded from it, so a slowdown that is still happening does not get to
    /// teach the baseline that it is normal.
    pub baseline_window: Duration,

    /// The span "current" latency is measured over. Derived, not configured:
    /// twenty probes' worth, capped at a third of the baseline window.
    pub recent_window: Duration,

    pub warn_multiple: f64,
    pub critical_multiple: f64,

    /// Latency that is unacceptable no matter what the baseline has learned.
    ///
    /// This is the only thing standing between a poisoned baseline and silence.
    /// If stillwatch starts while a dependency is already slow, the baseline
    /// learns that slow is normal and the multiples will never fire — so the
    /// ceiling is evaluated independently of the baseline, and before any
    /// baseline exists at all. Set it to what you actually consider
    /// unacceptable, not to some extreme.
    pub absolute_ceiling: Duration,

    /// Observations needed before the baseline is trusted enough to judge
    /// anything.
    pub min_samples: usize,
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
                    warn_children(
                        job,
                        &path,
                        &[
                            "name",
                            "mode",
                            "alive",
                            "worked",
                            "freshness",
                            "ratio",
                            "process",
                            "log",
                            "artifact",
                        ],
                        |_| {},
                    );
                    for (block, keys) in [
                        ("process", &["pidfile", "absent_after"][..]),
                        ("log", &["path", "stale_after", "error_regex"][..]),
                        ("artifact", &["path", "stale_after", "min_bytes"][..]),
                    ] {
                        if let Some(value) = job.get(block) {
                            warn_children(value, &format!("{path}.{block}"), keys, |_| {});
                        }
                    }
                    if let Some(alive) = job.get("alive") {
                        warn_children(
                            alive,
                            &format!("{path}.alive"),
                            &["expect_every", "warn_after", "critical_after"],
                            |_| {},
                        );
                    }
                    for signal in ["worked", "freshness"] {
                        if let Some(block) = job.get(signal) {
                            warn_children(
                                block,
                                &format!("{path}.{signal}"),
                                &["warn_after", "critical_after"],
                                |_| {},
                            );
                        }
                    }
                    if let Some(ratios) = job.get("ratio").and_then(Value::as_array) {
                        for (r, ratio) in ratios.iter().enumerate() {
                            warn_children(
                                ratio,
                                &format!("{path}.ratio[{r}]"),
                                &[
                                    "name",
                                    "numerator",
                                    "denominator",
                                    "window",
                                    "min",
                                    "min_sample",
                                    "message",
                                ],
                                |_| {},
                            );
                        }
                    }
                }
            }
            "check" => {
                let Some(checks) = value.as_array() else {
                    continue;
                };
                for (index, check) in checks.iter().enumerate() {
                    let path = format!("check[{index}]");
                    warn_children(
                        check,
                        &path,
                        &[
                            "name",
                            "type",
                            "url",
                            "method",
                            "interval",
                            "timeout",
                            "down_after",
                            "degradation",
                        ],
                        |_| {},
                    );
                    if let Some(degradation) = check.get("degradation") {
                        warn_children(
                            degradation,
                            &format!("{path}.degradation"),
                            &[
                                "baseline_window",
                                "warn_multiple",
                                "critical_multiple",
                                "absolute_ceiling",
                                "min_samples",
                            ],
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
    #[serde(default, rename = "check")]
    checks: Vec<RawCheck>,
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
    mode: Option<String>,
    alive: Option<RawAlive>,
    worked: Option<RawSilence>,
    freshness: Option<RawSilence>,
    #[serde(default, rename = "ratio")]
    ratios: Vec<RawRatio>,
    process: Option<RawProcess>,
    log: Option<RawLog>,
    artifact: Option<RawArtifact>,
}

#[derive(Deserialize)]
struct RawProcess {
    pidfile: PathBuf,
    #[serde(default, with = "humantime_serde")]
    absent_after: Option<Duration>,
}

#[derive(Deserialize)]
struct RawLog {
    path: PathBuf,
    #[serde(with = "humantime_serde")]
    stale_after: Duration,
    error_regex: Option<String>,
}

#[derive(Deserialize)]
struct RawArtifact {
    path: PathBuf,
    #[serde(with = "humantime_serde")]
    stale_after: Duration,
    min_bytes: Option<u64>,
}

#[derive(Deserialize)]
struct RawSilence {
    #[serde(with = "humantime_serde")]
    warn_after: Duration,
    #[serde(default, with = "humantime_serde")]
    critical_after: Option<Duration>,
}

#[derive(Deserialize)]
struct RawRatio {
    name: String,
    numerator: String,
    denominator: String,
    #[serde(with = "humantime_serde")]
    window: Duration,
    min: f64,
    min_sample: Option<u64>,
    message: Option<String>,
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

#[derive(Deserialize)]
struct RawCheck {
    name: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    url: String,
    method: Option<String>,
    #[serde(default, with = "humantime_serde")]
    interval: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    timeout: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    down_after: Option<Duration>,
    degradation: Option<RawDegradation>,
}

#[derive(Deserialize)]
struct RawDegradation {
    #[serde(with = "humantime_serde")]
    baseline_window: Duration,
    warn_multiple: f64,
    critical_multiple: f64,
    #[serde(with = "humantime_serde")]
    absolute_ceiling: Duration,
    min_samples: Option<usize>,
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

        // Jobs and checks share one subject namespace, so uniqueness is checked
        // across both rather than within each.
        let mut subjects = BTreeSet::new();
        let mut claim = |name: &str| -> Result<(), ConfigError> {
            if subjects.insert(name.to_string()) {
                Ok(())
            } else {
                Err(ConfigError::DuplicateSubject {
                    name: name.to_string(),
                })
            }
        };

        let mut jobs = Vec::with_capacity(self.jobs.len());
        for raw in self.jobs {
            let job = raw.resolve()?;
            claim(&job.name)?;
            jobs.push(job);
        }

        let mut checks = Vec::with_capacity(self.checks.len());
        for raw in self.checks {
            let check = raw.resolve()?;
            claim(&check.name)?;
            checks.push(check);
        }

        Ok(Config {
            listen,
            telegram,
            jobs,
            checks,
        })
    }
}

impl RawCheck {
    fn resolve(self) -> Result<CheckConfig, ConfigError> {
        let name = self.name;
        validate_subject_name(&name).map_err(|reason| ConfigError::InvalidCheck {
            check: name.clone(),
            message: format!("the name is not usable: {reason}"),
        })?;

        let invalid = |message: String| ConfigError::InvalidCheck {
            check: name.clone(),
            message,
        };

        let url = Url::parse(&self.url).map_err(|source| ConfigError::InvalidCheckUrl {
            check: name.clone(),
            url: self.url.clone(),
            source,
        })?;

        let kind = self.kind.as_deref().unwrap_or("http");
        let probe = match kind {
            "http" => {
                if self.method.is_some() {
                    return Err(invalid(
                        "`method` only means something for a jsonrpc check".to_string(),
                    ));
                }
                ProbeConfig::Http { url }
            }
            "jsonrpc" => {
                let method = self.method.clone().ok_or_else(|| {
                    invalid("a jsonrpc check needs a `method` to call".to_string())
                })?;
                ProbeConfig::JsonRpc { url, method }
            }
            other => {
                return Err(ConfigError::UnknownCheckType {
                    check: name,
                    kind: other.to_string(),
                })
            }
        };

        let interval = self.interval.unwrap_or(DEFAULT_CHECK_INTERVAL);
        if interval.is_zero() {
            return Err(invalid("interval must be greater than zero".to_string()));
        }

        let timeout = self.timeout.unwrap_or(DEFAULT_CHECK_TIMEOUT);
        if timeout.is_zero() {
            return Err(invalid("timeout must be greater than zero".to_string()));
        }
        // A probe allowed to outlast its own interval overlaps itself, and the
        // latency samples stop meaning what they claim to mean.
        if timeout >= interval {
            return Err(invalid(format!(
                "timeout ({}) must be shorter than interval ({})",
                crate::fmt::duration(timeout),
                crate::fmt::duration(interval),
            )));
        }

        let down_after = self.down_after.unwrap_or(interval * DOWN_AFTER_INTERVALS);
        if down_after < interval {
            return Err(invalid(format!(
                "down_after ({}) is shorter than interval ({}), so it could fire before \
                 a single probe has had the chance to run",
                crate::fmt::duration(down_after),
                crate::fmt::duration(interval),
            )));
        }

        let degradation = self
            .degradation
            .map(|raw| raw.resolve(&name, interval))
            .transpose()?;

        Ok(CheckConfig {
            name,
            probe,
            interval,
            timeout,
            down_after,
            degradation,
        })
    }
}

impl RawDegradation {
    fn resolve(self, check: &str, interval: Duration) -> Result<DegradationConfig, ConfigError> {
        let invalid = |message: String| ConfigError::InvalidCheck {
            check: check.to_string(),
            message,
        };

        if self.baseline_window.is_zero() {
            return Err(invalid(
                "degradation.baseline_window must be greater than zero".to_string(),
            ));
        }
        if self.absolute_ceiling.is_zero() {
            return Err(invalid(
                "degradation.absolute_ceiling must be greater than zero".to_string(),
            ));
        }
        if !(self.warn_multiple.is_finite() && self.warn_multiple > 1.0) {
            return Err(invalid(format!(
                "degradation.warn_multiple ({}) must be greater than 1.0, or it would fire \
                 at or below the baseline itself",
                self.warn_multiple
            )));
        }
        if !(self.critical_multiple.is_finite() && self.critical_multiple > self.warn_multiple) {
            return Err(invalid(format!(
                "degradation.critical_multiple ({}) must be greater than warn_multiple ({})",
                self.critical_multiple, self.warn_multiple
            )));
        }

        // Current latency is the last twenty probes, but never more than a third
        // of the window — otherwise there would be little left to form a
        // baseline from.
        let recent_window = (interval * RECENT_INTERVALS).min(self.baseline_window / 3);
        let baseline_span = self.baseline_window.saturating_sub(recent_window);

        let min_samples = self.min_samples.unwrap_or(DEFAULT_MIN_SAMPLES);
        if min_samples == 0 {
            return Err(invalid(
                "degradation.min_samples must be at least 1; a baseline drawn from no \
                 observations is not a baseline"
                    .to_string(),
            ));
        }

        // A check that could never accumulate enough samples would sit in
        // "warming up" forever, judging nothing — and the whole point of the
        // warming state is that nobody should have to guess whether a check is
        // being judged. That is a config error, and it is discoverable here.
        let capacity = (baseline_span.as_secs_f64() / interval.as_secs_f64()).floor() as usize;
        if capacity < min_samples {
            return Err(invalid(format!(
                "degradation.baseline_window ({}) leaves room for at most {capacity} \
                 baseline probes at interval {}, fewer than min_samples ({min_samples}); \
                 this check would never finish warming up",
                crate::fmt::duration(self.baseline_window),
                crate::fmt::duration(interval),
            )));
        }

        Ok(DegradationConfig {
            baseline_window: self.baseline_window,
            recent_window,
            warn_multiple: self.warn_multiple,
            critical_multiple: self.critical_multiple,
            absolute_ceiling: self.absolute_ceiling,
            min_samples,
        })
    }
}

impl RawJob {
    fn resolve(self) -> Result<JobConfig, ConfigError> {
        validate_job_name(&self.name)?;
        let alive = self.alive.map(|a| a.resolve(&self.name)).transpose()?;

        let worked = self
            .worked
            .map(|w| w.resolve(&self.name, "worked"))
            .transpose()?;
        let freshness = self
            .freshness
            .map(|f| f.resolve(&self.name, "freshness"))
            .transpose()?;

        let mut ratios: Vec<RatioConfig> = Vec::with_capacity(self.ratios.len());
        for raw in self.ratios {
            let ratio = raw.resolve(&self.name)?;
            // Ratio names key the dedup for their own alerts, so two rules on
            // one job sharing a name would suppress each other.
            if ratios.iter().any(|existing| existing.name == ratio.name) {
                return Err(ConfigError::InvalidThreshold {
                    job: self.name.clone(),
                    message: format!(
                        "two ratio rules are both named {:?}; names must be unique within a job",
                        ratio.name
                    ),
                });
            }
            ratios.push(ratio);
        }

        let invalid = |message: String| ConfigError::InvalidThreshold {
            job: self.name.clone(),
            message,
        };

        let mode = match self.mode.as_deref() {
            None | Some("active") => JobMode::Active,
            Some("passive") => JobMode::Passive,
            Some(other) => {
                return Err(invalid(format!(
                    "mode {other:?} is not one of \"active\" or \"passive\""
                )))
            }
        };

        // A job that is not sending heartbeats cannot be judged on missing
        // them. Catching this at startup beats discovering it as a permanent
        // alert five minutes after deploy.
        if mode == JobMode::Passive && alive.is_some() {
            return Err(invalid(
                "a passive job is watched from outside and sends no heartbeats, so a \
                 [job.alive] block would fire forever"
                    .to_string(),
            ));
        }

        let process = self
            .process
            .map(|raw| raw.resolve(&self.name))
            .transpose()?;
        let log = self.log.map(|raw| raw.resolve(&self.name)).transpose()?;
        let artifact = self
            .artifact
            .map(|raw| raw.resolve(&self.name))
            .transpose()?;

        if mode == JobMode::Passive && process.is_none() && log.is_none() && artifact.is_none() {
            return Err(invalid(
                "a passive job needs at least one of [job.process], [job.log] or \
                 [job.artifact]; otherwise nothing is watching it at all"
                    .to_string(),
            ));
        }

        Ok(JobConfig {
            name: self.name,
            alive,
            worked,
            freshness,
            ratios,
            mode,
            process,
            log,
            artifact,
        })
    }
}

impl RawProcess {
    fn resolve(self, job: &str) -> Result<ProcessWatch, ConfigError> {
        if self.pidfile.as_os_str().is_empty() {
            return Err(ConfigError::InvalidThreshold {
                job: job.to_string(),
                message: "process.pidfile must be a path".to_string(),
            });
        }

        Ok(ProcessWatch {
            pidfile: self.pidfile,
            absent_after: self.absent_after.unwrap_or(DEFAULT_ABSENT_AFTER),
        })
    }
}

impl RawLog {
    fn resolve(self, job: &str) -> Result<LogWatch, ConfigError> {
        let invalid = |message: String| ConfigError::InvalidThreshold {
            job: job.to_string(),
            message,
        };

        if self.path.as_os_str().is_empty() {
            return Err(invalid("log.path must be a path".to_string()));
        }
        if self.stale_after.is_zero() {
            return Err(invalid(
                "log.stale_after must be greater than zero".to_string(),
            ));
        }

        let error_regex = self
            .error_regex
            .map(|pattern| {
                Regex::new(&pattern).map_err(|source| ConfigError::InvalidRegex {
                    job: job.to_string(),
                    pattern,
                    source: Box::new(source),
                })
            })
            .transpose()?;

        Ok(LogWatch {
            path: self.path,
            stale_after: self.stale_after,
            error_regex,
        })
    }
}

impl RawArtifact {
    fn resolve(self, job: &str) -> Result<ArtifactWatch, ConfigError> {
        let invalid = |message: String| ConfigError::InvalidThreshold {
            job: job.to_string(),
            message,
        };

        if self.path.as_os_str().is_empty() {
            return Err(invalid("artifact.path must be a path".to_string()));
        }
        if self.stale_after.is_zero() {
            return Err(invalid(
                "artifact.stale_after must be greater than zero".to_string(),
            ));
        }

        Ok(ArtifactWatch {
            path: self.path,
            stale_after: self.stale_after,
            min_bytes: self.min_bytes.unwrap_or_default(),
        })
    }
}

impl RawSilence {
    fn resolve(self, job: &str, signal: &str) -> Result<SilenceConfig, ConfigError> {
        let invalid = |message: String| ConfigError::InvalidThreshold {
            job: job.to_string(),
            message,
        };

        if self.warn_after.is_zero() {
            return Err(invalid(format!(
                "{signal}.warn_after must be greater than zero"
            )));
        }

        if let Some(critical_after) = self.critical_after {
            if critical_after <= self.warn_after {
                return Err(invalid(format!(
                    "{signal}.critical_after ({}) must be longer than {signal}.warn_after ({})",
                    crate::fmt::duration(critical_after),
                    crate::fmt::duration(self.warn_after),
                )));
            }
        }

        Ok(SilenceConfig {
            warn_after: self.warn_after,
            critical_after: self.critical_after,
        })
    }
}

impl RawRatio {
    fn resolve(self, job: &str) -> Result<RatioConfig, ConfigError> {
        let invalid = |message: String| ConfigError::InvalidThreshold {
            job: job.to_string(),
            message,
        };

        if self.name.trim().is_empty() {
            return Err(invalid("a ratio rule needs a name".to_string()));
        }
        if self.numerator.is_empty() || self.denominator.is_empty() {
            return Err(invalid(format!(
                "ratio {:?} needs both a numerator and a denominator counter",
                self.name
            )));
        }
        if self.numerator == self.denominator {
            return Err(invalid(format!(
                "ratio {:?} divides {:?} by itself, which is always 1",
                self.name, self.numerator
            )));
        }
        if self.window.is_zero() {
            return Err(invalid(format!(
                "ratio {:?}: window must be greater than zero",
                self.name
            )));
        }
        if !(self.min.is_finite() && self.min > 0.0 && self.min <= 1.0) {
            return Err(invalid(format!(
                "ratio {:?}: min ({}) must be greater than 0 and at most 1",
                self.name, self.min
            )));
        }

        // At least one, always. The rule is reported as unjudged until the
        // denominator reaches this, so the ratio is never computed against a
        // zero denominator — the check below is what makes the division safe
        // rather than a guard at the point of division.
        let min_sample = self.min_sample.unwrap_or(DEFAULT_MIN_SAMPLE);
        if min_sample == 0 {
            return Err(invalid(format!(
                "ratio {:?}: min_sample must be at least 1; a ratio computed from no \
                 observations would be a division by zero",
                self.name
            )));
        }

        Ok(RatioConfig {
            name: self.name,
            numerator: self.numerator,
            denominator: self.denominator,
            window: self.window,
            min: self.min,
            min_sample,
            message: self.message,
        })
    }
}

/// A job name is a path segment in `POST /beat/{job}`; every subject name is the
/// lead line of an alert and the key the notifier deduplicates on. Names have to
/// survive all of that.
fn validate_subject_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        Err("it is empty")
    } else if name.contains('/') {
        Err("it contains '/', which cannot appear in the /beat/{job} path")
    } else if name.chars().any(char::is_whitespace) {
        Err("it contains whitespace")
    } else if name.chars().any(char::is_control) {
        Err("it contains a control character")
    } else {
        Ok(())
    }
}

fn validate_job_name(name: &str) -> Result<(), ConfigError> {
    validate_subject_name(name).map_err(|reason| ConfigError::InvalidJobName {
        name: name.to_string(),
        reason,
    })
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

        assert!(matches!(&err, ConfigError::DuplicateSubject { name } if name == "nightly-sync"));
    }

    /// Jobs and checks both become alert subjects, and the notifier dedups on
    /// the subject — so a job and a check sharing a name would silently
    /// suppress each other's alerts.
    #[test]
    fn a_job_and_a_check_may_not_share_a_name() {
        let err = Config::from_toml(
            r#"
[[job]]
name = "vendor-api"

[[check]]
name = "vendor-api"
url  = "https://api.vendor.com/health"
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(matches!(&err, ConfigError::DuplicateSubject { name } if name == "vendor-api"));
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

    // -- worked, freshness, ratios ----------------------------------------

    #[test]
    fn the_spec_example_quiet_job_resolves() {
        let config = load(
            r#"
[[job]]
name = "nightly-sync"

  [job.worked]
  warn_after     = "26h"
  critical_after = "50h"

  [[job.ratio]]
  name        = "write rate"
  numerator   = "rows_written"
  denominator = "rows_read"
  window      = "24h"
  min         = 0.99
  min_sample  = 100
"#,
        );

        let job = &config.jobs[0];
        let worked = job.worked.expect("worked");
        assert_eq!(worked.warn_after, Duration::from_secs(26 * 3_600));
        assert_eq!(worked.critical_after, Some(Duration::from_secs(50 * 3_600)));

        assert_eq!(job.ratios.len(), 1);
        assert_eq!(job.ratios[0].name, "write rate");
        assert_eq!(job.ratios[0].numerator, "rows_written");
        assert_eq!(job.ratios[0].min_sample, 100);
        assert!(job.ratios[0].message.is_none());
    }

    /// A job may want telling without waking anyone, so `critical_after` is
    /// optional on the irregular signals.
    #[test]
    fn worked_and_freshness_may_declare_a_warning_only() {
        let config = load(
            r#"
[[job]]
name = "nightly-sync"
  [job.freshness]
  warn_after = "26h"
"#,
        );

        let freshness = config.jobs[0].freshness.expect("freshness");
        assert_eq!(freshness.warn_after, Duration::from_secs(26 * 3_600));
        assert_eq!(freshness.critical_after, None);
    }

    #[test]
    fn a_job_declaring_none_of_the_irregular_signals_gets_none_of_them() {
        let config = load("[[job]]\nname = \"nightly-sync\"\n");
        let job = &config.jobs[0];

        assert!(job.alive.is_none());
        assert!(job.worked.is_none());
        assert!(job.freshness.is_none());
        assert!(job.ratios.is_empty());
    }

    #[test]
    fn silence_thresholds_must_be_ordered() {
        let err = Config::from_toml(
            r#"
[[job]]
name = "nightly-sync"
  [job.worked]
  warn_after     = "50h"
  critical_after = "26h"
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(err.to_string().contains("worked.critical_after"), "{err}");
    }

    #[test]
    fn a_ratio_gets_a_default_min_sample() {
        let config = load(
            r#"
[[job]]
name = "product-scraper"
  [[job.ratio]]
  name        = "parse rate"
  numerator   = "parsed"
  denominator = "fetched"
  window      = "1h"
  min         = 0.9
"#,
        );

        assert_eq!(config.jobs[0].ratios[0].min_sample, 30);
    }

    /// `min_sample` of zero is the only way the ratio could ever be computed
    /// against a zero denominator, so it is refused at startup.
    #[test]
    fn a_ratio_min_sample_of_zero_is_rejected() {
        let err = Config::from_toml(
            r#"
[[job]]
name = "product-scraper"
  [[job.ratio]]
  name        = "parse rate"
  numerator   = "parsed"
  denominator = "fetched"
  window      = "1h"
  min         = 0.9
  min_sample  = 0
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(err.to_string().contains("division by zero"), "{err}");
    }

    #[test]
    fn a_ratio_min_outside_zero_to_one_is_rejected() {
        for min in ["0.0", "1.5", "-0.5"] {
            let text = format!(
                r#"
[[job]]
name = "product-scraper"
  [[job.ratio]]
  name        = "parse rate"
  numerator   = "parsed"
  denominator = "fetched"
  window      = "1h"
  min         = {min}
"#
            );
            let err = Config::from_toml(&text, &no_env()).expect_err("should fail");
            assert!(err.to_string().contains("min"), "min {min}: {err}");
        }
    }

    #[test]
    fn a_ratio_dividing_a_counter_by_itself_is_rejected() {
        let err = Config::from_toml(
            r#"
[[job]]
name = "product-scraper"
  [[job.ratio]]
  name        = "parse rate"
  numerator   = "fetched"
  denominator = "fetched"
  window      = "1h"
  min         = 0.9
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(err.to_string().contains("always 1"), "{err}");
    }

    #[test]
    fn two_ratios_on_one_job_may_not_share_a_name() {
        let err = Config::from_toml(
            r#"
[[job]]
name = "product-scraper"
  [[job.ratio]]
  name        = "parse rate"
  numerator   = "parsed"
  denominator = "fetched"
  window      = "1h"
  min         = 0.9
  [[job.ratio]]
  name        = "parse rate"
  numerator   = "stored"
  denominator = "parsed"
  window      = "1h"
  min         = 0.9
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(err.to_string().contains("must be unique"), "{err}");
    }

    #[test]
    fn a_job_may_carry_several_differently_named_ratios() {
        let config = load(
            r#"
[[job]]
name = "product-scraper"
  [[job.ratio]]
  name        = "parse rate"
  numerator   = "parsed"
  denominator = "fetched"
  window      = "1h"
  min         = 0.9
  message     = "fetching fine, parsing broken — source markup likely changed"
  [[job.ratio]]
  name        = "store rate"
  numerator   = "stored"
  denominator = "parsed"
  window      = "1h"
  min         = 0.95
"#,
        );

        assert_eq!(config.jobs[0].ratios.len(), 2);
        assert!(config.jobs[0].ratios[0].message.is_some());
        assert!(config.jobs[0].ratios[1].message.is_none());
    }

    // -- passive mode ------------------------------------------------------

    #[test]
    fn the_spec_example_passive_job_resolves() {
        let config = load(
            r#"
[[job]]
name = "clients-etl"
mode = "passive"

  [job.process]
  pidfile = "/var/run/etl.pid"

  [job.log]
  path        = "/var/log/etl.log"
  stale_after = "10m"
  error_regex = "(?i)(traceback|fatal|failed to write)"

  [job.artifact]
  path        = "/data/exports/daily.csv"
  stale_after = "26h"
  min_bytes   = 1024
"#,
        );

        let job = &config.jobs[0];
        assert_eq!(job.mode, JobMode::Passive);
        assert!(job.is_passive());

        let process = job.process.as_ref().expect("process");
        assert_eq!(process.pidfile, PathBuf::from("/var/run/etl.pid"));
        assert_eq!(process.absent_after, Duration::from_secs(60), "default");

        let log = job.log.as_ref().expect("log");
        assert_eq!(log.stale_after, Duration::from_secs(600));
        let regex = log.error_regex.as_ref().expect("regex");
        assert!(regex.is_match("Traceback (most recent call last):"));
        assert!(regex.is_match("FATAL: out of memory"));
        assert!(!regex.is_match("wrote 400 rows"));

        let artifact = job.artifact.as_ref().expect("artifact");
        assert_eq!(artifact.stale_after, Duration::from_secs(26 * 3_600));
        assert_eq!(artifact.min_bytes, 1_024);
    }

    /// A job that sends no heartbeats cannot be judged on missing them. Catching
    /// it at startup beats a permanent alert five minutes after deploy.
    #[test]
    fn a_passive_job_may_not_declare_a_liveness_rule() {
        let err = Config::from_toml(
            r#"
[[job]]
name = "clients-etl"
mode = "passive"
  [job.alive]
  expect_every = "60s"
  [job.process]
  pidfile = "/var/run/etl.pid"
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(err.to_string().contains("fire forever"), "{err}");
    }

    #[test]
    fn a_passive_job_with_nothing_to_watch_is_rejected() {
        let err = Config::from_toml(
            r#"
[[job]]
name = "clients-etl"
mode = "passive"
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(err.to_string().contains("nothing is watching it"), "{err}");
    }

    /// Watching a job's output does not stop it also reporting in. A job that
    /// beats *and* has its export watched is a perfectly good arrangement.
    #[test]
    fn an_active_job_may_still_have_its_artifact_watched() {
        let config = load(
            r#"
[[job]]
name = "nightly-sync"
  [job.alive]
  expect_every = "60s"
  [job.artifact]
  path        = "/data/exports/daily.csv"
  stale_after = "26h"
"#,
        );

        let job = &config.jobs[0];
        assert_eq!(job.mode, JobMode::Active);
        assert!(job.alive.is_some());
        assert!(job.artifact.is_some());
        assert_eq!(job.artifact.as_ref().expect("artifact").min_bytes, 0);
    }

    #[test]
    fn an_uncompilable_error_regex_is_a_startup_error_naming_it() {
        let err = Config::from_toml(
            r#"
[[job]]
name = "clients-etl"
  [job.log]
  path        = "/var/log/etl.log"
  stale_after = "10m"
  error_regex = "(unclosed"
"#,
            &no_env(),
        )
        .expect_err("should fail");

        assert!(
            matches!(&err, ConfigError::InvalidRegex { pattern, .. } if pattern == "(unclosed"),
            "{err}"
        );
    }

    #[test]
    fn an_unknown_mode_is_rejected() {
        let err = Config::from_toml("[[job]]\nname = \"x\"\nmode = \"observant\"\n", &no_env())
            .expect_err("should fail");

        assert!(err.to_string().contains("observant"), "{err}");
    }

    #[test]
    fn a_zero_stale_after_is_rejected() {
        for block in ["log", "artifact"] {
            let text = format!(
                "[[job]]\nname = \"x\"\n  [job.{block}]\n  path = \"/tmp/x\"\n  \
                 stale_after = \"0s\"\n"
            );
            let err = Config::from_toml(&text, &no_env()).expect_err("should fail");
            assert!(err.to_string().contains("stale_after"), "{block}: {err}");
        }
    }

    // -- checks ------------------------------------------------------------

    fn one_check(body: &str) -> Result<CheckConfig, ConfigError> {
        let text = format!("[[check]]\n{body}\n");
        Config::from_toml(&text, &no_env()).map(|mut c| c.checks.remove(0))
    }

    #[test]
    fn a_minimal_check_gets_sensible_defaults() {
        let check = one_check(
            r#"
name = "vendor-api"
url  = "https://api.vendor.com/health"
"#,
        )
        .expect("check should load");

        assert_eq!(check.interval, Duration::from_secs(30));
        assert_eq!(check.timeout, Duration::from_secs(5));
        assert_eq!(check.down_after, Duration::from_secs(60), "two intervals");
        assert!(
            check.degradation.is_none(),
            "no [check.degradation] block means up/down only, never a latency verdict"
        );
        assert_eq!(check.probe.kind(), "http");
    }

    #[test]
    fn the_spec_example_check_resolves() {
        let check = one_check(
            r#"
name     = "vendor-api"
type     = "http"
url      = "https://api.vendor.com/health"
interval = "30s"
timeout  = "3s"

  [check.degradation]
  baseline_window   = "1h"
  warn_multiple     = 3.0
  critical_multiple = 8.0
  absolute_ceiling  = "2s"
"#,
        )
        .expect("check should load");

        let degradation = check.degradation.expect("degradation");
        assert_eq!(degradation.baseline_window, Duration::from_secs(3_600));
        assert_eq!(degradation.absolute_ceiling, Duration::from_secs(2));
        assert_eq!(degradation.min_samples, 30);
        // Twenty probes at 30s, which is under a third of the hour.
        assert_eq!(degradation.recent_window, Duration::from_secs(600));
    }

    #[test]
    fn the_recent_window_never_eats_more_than_a_third_of_the_baseline() {
        let check = one_check(
            r#"
name     = "slow-rpc"
url      = "https://rpc.example.com"
interval = "60s"

  [check.degradation]
  baseline_window   = "30m"
  warn_multiple     = 3.0
  critical_multiple = 8.0
  absolute_ceiling  = "2s"
  min_samples       = 10
"#,
        )
        .expect("check should load");

        let degradation = check.degradation.expect("degradation");
        // 20 x 60s = 20m would be two thirds of the window, so it is capped.
        assert_eq!(degradation.recent_window, Duration::from_secs(600));
    }

    #[test]
    fn a_jsonrpc_check_needs_a_method() {
        let err = one_check(
            r#"
name = "chain-rpc"
type = "jsonrpc"
url  = "https://rpc.example.com"
"#,
        )
        .expect_err("should fail");

        assert!(err.to_string().contains("method"), "{err}");
    }

    #[test]
    fn a_jsonrpc_check_with_a_method_resolves() {
        let check = one_check(
            r#"
name   = "chain-rpc"
type   = "jsonrpc"
url    = "https://rpc.example.com"
method = "eth_blockNumber"
"#,
        )
        .expect("check should load");

        assert_eq!(check.probe.kind(), "jsonrpc");
        assert!(matches!(
            &check.probe,
            ProbeConfig::JsonRpc { method, .. } if method == "eth_blockNumber"
        ));
    }

    #[test]
    fn a_method_on_an_http_check_is_rejected_rather_than_ignored() {
        let err = one_check(
            r#"
name   = "vendor-api"
url    = "https://api.vendor.com/health"
method = "eth_blockNumber"
"#,
        )
        .expect_err("should fail");

        assert!(err.to_string().contains("jsonrpc"), "{err}");
    }

    #[test]
    fn an_unknown_check_type_is_rejected() {
        let err = one_check(
            r#"
name = "vendor-api"
type = "grpc"
url  = "https://api.vendor.com/health"
"#,
        )
        .expect_err("should fail");

        assert!(matches!(&err, ConfigError::UnknownCheckType { kind, .. } if kind == "grpc"));
    }

    #[test]
    fn an_unparseable_url_is_a_startup_error() {
        let err = one_check(
            r#"
name = "vendor-api"
url  = "not a url"
"#,
        )
        .expect_err("should fail");

        assert!(matches!(err, ConfigError::InvalidCheckUrl { .. }));
    }

    #[test]
    fn a_timeout_longer_than_the_interval_is_rejected() {
        let err = one_check(
            r#"
name     = "vendor-api"
url      = "https://api.vendor.com/health"
interval = "5s"
timeout  = "10s"
"#,
        )
        .expect_err("should fail");

        assert!(err.to_string().contains("timeout"), "{err}");
    }

    /// A check whose window cannot physically hold `min_samples` would sit in
    /// "warming up" forever, judging nothing and quietly looking fine. That is
    /// the cold-start failure, and it is catchable at startup.
    #[test]
    fn a_baseline_window_too_short_to_ever_warm_up_is_rejected() {
        let err = one_check(
            r#"
name     = "vendor-api"
url      = "https://api.vendor.com/health"
interval = "30s"

  [check.degradation]
  baseline_window   = "5m"
  warn_multiple     = 3.0
  critical_multiple = 8.0
  absolute_ceiling  = "2s"
"#,
        )
        .expect_err("should fail");

        assert!(err.to_string().contains("never finish warming up"), "{err}");
        assert!(err.to_string().contains("min_samples"), "{err}");
    }

    #[test]
    fn multiples_must_be_ordered_and_above_one() {
        let too_small = one_check(
            r#"
name = "vendor-api"
url  = "https://api.vendor.com/health"
  [check.degradation]
  baseline_window   = "1h"
  warn_multiple     = 1.0
  critical_multiple = 8.0
  absolute_ceiling  = "2s"
"#,
        )
        .expect_err("should fail");
        assert!(
            too_small.to_string().contains("warn_multiple"),
            "{too_small}"
        );

        let inverted = one_check(
            r#"
name = "vendor-api"
url  = "https://api.vendor.com/health"
  [check.degradation]
  baseline_window   = "1h"
  warn_multiple     = 8.0
  critical_multiple = 3.0
  absolute_ceiling  = "2s"
"#,
        )
        .expect_err("should fail");
        assert!(
            inverted.to_string().contains("critical_multiple"),
            "{inverted}"
        );
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

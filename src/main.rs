use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use clap::Parser;
use stillwatch::config::{self, Config, ConfigError, SystemEnv};
use stillwatch::evaluate::{check_health, evaluate, unjudged_signals, CheckHealth, UnjudgedSignal};
use stillwatch::notify::{Dispatcher, DryRun, LogOnly, Notifier, Telegram};
use stillwatch::state::{SharedState, State};
use stillwatch::{fmt, learn, prober, receiver};
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;

/// How often the evaluator runs.
///
/// Not configurable: thresholds are minutes and the tick only bounds how late
/// an alert can be, so there is nothing here worth a knob.
const TICK: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(
    name = "stillwatch",
    version,
    about = "A watchdog for processes that run unattended."
)]
struct Cli {
    /// Path to the config file. Defaults to $STILLWATCH_CONFIG, then ./stillwatch.toml
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Evaluate and log what would have been sent, without sending anything
    #[arg(long)]
    dry_run: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Watch without alerting, then print thresholds derived from what happened
    Learn {
        /// Only learn this job or check. Defaults to everything in the config.
        #[arg(long, value_name = "NAME")]
        job: Option<String>,

        /// How long to observe, e.g. 6h
        #[arg(long = "for", value_name = "DURATION")]
        window: String,
    },
}

#[derive(Debug, thiserror::Error)]
enum StartupError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("could not listen on {addr}")]
    Listen {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },

    #[error("could not build the http client used to probe dependencies")]
    Client(#[source] reqwest::Error),

    #[error("--for is not a duration I can read; try something like 6h or 90m")]
    Window {
        #[source]
        source: humantime::DurationError,
    },

    #[error("the receiver stopped unexpectedly")]
    Serve(#[source] io::Error),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            report(&err);
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), StartupError> {
    let env = SystemEnv;
    let path = config::resolve_path(cli.config, &env);

    tracing::info!(path = %path.display(), "loading config");
    let config = Config::load(&path, &env)?;

    match cli.command {
        Some(Command::Learn { job, window }) => {
            let window = humantime::parse_duration(&window)
                .map_err(|source| StartupError::Window { source })?;
            learn_mode(config, job, window).await
        }
        None => watch_mode(config, cli.dry_run).await,
    }
}

/// Binds the receiver and starts one prober per check.
///
/// Shared by both modes on purpose: `learn` must observe through exactly the
/// same path the daemon does, or the thresholds it derives will not match what
/// the evaluator later judges against.
async fn observe(
    config: &Config,
    state: &SharedState,
) -> Result<(tokio::net::TcpListener, Vec<tokio::task::JoinHandle<()>>), StartupError> {
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .map_err(|source| StartupError::Listen {
            addr: config.listen,
            source,
        })?;
    tracing::info!(listen = %config.listen, "receiver ready");

    // One shared client so connection pooling is real: a fresh client per probe
    // would measure TLS handshakes rather than the dependency.
    let client = reqwest::Client::builder()
        .user_agent(concat!("stillwatch/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(StartupError::Client)?;

    let probers = config
        .checks
        .iter()
        .map(|check| tokio::spawn(prober::run(check.clone(), client.clone(), state.clone())))
        .collect();

    Ok((listener, probers))
}

async fn watch_mode(config: Config, dry_run: bool) -> Result<(), StartupError> {
    describe(&config);

    let notifier: Arc<dyn Notifier> = if dry_run {
        tracing::warn!(
            "dry run: everything will be evaluated and logged, and nothing will be \
             delivered anywhere"
        );
        Arc::new(DryRun)
    } else {
        match &config.telegram {
            Some(telegram) => Arc::new(Telegram::new(telegram)),
            None => Arc::new(LogOnly),
        }
    };
    tracing::info!(channel = notifier.channel(), "notifier ready");

    let state = SharedState::new(State::new(SystemTime::now(), &config.jobs, &config.checks));
    let (listener, probers) = observe(&config, &state).await?;

    let evaluator = tokio::spawn(watch(state.clone(), Dispatcher::new(notifier)));

    let served = axum::serve(listener, receiver::router(state))
        .with_graceful_shutdown(shutdown())
        .await;

    evaluator.abort();
    for prober in probers {
        prober.abort();
    }
    served.map_err(StartupError::Serve)
}

/// Watches without judging, then writes down what it saw.
///
/// No evaluator and no notifier are started at all — not a notifier that
/// discards, none. `learn` is meant to be safe to point at production before
/// anyone trusts the thresholds, and the way to be sure it sends nothing is for
/// there to be nothing that could.
async fn learn_mode(
    config: Config,
    only: Option<String>,
    window: Duration,
) -> Result<(), StartupError> {
    let state = SharedState::new(State::new(SystemTime::now(), &config.jobs, &config.checks));
    state.start_journal();

    let (listener, probers) = observe(&config, &state).await?;

    tracing::info!(
        window = %fmt::duration(window),
        job = only.as_deref().unwrap_or("<everything>"),
        "learning: observing only, nothing will be evaluated or sent"
    );

    let served = axum::serve(listener, receiver::router(state.clone()))
        .with_graceful_shutdown(observation_window(window))
        .await;

    for prober in probers {
        prober.abort();
    }
    served.map_err(StartupError::Serve)?;

    let now = SystemTime::now();
    let report = state.read(|state| match state.journal() {
        Some(journal) => learn::report(journal, &config, only.as_deref(), now),
        None => String::new(),
    });

    // The config block goes to stdout so it can be redirected straight into a
    // file; everything else has been going to the log all along.
    println!("{report}");
    Ok(())
}

/// Ends when the window elapses, or sooner on ctrl-c.
async fn observation_window(window: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(window) => {
            tracing::info!("observation window complete");
        }
        _ = shutdown() => {
            tracing::warn!(
                "stopped early: the numbers below come from a shorter window than asked for"
            );
        }
    }
}

/// Evaluates every job on a fixed tick and hands whatever is wrong to the
/// dispatcher.
///
/// The system clock is read exactly here and nowhere below: `evaluate` is given
/// the time rather than reading it, which is what makes it testable without
/// sleeping.
async fn watch(state: SharedState, mut dispatcher: Dispatcher) {
    let mut ticker = tokio::time::interval(TICK);
    // If the machine suspends, catching up on every missed tick would fire a
    // burst of identical evaluations. One late tick is enough.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut reported: BTreeMap<String, CheckHealth> = BTreeMap::new();
    let mut reported_unjudged: BTreeSet<(String, String)> = BTreeSet::new();

    loop {
        ticker.tick().await;
        let now = SystemTime::now();

        // The lock is released before any awaiting happens.
        let (assessments, health, unjudged) = state.read(|state| {
            (
                evaluate(state, now),
                check_health(state, now),
                unjudged_signals(state, now),
            )
        });

        report_check_health(&mut reported, health);
        report_unjudged(&mut reported_unjudged, unjudged);
        dispatcher.dispatch(&assessments, now).await;
    }
}

/// Logs each job rule that is configured but reaching no conclusion.
///
/// None of these is an incident and none of them pages. They are here because a
/// rule that is silently not judging anything looks exactly like a rule that is
/// passing, and a reader has to be able to tell the two apart without reading
/// the source.
fn report_unjudged(reported: &mut BTreeSet<(String, String)>, current: Vec<UnjudgedSignal>) {
    let now: BTreeSet<(String, String)> = current
        .iter()
        .map(|signal| (signal.subject.clone(), signal.signal.clone()))
        .collect();

    for signal in &current {
        let key = (signal.subject.clone(), signal.signal.clone());
        if reported.contains(&key) {
            continue;
        }
        tracing::info!(
            job = %signal.subject,
            rule = %signal.signal,
            why = %signal.why.describe(),
            "configured but not judging anything yet"
        );
    }

    for (subject, signal) in reported.difference(&now) {
        tracing::info!(job = %subject, rule = %signal, "now being judged");
    }

    *reported = now;
}

/// Logs each check's verdict basis whenever it changes.
///
/// Not every one of these is an alert, and most should not be: a check that is
/// still warming up is not an incident. But whether a check is *being judged* is
/// something a reader has to be able to find out, so every transition is stated
/// once rather than left to be inferred from silence.
fn report_check_health(
    reported: &mut BTreeMap<String, CheckHealth>,
    current: Vec<(String, CheckHealth)>,
) {
    for (name, health) in current {
        if reported.get(&name) == Some(&health) {
            continue;
        }

        match &health {
            CheckHealth::NotJudged(why) => tracing::info!(
                check = %name,
                why = %why.describe(),
                "not judged on latency yet; the ceiling still applies"
            ),
            CheckHealth::Ok => tracing::info!(check = %name, "ok"),
            CheckHealth::OkWithStaleBaseline {
                baseline_p90,
                recent_p90,
            } => tracing::warn!(
                check = %name,
                baseline = %fmt::latency(*baseline_p90),
                current = %fmt::latency(*recent_p90),
                "responding well, but its baseline was learned during a slower stretch \
                 and will not fire until the window rolls over"
            ),
            CheckHealth::Degraded => tracing::warn!(check = %name, "degraded"),
            CheckHealth::Down => tracing::error!(check = %name, "down"),
        }

        reported.insert(name, health);
    }
}

/// Says out loud what is and is not being watched, so a misconfiguration is
/// visible at startup rather than discovered during the outage it missed.
fn describe(config: &Config) {
    if config.telegram.is_none() {
        tracing::warn!("no notifier configured; alerts will only be logged");
    }

    if config.jobs.is_empty() && config.checks.is_empty() {
        tracing::warn!("no jobs and no checks configured; nothing is being watched");
    }

    for job in &config.jobs {
        match &job.alive {
            Some(alive) => tracing::info!(
                job = %job.name,
                expect_every = %fmt::duration(alive.expect_every),
                warn_after = %fmt::duration(alive.warn_after),
                critical_after = %fmt::duration(alive.critical_after),
                "watching liveness"
            ),
            None => tracing::info!(
                job = %job.name,
                "no [job.alive] block; not watching for missed beats"
            ),
        }

        if let Some(worked) = &job.worked {
            tracing::info!(
                job = %job.name,
                warn_after = %fmt::duration(worked.warn_after),
                critical_after = worked.critical_after.map(fmt::duration),
                capped_at_warn = job.alive.is_some(),
                "watching for real work; a job that is alive and quiet is never a page"
            );
        }

        if let Some(freshness) = &job.freshness {
            tracing::info!(
                job = %job.name,
                warn_after = %fmt::duration(freshness.warn_after),
                critical_after = freshness.critical_after.map(fmt::duration),
                "watching data freshness; beats must carry data_ts for this to judge anything"
            );
        }

        for ratio in &job.ratios {
            tracing::info!(
                job = %job.name,
                rule = %ratio.name,
                counters = %format!("{} / {}", ratio.numerator, ratio.denominator),
                window = %fmt::duration(ratio.window),
                min = %fmt::percent(ratio.min),
                min_sample = ratio.min_sample,
                "watching a counter ratio"
            );
        }
    }

    for check in &config.checks {
        tracing::info!(
            check = %check.name,
            kind = check.probe.kind(),
            url = %check.probe.url(),
            interval = %fmt::duration(check.interval),
            down_after = %fmt::duration(check.down_after),
            "probing"
        );

        match &check.degradation {
            // Says up front how long this check will go before it can judge
            // anything on latency, so nobody has to guess whether silence means
            // healthy or means not-yet-watching.
            Some(degradation) => tracing::info!(
                check = %check.name,
                ceiling = %fmt::latency(degradation.absolute_ceiling),
                warn_multiple = degradation.warn_multiple,
                critical_multiple = degradation.critical_multiple,
                baseline_after = %fmt::duration(check.interval * degradation.min_samples as u32),
                "the ceiling applies immediately; the multiples wait for a baseline"
            ),
            None => tracing::info!(
                check = %check.name,
                "no [check.degradation] block; watching up/down only, never latency"
            ),
        }
    }
}

async fn shutdown() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!("shutting down"),
        Err(err) => tracing::error!(%err, "could not listen for ctrl-c; running until killed"),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("stillwatch=info,warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Prints an error and everything underneath it. A watchdog that fails to start
/// with a one-line message is a watchdog nobody can fix.
fn report(err: &dyn Error) {
    eprintln!("stillwatch: {err}");
    let mut source = err.source();
    while let Some(cause) = source {
        eprintln!("  caused by: {cause}");
        source = cause.source();
    }
}

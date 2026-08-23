use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use clap::Parser;
use stillwatch::config::{self, Config, ConfigError, SystemEnv};
use stillwatch::evaluate::evaluate;
use stillwatch::notify::{Dispatcher, LogOnly, Notifier, Telegram};
use stillwatch::state::{SharedState, State};
use stillwatch::{fmt, receiver};
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
    describe(&config);

    let state = SharedState::new(State::new(SystemTime::now(), &config.jobs));

    let notifier: Arc<dyn Notifier> = match &config.telegram {
        Some(telegram) => Arc::new(Telegram::new(telegram)),
        None => Arc::new(LogOnly),
    };
    tracing::info!(channel = notifier.channel(), "notifier ready");

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .map_err(|source| StartupError::Listen {
            addr: config.listen,
            source,
        })?;

    tracing::info!(listen = %config.listen, "receiver ready");

    let evaluator = tokio::spawn(watch(state.clone(), Dispatcher::new(notifier)));

    let served = axum::serve(listener, receiver::router(state))
        .with_graceful_shutdown(shutdown())
        .await;

    evaluator.abort();
    served.map_err(StartupError::Serve)
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

    loop {
        ticker.tick().await;
        let now = SystemTime::now();

        // The lock is released before any awaiting happens.
        let assessments = state.read(|state| evaluate(state, now));

        dispatcher.dispatch(&assessments, now).await;
    }
}

/// Says out loud what is and is not being watched, so a misconfiguration is
/// visible at startup rather than discovered during the outage it missed.
fn describe(config: &Config) {
    if config.telegram.is_none() {
        tracing::warn!("no notifier configured; alerts will only be logged");
    }

    if config.jobs.is_empty() {
        tracing::warn!("no jobs configured; nothing is being watched");
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

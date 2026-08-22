use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::SystemTime;

use clap::Parser;
use stillwatch::config::{self, Config, ConfigError, SystemEnv};
use stillwatch::state::{SharedState, State};
use stillwatch::{fmt, receiver};
use tracing_subscriber::EnvFilter;

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

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .map_err(|source| StartupError::Listen {
            addr: config.listen,
            source,
        })?;

    tracing::info!(listen = %config.listen, "receiver ready");

    axum::serve(listener, receiver::router(state))
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(StartupError::Serve)
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

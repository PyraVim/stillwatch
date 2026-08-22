use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use stillwatch::config::{self, Config, ConfigError, SystemEnv};
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

async fn run(cli: Cli) -> Result<(), ConfigError> {
    let env = SystemEnv;
    let path = config::resolve_path(cli.config, &env);

    tracing::info!(path = %path.display(), "loading config");
    let config = Config::load(&path, &env)?;

    describe(&config);
    Ok(())
}

fn describe(config: &Config) {
    if config.telegram.is_none() {
        tracing::warn!("no notifier configured; alerts will only be logged");
    }

    for job in &config.jobs {
        match &job.alive {
            Some(alive) => tracing::info!(
                job = %job.name,
                expect_every = %stillwatch::fmt::duration(alive.expect_every),
                warn_after = %stillwatch::fmt::duration(alive.warn_after),
                critical_after = %stillwatch::fmt::duration(alive.critical_after),
                "watching liveness"
            ),
            None => {
                tracing::info!(job = %job.name, "no liveness rule; not watching for missed beats")
            }
        }
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

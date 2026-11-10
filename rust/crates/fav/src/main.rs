//! `fav`: one binary for the API, the worker, or both.
//!
//! Commands arrive with their work packages (`rust/docs/PLAN.md`): `worker` and
//! `trigger` in WP2 (`python run.py` and `python run.py --trigger <target>`), `api` and
//! `serve` (the default) in WP3.

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, LazyLock};

use clap::{Parser, Subcommand};
use fav_core::env::Env;
use fav_core::logging::{self, Level};
use fav_db::{Db, SettingsStore};
use fav_runtime::telegram_bot::BotClient;
use fav_runtime::worker::{self, WorkerError, WorkerOptions};

static VERSION: LazyLock<String> = LazyLock::new(version);

#[derive(Debug, Parser)]
#[command(name = "fav", version = VERSION.as_str(), arg_required_else_help = true, about = "Crawls content sources on a schedule and archives their media")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the worker: scheduler, manual triggers, notification delivery, settings watcher.
    Worker,
    /// Run one job immediately and exit. Use a job key or "all".
    Trigger {
        /// A job key, or "all" for every enabled job.
        target: String,
    },
}

fn version() -> String {
    format!("{} (python baseline {})", env!("CARGO_PKG_VERSION"), fav_core::BASELINE_COMMIT)
}

/// A line on stderr, as Python prints an uncaught `SystemExit` or error.
fn report(message: &str) {
    // Nothing useful is left to do when stderr itself is gone.
    let _ = writeln!(std::io::stderr(), "{message}");
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            report(&format!("cannot start the async runtime: {error}"));
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run(cli.command))
}

/// The pieces every command needs, from the environment.
async fn options() -> Result<WorkerOptions, String> {
    let env = Env::from_process().map_err(|error| error.to_string())?;
    let level = Level::from_name(&env.log_level).unwrap_or(Level::Info);
    logging::init(level, Path::new("log")).map_err(|error| format!("cannot open the log file: {error}"))?;
    let db = Db::connect_lazy(&env.postgres_dsn).map_err(|error| error.to_string())?;
    db.ensure_schema().await.map_err(|error| error.to_string())?;
    let bot = BotClient::new().map_err(|error| format!("cannot build the Telegram client: {error}"))?;
    Ok(WorkerOptions {
        settings: Arc::new(SettingsStore::new(db.clone())),
        db,
        registry: fav_sources::registry(),
        bot,
        timezone: worker::resolve_scheduler_timezone(std::env::var("TZ").ok().as_deref()),
    })
}

async fn run(command: Command) -> ExitCode {
    let options = match options().await {
        Ok(options) => options,
        Err(message) => {
            report(&message);
            return ExitCode::FAILURE;
        }
    };
    let (stop, signal) = fav_runtime::stop_signal();
    if let Err(error) = worker::install_signal_handlers(stop.clone()) {
        report(&format!("cannot install the signal handlers: {error}"));
        return ExitCode::FAILURE;
    }
    let result = match command {
        Command::Worker => worker::run_worker(options, stop).await,
        Command::Trigger { target } => worker::run_trigger(options, &target, signal).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // `SystemExit(message)`: the message on stderr, status 1.
        Err(WorkerError::Exit(message)) => {
            if let Some(message) = message {
                report(&message);
            }
            ExitCode::FAILURE
        }
        Err(WorkerError::Failed(message)) => {
            logging::get("main").critical(format!("RuntimeError: {message}"));
            report(&format!("RuntimeError: {message}"));
            ExitCode::FAILURE
        }
        Err(WorkerError::Store(error)) => {
            logging::get("main").critical(&error);
            report(&error.to_string());
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser as _};

    use super::{Cli, Command};

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_names_the_baseline() {
        let version = super::version();
        assert!(version.contains(fav_core::BASELINE_COMMIT), "{version}");
    }

    #[test]
    fn worker_and_trigger_parse() {
        assert!(matches!(Cli::parse_from(["fav", "worker"]).command, Command::Worker));
        assert!(matches!(
            Cli::parse_from(["fav", "trigger", "all"]).command,
            Command::Trigger { target } if target == "all"
        ));
        assert!(Cli::try_parse_from(["fav", "trigger"]).is_err());
    }
}

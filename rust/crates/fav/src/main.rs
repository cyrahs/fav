//! `fav`: one binary for the API, the worker, or both.
//!
//! Commands arrive with their work packages (`rust/docs/PLAN.md`): `worker` and
//! `trigger` in WP2 (`python run.py` and `python run.py --trigger <target>`), `api`
//! (`python -m src.api`) and `serve` (`python -m src.service`, the default) in WP3.

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, LazyLock};

use clap::{Parser, Subcommand};
use fav_api::mcp::{McpState, WorkerProcess};
use fav_api::{ApiConfig, ApiState};
use fav_core::env::Env;
use fav_core::logging::{self, Level};
use fav_db::{Db, SettingsStore};
use fav_runtime::telegram_bot::BotClient;
use fav_runtime::worker::{self, WorkerError, WorkerOptions};
use fav_runtime::{StopHandle, StopSignal};

static VERSION: LazyLock<String> = LazyLock::new(version);

/// Where the log files go (`./log`), which the MCP `read_logs` tool reads back.
const LOG_DIR: &str = "log";

#[derive(Debug, Parser)]
#[command(name = "fav", version = VERSION.as_str(), about = "Crawls content sources on a schedule and archives their media")]
struct Cli {
    /// What to run; `serve` when left out.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the API and the worker in one process (the default).
    Serve,
    /// Run the HTTP API only: the web UI, `/api/v2`, credentials and health.
    Api,
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
    runtime.block_on(run(cli.command.unwrap_or(Command::Serve)))
}

/// What every command shares, from the environment.
struct Base {
    env: Env,
    db: Db,
    settings: Arc<SettingsStore>,
    bot: BotClient,
}

async fn base() -> Result<Base, String> {
    let env = Env::from_process().map_err(|error| error.to_string())?;
    let level = Level::from_name(&env.log_level).unwrap_or(Level::Info);
    logging::init(level, Path::new(LOG_DIR)).map_err(|error| format!("cannot open the log file: {error}"))?;
    let db = Db::connect_lazy(&env.postgres_dsn).map_err(|error| error.to_string())?;
    db.ensure_schema().await.map_err(|error| error.to_string())?;
    let bot = BotClient::new().map_err(|error| format!("cannot build the Telegram client: {error}"))?;
    Ok(Base {
        settings: Arc::new(SettingsStore::new(db.clone())),
        env,
        db,
        bot,
    })
}

impl Base {
    fn worker_options(&self) -> WorkerOptions {
        WorkerOptions {
            settings: Arc::clone(&self.settings),
            db: self.db.clone(),
            registry: fav_sources::registry(),
            bot: self.bot.clone(),
            timezone: worker::resolve_scheduler_timezone(std::env::var("TZ").ok().as_deref()),
        }
    }

    /// `load_config_from_env()` and `create_app(config=...)`. The front end is
    /// `web/dist` under the working directory, as in the image (`/app/web/dist`).
    fn api(&self, worker: WorkerProcess) -> Result<(ApiConfig, Arc<ApiState>), WorkerError> {
        let config = fav_api::config::load_config_from_env(&self.env).map_err(|message| WorkerError::Exit(Some(message)))?;
        let web_dist = fav_api::find_dist(Path::new("web/dist"));
        let log_dir = std::fs::canonicalize(LOG_DIR)
            .or_else(|_| std::path::absolute(LOG_DIR))
            .unwrap_or_else(|_| LOG_DIR.into());
        let mcp = McpState::new(
            &config.token,
            &self.env.postgres_dsn,
            log_dir,
            std::env::var("TZ").ok().as_deref(),
            worker,
        );
        let state = ApiState::new(
            &config,
            self.db.clone(),
            Arc::clone(&self.settings),
            self.bot.clone(),
            web_dist,
            mcp,
        )
        .map_err(|message| WorkerError::Exit(Some(message)))?;
        Ok((config, Arc::new(state)))
    }
}

/// `python -m src.api`: serve until a stop signal.
async fn run_api(config: &ApiConfig, state: Arc<ApiState>, signal: StopSignal) -> Result<(), WorkerError> {
    fav_api::serve(state, &config.bind, config.port, async move { signal.wait().await })
        .await
        .map_err(|error| WorkerError::Exit(Some(format!("cannot serve the API on {}:{}: {error}", config.bind, config.port))))
}

/// `python -m src.service`: the worker and the API side by side. When either ends, the
/// other is stopped, as the Python launcher does with its two processes.
async fn run_serve(base: &Base, stop: StopHandle) -> Result<(), WorkerError> {
    let log = logging::get("service");
    log.info("Starting services (worker + api)");
    let (config, state) = base.api(WorkerProcess::InProcess)?;
    let api = async {
        let result = run_api(&config, state, stop.signal()).await;
        ended(&stop, "api", &result);
        result
    };
    let worker = async {
        let result = worker::run_worker(base.worker_options(), stop.clone()).await;
        ended(&stop, "worker", &result);
        result
    };
    let (api, worker) = tokio::join!(api, worker);
    worker.and(api)
}

/// One half of `serve` ended: unless a stop was asked for, say so and stop the other.
fn ended(stop: &StopHandle, name: &str, result: &Result<(), WorkerError>) {
    if stop.is_stopped() {
        return;
    }
    let log = logging::get("service");
    match result {
        Ok(()) => log.warning(format!("{name} exited with code 0, shutting down remaining services")),
        Err(_) => log.error(format!("{name} exited with code 1, shutting down remaining services")),
    }
    stop.stop();
}

async fn run(command: Command) -> ExitCode {
    let base = match base().await {
        Ok(base) => base,
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
        Command::Serve => run_serve(&base, stop).await,
        Command::Api => match base.api(WorkerProcess::Separate) {
            Ok((config, state)) => run_api(&config, state, signal).await,
            Err(error) => Err(error),
        },
        Command::Worker => worker::run_worker(base.worker_options(), stop).await,
        Command::Trigger { target } => worker::run_trigger(base.worker_options(), &target, signal).await,
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
    fn commands_parse_and_serve_is_the_default() {
        assert!(Cli::parse_from(["fav"]).command.is_none());
        assert!(matches!(Cli::parse_from(["fav", "serve"]).command, Some(Command::Serve)));
        assert!(matches!(Cli::parse_from(["fav", "api"]).command, Some(Command::Api)));
        assert!(matches!(Cli::parse_from(["fav", "worker"]).command, Some(Command::Worker)));
        assert!(matches!(
            Cli::parse_from(["fav", "trigger", "all"]).command,
            Some(Command::Trigger { target }) if target == "all"
        ));
        assert!(Cli::try_parse_from(["fav", "trigger"]).is_err());
    }
}

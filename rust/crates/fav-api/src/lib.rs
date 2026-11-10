//! The fav HTTP API.
//!
//! Python origin: `src/api` (`app.py`, `routes.py`, `service.py`, `schemas.py`,
//! `archive.py`, `credentials.py`, `settings_masking.py`, `config.py`, `helpers.py`). The
//! same paths, fields, status codes and error envelopes as the FastAPI app, checked against
//! Python's own responses (`rust/goldens/api/requests.json`) and its OpenAPI document
//! (`rust/goldens/api/openapi.json`).
//!
//! FastAPI's request handling is reproduced rather than approximated: routes are matched in
//! Starlette's order (a wrong method is a 405 naming the first route whose path matched, a
//! path that only differs by its trailing slash is redirected), the bearer-token dependency
//! runs before parameter validation, and parameters and bodies are validated with
//! pydantic's rules and error entries ([`params`]). axum serves the connection; the dispatch
//! itself is [`router`].
//!
//! Work-package scope (`rust/docs/PLAN.md`): WP3 ports the system, jobs, settings, archive,
//! site-session, credential and notification-test routes, the documents and the bundled
//! front end; WP4 the MCP endpoint ([`mcp`]). The source-specific routes arrive with their
//! sources; [`openapi::PENDING`] lists every route still missing and which package owns it.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use fav_db::{Db, SettingsStore};
use fav_runtime::telegram_bot::BotClient;
use fav_sources::kemono_creator::{KemonoCreatorResolver, ResolveCreator};

pub mod archive;
pub mod config;
pub mod cors;
pub mod credentials;
pub mod error;
pub mod masking;
pub mod mcp;
pub mod openapi;
pub mod params;
pub mod router;
pub mod site_logins;

mod handlers;
mod json;
mod web;

pub use config::ApiConfig;
pub use error::ApiError;
pub use router::app;
pub use web::find_dist;

/// The clock behind `generated_at` (`now_provider` in Python).
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Everything a request handler can reach.
pub struct ApiState {
    /// `API_TOKEN`: what the bearer token of every `/api/v2` route is compared with.
    pub token: String,
    pub db: Db,
    pub settings: Arc<SettingsStore>,
    /// Sends the notification test (`POST /api/v2/notifications/telegram/test`).
    pub bot: BotClient,
    /// Services allowed to read stored logins (`CREDENTIAL_CLIENTS`).
    pub credential_clients: Vec<credentials::CredentialClient>,
    /// `API_CORS_ORIGINS`; `None` installs no CORS handling at all, as in Python.
    pub cors: Option<cors::Cors>,
    /// The built front end (`web/dist`), when it has an `index.html`.
    pub web_dist: Option<PathBuf>,
    pub clock: Clock,
    /// What the MCP tools read beyond the rest: bootstrap secrets, logs, zone, worker.
    pub mcp: mcp::McpState,
    /// Resolves pasted Kemono creator URLs (`POST /api/v2/kemono/creators/resolve`).
    pub kemono_creators: Arc<dyn ResolveCreator>,
}

impl std::fmt::Debug for ApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiState")
            .field("token", &"<redacted>")
            .field("credential_clients", &self.credential_clients)
            .field("cors", &self.cors)
            .field("web_dist", &self.web_dist)
            .field("mcp", &self.mcp)
            .finish_non_exhaustive()
    }
}

impl ApiState {
    /// The state `create_app` builds from its configuration. A malformed
    /// `CREDENTIAL_CLIENTS` is an error, so the API does not start with it.
    pub fn new(
        config: &ApiConfig,
        db: Db,
        settings: Arc<SettingsStore>,
        bot: BotClient,
        web_dist: Option<PathBuf>,
        mcp: mcp::McpState,
    ) -> Result<Self, String> {
        Ok(Self {
            token: config.token.clone(),
            db,
            bot,
            credential_clients: credentials::parse_clients(&config.credential_clients)?,
            cors: cors::Cors::new(config.cors_origins.clone(), config.cors_allow_credentials),
            web_dist,
            clock: system_clock(),
            mcp,
            kemono_creators: Arc::new(KemonoCreatorResolver::new(Arc::clone(&settings))?),
            settings,
        })
    }
}

/// The wall clock.
pub fn system_clock() -> Clock {
    Arc::new(Utc::now)
}

/// How long open connections may take to finish after a stop, as the Python launcher
/// waits for its API process (`_SHUTDOWN_TIMEOUT_SECONDS`) before killing it.
pub const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Serve the API on `bind:port` until `shutdown` resolves (`uvicorn.run`), then give
/// open connections [`SHUTDOWN_GRACE`] to finish.
pub async fn serve(
    state: Arc<ApiState>,
    bind: &str,
    port: u16,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind((bind, port)).await?;
    fav_core::logging::get("fav-api").info(format!("Starting fav API on {bind}:{port}"));
    serve_listener(listener, state, shutdown, SHUTDOWN_GRACE).await
}

/// [`serve`] on a bound listener. A connection still open `grace` after the stop (a
/// client that never finishes its request) is dropped instead of holding the process.
pub(crate) async fn serve_listener(
    listener: tokio::net::TcpListener,
    state: Arc<ApiState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
    grace: std::time::Duration,
) -> std::io::Result<()> {
    let (stopped, mut stopped_rx) = tokio::sync::watch::channel(false);
    let graceful = async move {
        shutdown.await;
        stopped.send_replace(true);
    };
    let server = axum::serve(listener, app(state)).with_graceful_shutdown(graceful).into_future();
    let deadline = async move {
        if stopped_rx.wait_for(|stopped| *stopped).await.is_err() {
            // The server ended on its own; only its result matters.
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(grace).await;
    };
    tokio::select! {
        result = server => result,
        () = deadline => {
            fav_core::logging::get("service").warning(format!("api did not stop in {}s, killing", grace.as_secs()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;

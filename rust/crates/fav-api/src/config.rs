//! The API's settings from the environment.
//!
//! Python origin: `load_config_from_env` in `src/api/config.py` and `ApiConfig` in
//! `src/api/models.py`.

use fav_core::env::Env;

pub const DEFAULT_BIND: &str = "127.0.0.1";
const MAX_PORT: u32 = 65_535;

/// `ApiConfig`.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiConfig {
    pub dsn: String,
    pub token: String,
    pub bind: String,
    pub port: u16,
    pub cors_origins: Vec<String>,
    pub cors_allow_credentials: bool,
    pub credential_clients: String,
}

impl std::fmt::Debug for ApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiConfig")
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("cors_origins", &self.cors_origins)
            .field("cors_allow_credentials", &self.cors_allow_credentials)
            .finish_non_exhaustive()
    }
}

/// `load_config_from_env`: the bootstrap values, refused when one the API needs is empty.
pub fn load_config_from_env(env: &Env) -> Result<ApiConfig, String> {
    let dsn = fav_core::pytext::py_strip(&env.postgres_dsn).to_owned();
    let token = fav_core::pytext::py_strip(&env.api_token).to_owned();
    let bind = match fav_core::pytext::py_strip(&env.api_bind) {
        "" => DEFAULT_BIND.to_owned(),
        bind => bind.to_owned(),
    };
    if dsn.is_empty() {
        return Err("POSTGRES_DSN is required".to_owned());
    }
    if token.is_empty() {
        return Err("API_TOKEN is required".to_owned());
    }
    if env.api_port == 0 {
        return Err(format!("API_PORT must be between 1 and {MAX_PORT}"));
    }
    Ok(ApiConfig {
        dsn,
        token,
        bind,
        port: env.api_port,
        cors_origins: env.cors_origins(),
        cors_allow_credentials: env.api_cors_allow_credentials,
        credential_clients: env.credential_clients.clone(),
    })
}

#[cfg(test)]
mod tests {
    use fav_core::env::Env;

    use super::load_config_from_env;

    fn env(vars: &[(&str, &str)]) -> Env {
        Env::from_sources(vars.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())), None).expect("a valid environment")
    }

    // py: tests/test_api_server.py::test_load_config_from_env_reads_bootstrap_values
    #[test]
    fn load_config_from_env_reads_bootstrap_values() {
        let config = load_config_from_env(&env(&[
            ("POSTGRES_DSN", " postgresql://fav:pw@db/fav "),
            ("API_TOKEN", " secret-token "),
            ("API_BIND", "0.0.0.0"),
            ("API_PORT", "9000"),
        ]))
        .expect("valid");
        assert_eq!(config.dsn, "postgresql://fav:pw@db/fav");
        assert_eq!(config.token, "secret-token");
        assert_eq!(config.bind, "0.0.0.0");
        assert_eq!(config.port, 9000);
        assert!(config.cors_origins.is_empty());
        assert!(!config.cors_allow_credentials);
    }

    // py: tests/test_api_server.py::test_load_config_from_env_reads_cors_fields
    #[test]
    fn load_config_from_env_reads_cors_fields() {
        let config = load_config_from_env(&env(&[
            ("POSTGRES_DSN", "postgresql://fav:pw@db/fav"),
            ("API_TOKEN", "secret-token"),
            ("API_CORS_ORIGINS", "https://game-view.s117.me, http://localhost:5173"),
            ("API_CORS_ALLOW_CREDENTIALS", "true"),
        ]))
        .expect("valid");
        assert_eq!(config.cors_origins, ["https://game-view.s117.me", "http://localhost:5173"]);
        assert!(config.cors_allow_credentials);
    }

    // py: tests/test_api_server.py::test_load_config_from_env_rejects_empty_postgres_dsn
    #[test]
    fn load_config_from_env_rejects_empty_postgres_dsn() {
        // Python mocks `env`; the validated `Env` refuses an empty value itself.
        let mut env = env(&[("POSTGRES_DSN", "postgresql://db.local/fav"), ("API_TOKEN", "abc123")]);
        env.postgres_dsn = String::new();
        let error = load_config_from_env(&env).unwrap_err();
        assert_eq!(error, "POSTGRES_DSN is required");
    }

    // py: tests/test_api_server.py::test_load_config_from_env_rejects_empty_api_token
    #[test]
    fn load_config_from_env_rejects_empty_api_token() {
        let mut env = env(&[("POSTGRES_DSN", "postgresql://db.local/fav"), ("API_TOKEN", "abc123")]);
        env.api_token = String::new();
        let error = load_config_from_env(&env).unwrap_err();
        assert_eq!(error, "API_TOKEN is required");
    }
}

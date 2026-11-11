//! Signed-in sessions, served read-only to other services on the cluster.
//!
//! Python origin: `src/api/credentials.py`. Each client (`CREDENTIAL_CLIENTS`) has its own
//! bearer token, of which fav keeps only the SHA-256, and may read only the sites it is
//! configured for. The responses carry cookie values: they are never logged and are
//! marked `no-store`. `goldens/api/credential_clients.json` holds Python's parses.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest as _, Sha256};

use crate::error::ApiError;

static SHA256_HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{64}$").expect("valid pattern"));
static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").expect("valid pattern"));
static SEPARATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\s;]+").expect("valid pattern"));

const TOKEN_REQUIRED: &str = "A Bearer token is required.";

/// One configured client.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialClient {
    pub name: String,
    pub token_sha256: String,
    pub sites: BTreeSet<String>,
}

impl std::fmt::Debug for CredentialClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialClient")
            .field("name", &self.name)
            .field("sites", &self.sites)
            .finish_non_exhaustive()
    }
}

/// `parse_clients`: `name:sha256:site,site` entries separated by whitespace or `;`. A
/// malformed entry is an error, which stops the API from starting.
pub fn parse_clients(raw: &str) -> Result<Vec<CredentialClient>, String> {
    let mut clients = Vec::new();
    for entry in SEPARATOR.split(fav_core::pytext::py_strip(raw)) {
        if entry.is_empty() {
            continue;
        }
        let parts: Vec<&str> = entry.split(':').collect();
        let [name, digest, sites] = parts.as_slice() else {
            return Err("CREDENTIAL_CLIENTS entries must be name:sha256:site[,site...]".to_owned());
        };
        let digest = digest.to_lowercase();
        let sites: Vec<&str> = sites.split(',').filter(|site| !site.is_empty()).collect();
        if !NAME.is_match(name) || !SHA256_HEX.is_match(&digest) || sites.is_empty() || !sites.iter().all(|site| NAME.is_match(site)) {
            let shown = if name.is_empty() { "?" } else { name };
            return Err(format!("CREDENTIAL_CLIENTS entry for {shown} is malformed"));
        }
        clients.push(CredentialClient {
            name: (*name).to_owned(),
            token_sha256: digest,
            sites: sites.into_iter().map(str::to_owned).collect(),
        });
    }
    Ok(clients)
}

fn unauthorized(code: &str) -> ApiError {
    ApiError::unauthorized(code, TOKEN_REQUIRED)
}

/// Equal-time comparison of two hex digests.
/// The lowercase hex SHA-256 of a token (`hashlib.sha256(...).hexdigest()`).
pub(crate) fn sha256_hex(token: &str) -> String {
    use std::fmt::Write as _;
    Sha256::digest(token.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn digests_equal(left: &str, right: &str) -> bool {
    left.len() == right.len() && left.bytes().zip(right.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// `authenticate_client`: the client whose token this is.
pub fn authenticate_client<'a>(clients: &'a [CredentialClient], authorization: Option<&str>) -> Result<&'a CredentialClient, ApiError> {
    let Some(rest) = authorization
        .filter(|value| !value.is_empty())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return Err(unauthorized("missing_authorization"));
    };
    let token = fav_core::pytext::py_strip(rest);
    if token.is_empty() {
        return Err(unauthorized("missing_bearer_token"));
    }
    let digest = sha256_hex(token);
    // Every entry is compared, so the time taken does not say which one matched.
    let matched: Vec<&CredentialClient> = clients
        .iter()
        .filter(|client| digests_equal(&digest, &client.token_sha256))
        .collect();
    matched
        .first()
        .copied()
        .ok_or_else(|| ApiError::new(403, "invalid_token", "Bearer token is not valid."))
}

#[cfg(test)]
mod tests {
    use fav_core::golden;
    use fav_core::pyjson::Value;

    use super::parse_clients;

    fn as_value(clients: &[super::CredentialClient]) -> Value {
        Value::List(
            clients
                .iter()
                .map(|client| {
                    crate::json::dict([
                        ("name", Value::Str(client.name.clone())),
                        ("token_sha256", Value::Str(client.token_sha256.clone())),
                        ("sites", crate::json::str_list(&client.sites.iter().collect::<Vec<_>>())),
                    ])
                })
                .collect(),
        )
    }

    // py: tests/test_credentials_api.py::test_clients_parse_from_the_environment_value
    // py: tests/test_credentials_api.py::test_malformed_clients_are_refused
    #[test]
    fn clients_parse_like_python() {
        let document = golden::load("api/credential_clients.json");
        for case in golden::cases(&document) {
            let raw = golden::string(golden::get(case, "raw"));
            match (parse_clients(&raw), case.get("error")) {
                (Ok(clients), None) => assert_eq!(as_value(&clients), *golden::get(case, "clients"), "{raw:?}"),
                (Err(message), Some(expected)) => assert_eq!(message, golden::string(expected), "{raw:?}"),
                (other, expected) => panic!("{raw:?}: got {other:?}, expected {expected:?}"),
            }
        }
    }
}

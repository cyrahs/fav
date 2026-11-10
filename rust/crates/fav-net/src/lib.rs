//! Network plumbing shared by the sources: the one place HTTP clients are built.
//!
//! Python origin: every `httpx.AsyncClient(...)` in the tree. Keeping construction here
//! means the TLS backend (rustls with the `ring` provider, the same one sqlx uses) can be
//! swapped in one place if a site turns out to care about the TLS fingerprint
//! (`rust/docs/PLAN.md`, technology mapping). Like httpx, the clients honour
//! `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY` from the environment. Streamed downloads and
//! the subprocess runner arrive with the sources that need them.

use std::sync::Once;

pub use reqwest;

static CRYPTO_PROVIDER: Once = Once::new();

/// Install rustls' `ring` provider as the process default, once.
///
/// reqwest is built without a provider of its own so the binary carries one TLS stack;
/// it then uses the process default, which has to exist before the first client is built.
fn install_crypto_provider() {
    CRYPTO_PROVIDER.call_once(|| {
        // An error means another component installed one first, which is just as good.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A client builder with fav's TLS setup; callers add their timeouts.
pub fn client_builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder()
}

/// The httpx exception class a reqwest transport error stands in for.
///
/// Callers that report a failure by its class name (`Telegram Bot API request failed:
/// ConnectTimeout`, a job's `RuntimeError: ...`) use this so the text keeps httpx's names.
/// reqwest has no write timeout and does not tell a read failure from a protocol error, so
/// the mapping is approximate (DIVERGENCES.md D18).
pub fn httpx_error_class(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        if error.is_connect() { "ConnectTimeout" } else { "ReadTimeout" }
    } else if error.is_connect() {
        "ConnectError"
    } else if error.is_redirect() {
        "TooManyRedirects"
    } else if error.is_decode() {
        "DecodingError"
    } else if error.is_body() {
        "ReadError"
    } else if error.is_builder() {
        "UnsupportedProtocol"
    } else {
        "RemoteProtocolError"
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_client_can_be_built() {
        super::client_builder().build().expect("client builds with the ring provider");
    }

    #[tokio::test]
    async fn a_refused_connection_is_a_connect_error() {
        let client = super::client_builder().build().expect("client");
        // Port 1 on loopback refuses at once.
        let error = client.get("http://127.0.0.1:1/").send().await.expect_err("refused");
        assert_eq!(super::httpx_error_class(&error), "ConnectError");
    }
}

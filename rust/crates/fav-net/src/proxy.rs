//! httpx's rule for which proxy URLs it can dial.
//!
//! Python origin: `httpx.Proxy.__init__`, which raises `ValueError: Unknown scheme for proxy
//! URL ...` unless the scheme is `http`, `https`, `socks5` or `socks5h`. reqwest takes a
//! bare `host:port` (and other schemes) as an HTTP proxy, so every client that is given a
//! proxy setting checks it here first (DIVERGENCES.md D67, D113, D181). Folded in WP18 from
//! the copies in X, Pixiv, Azur Lane and the RedNote proxy probe.

/// The schemes httpx's `Proxy` takes.
pub const HTTPX_PROXY_SCHEMES: [&str; 4] = ["http", "https", "socks5", "socks5h"];

/// Whether httpx's `Proxy(proxy)` accepts the URL: its `urlsplit` scheme is one of
/// [`HTTPX_PROXY_SCHEMES`]. A bare `host:port` has no scheme (or `host` as one) and is refused.
pub fn is_httpx_proxy(proxy: &str) -> bool {
    let scheme = fav_core::pyurl::urlsplit(proxy, "").map(|split| split.scheme).unwrap_or_default();
    HTTPX_PROXY_SCHEMES.contains(&scheme.as_str())
}

#[cfg(test)]
mod tests {
    use super::is_httpx_proxy;

    #[test]
    fn httpx_takes_only_its_four_schemes() {
        for proxy in ["http://p:1", "https://p", "socks5://u:p@p:1080", "socks5h://p", "HTTP://p:1"] {
            assert!(is_httpx_proxy(proxy), "{proxy}");
        }
        for proxy in ["127.0.0.1:8080", "proxy.example:8080", "ftp://p", "socks4://p", "bogus", ""] {
            assert!(!is_httpx_proxy(proxy), "{proxy}");
        }
    }
}

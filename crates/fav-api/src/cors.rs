//! Cross-origin requests from the front ends served elsewhere (game-view, Vite in
//! development).
//!
//! Python origin: `create_app` installs Starlette's `CORSMiddleware` (0.52) only when
//! `API_CORS_ORIGINS` is set, with methods `DELETE, GET, POST, PUT, OPTIONS` and headers
//! `Authorization, Content-Type`. Preflights are answered here without reaching a route;
//! every other request with an `Origin` gets the simple-response headers added.

use axum::http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode, header};

use crate::web::latin1;

const ALLOW_METHODS: [&str; 5] = ["DELETE", "GET", "POST", "PUT", "OPTIONS"];
const CONFIGURED_HEADERS: [&str; 2] = ["Authorization", "Content-Type"];
const SAFELISTED_HEADERS: [&str; 4] = ["Accept", "Accept-Language", "Content-Language", "Content-Type"];
const MAX_AGE: &str = "600";

/// `CORSMiddleware` as `create_app` configures it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cors {
    origins: Vec<String>,
    allow_credentials: bool,
}

impl Cors {
    /// `None` when no origin is configured: Python then installs no middleware at all.
    pub fn new(origins: Vec<String>, allow_credentials: bool) -> Option<Self> {
        (!origins.is_empty()).then_some(Self {
            origins,
            allow_credentials,
        })
    }

    fn allow_all_origins(&self) -> bool {
        self.origins.iter().any(|origin| origin == "*")
    }

    fn is_allowed_origin(&self, origin: &str) -> bool {
        self.allow_all_origins() || self.origins.iter().any(|allowed| allowed == origin)
    }

    /// `sorted(SAFELISTED_HEADERS | set(allow_headers))`.
    fn allow_headers() -> Vec<&'static str> {
        let mut headers: Vec<&str> = SAFELISTED_HEADERS.iter().chain(CONFIGURED_HEADERS.iter()).copied().collect();
        headers.sort_unstable();
        headers.dedup();
        headers
    }

    /// The preflight answer, when this request is one (`OPTIONS` with an `Origin` and an
    /// `Access-Control-Request-Method`).
    pub fn preflight(&self, method: &str, headers: &HeaderMap) -> Option<Response<axum::body::Body>> {
        let origin = headers.get(header::ORIGIN)?;
        if method != "OPTIONS" {
            return None;
        }
        let requested_method = headers.get(header::ACCESS_CONTROL_REQUEST_METHOD)?;
        let origin = latin1(origin.as_bytes());
        let explicit_origin = !self.allow_all_origins() || self.allow_credentials;
        let mut out = HeaderMap::new();
        if explicit_origin {
            out.insert(header::VARY, HeaderValue::from_static("Origin"));
        } else {
            out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
        }
        out.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("DELETE, GET, POST, PUT, OPTIONS"),
        );
        out.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static(MAX_AGE));
        let allow_headers = Self::allow_headers();
        if let Ok(value) = HeaderValue::from_str(&allow_headers.join(", ")) {
            out.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, value);
        }
        if self.allow_credentials {
            out.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
        }

        let mut failures = Vec::new();
        if self.is_allowed_origin(&origin) {
            if explicit_origin && let Some(value) = headers.get(header::ORIGIN) {
                out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value.clone());
            }
        } else {
            failures.push("origin");
        }
        if !ALLOW_METHODS.contains(&latin1(requested_method.as_bytes()).as_str()) {
            failures.push("method");
        }
        if let Some(requested) = headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
            let allowed: Vec<String> = allow_headers.iter().map(|name| name.to_lowercase()).collect();
            let requested = latin1(requested.as_bytes()).to_lowercase();
            if requested
                .split(',')
                .any(|name| !allowed.iter().any(|allowed| allowed == fav_core::pytext::py_strip(name)))
            {
                failures.push("headers");
            }
        }
        if headers.contains_key("access-control-request-private-network") {
            failures.push("private-network");
        }

        let (status, text) = if failures.is_empty() {
            (StatusCode::OK, "OK".to_owned())
        } else {
            (StatusCode::BAD_REQUEST, format!("Disallowed CORS {}", failures.join(", ")))
        };
        let mut response = crate::web::plain_text(status, text);
        response.headers_mut().extend(out);
        Some(response)
    }

    /// The simple-response headers, added to whatever the app answered.
    pub fn decorate(&self, request_headers: &HeaderMap, response: &mut Response<axum::body::Body>) {
        let Some(origin) = request_headers.get(header::ORIGIN) else {
            return;
        };
        let headers = response.headers_mut();
        if self.allow_all_origins() {
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
        }
        if self.allow_credentials {
            headers.insert(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true"));
        }
        let has_cookie = request_headers.contains_key(header::COOKIE);
        let explicit = if self.allow_all_origins() {
            has_cookie
        } else {
            self.is_allowed_origin(&latin1(origin.as_bytes()))
        };
        if explicit {
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
            add_vary_origin(headers);
        }
    }
}

/// `MutableHeaders.add_vary_header('Origin')`.
fn add_vary_origin(headers: &mut HeaderMap) {
    let vary = match headers.get(header::VARY) {
        Some(existing) => format!("{}, Origin", latin1(existing.as_bytes())),
        None => "Origin".to_owned(),
    };
    if let Ok(value) = HeaderValue::from_str(&vary) {
        headers.insert(HeaderName::from_static("vary"), value);
    }
}

#[cfg(test)]
mod tests {
    use super::Cors;

    #[test]
    fn no_origins_means_no_cors() {
        assert!(Cors::new(Vec::new(), true).is_none());
        assert_eq!(
            Cors::allow_headers(),
            ["Accept", "Accept-Language", "Authorization", "Content-Language", "Content-Type"]
        );
    }
}

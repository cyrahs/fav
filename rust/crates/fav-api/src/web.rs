//! The built front end, the documentation pages and the plain responses around them.
//!
//! Python origin: `_mount_web_ui` and `_spa_fallback_response` in `src/api/app.py`,
//! Starlette's `StaticFiles` and `FileResponse`, and FastAPI's `/docs`,
//! `/docs/oauth2-redirect` and `/redoc` pages (their HTML is FastAPI's, byte for byte;
//! `rust/goldens/api/docs.json` holds Python's).

use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::http::{HeaderValue, Response, StatusCode, header};

use crate::error::ApiError;

pub(crate) const SWAGGER_UI_HTML: &str = include_str!("web/swagger-ui.html");
pub(crate) const OAUTH2_REDIRECT_HTML: &str = include_str!("web/oauth2-redirect.html");
pub(crate) const REDOC_HTML: &str = include_str!("web/redoc.html");

/// The icons served from the document root, as Python registers them.
pub(crate) const ROOT_ICONS: [&str; 3] = ["favicon.ico", "icon.svg", "apple-touch-icon.png"];

/// Paths the backend owns: the SPA fallback never answers for them, so a mistyped API
/// route stays a JSON 404 instead of an HTML page.
const NON_SPA_PREFIXES: [&str; 7] = ["api", "mcp", "static", "docs", "openapi.json", "healthz", "readyz"];

/// Header bytes as Starlette decodes them (latin-1).
pub(crate) fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

fn response(status: StatusCode, content_type: &str, body: impl Into<Body>) -> Response<Body> {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

/// `PlainTextResponse`.
pub(crate) fn plain_text(status: StatusCode, text: String) -> Response<Body> {
    response(status, "text/plain; charset=utf-8", text)
}

/// `HTMLResponse`.
pub(crate) fn html(text: &'static str) -> Response<Body> {
    response(StatusCode::OK, "text/html; charset=utf-8", text)
}

/// `_mount_web_ui`: the dist directory when it holds an `index.html`, else `None` (API only).
pub fn find_dist(dir: &Path) -> Option<PathBuf> {
    let log = fav_core::logging::get("fav-api");
    if dir.join("index.html").is_file() {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        log.info(format!("Serving web UI from {}", dir.display()));
        Some(dir)
    } else {
        log.info("web/dist not found; serving API only");
        None
    }
}

/// `_is_backend_path`.
pub(crate) fn is_backend_path(path: &str) -> bool {
    let stripped = path.trim_start_matches('/');
    NON_SPA_PREFIXES
        .iter()
        .any(|prefix| stripped == *prefix || stripped.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')))
}

/// Python 3.12's built-in `mimetypes` entries for what a Vite build contains; the system
/// `mime.types` files are consulted first, as Python reads them over its defaults.
const WEB_TYPES: [(&str, &str); 14] = [
    (".js", "text/javascript"),
    (".mjs", "text/javascript"),
    (".css", "text/css"),
    (".html", "text/html"),
    (".htm", "text/html"),
    (".json", "application/json"),
    (".svg", "image/svg+xml"),
    (".ico", "image/vnd.microsoft.icon"),
    (".png", "image/png"),
    (".webp", "image/webp"),
    (".woff", "font/woff"),
    (".woff2", "font/woff2"),
    (".txt", "text/plain"),
    (".webmanifest", "application/manifest+json"),
];

/// `FileResponse`'s media type: `guess_type(path)[0] or 'text/plain'`, with a charset on
/// text types.
fn media_type(path: &Path) -> String {
    let name = path.to_string_lossy();
    let guessed = fav_core::mimetypes::MimeTypes::system().guess_type(&name).or_else(|| {
        let lower = name.to_lowercase();
        WEB_TYPES
            .iter()
            .find(|(suffix, _)| lower.ends_with(suffix))
            .map(|(_, kind)| (*kind).to_owned())
    });
    let kind = guessed.unwrap_or_else(|| "text/plain".to_owned());
    if kind.starts_with("text/") && !kind.to_lowercase().contains("charset=") {
        format!("{kind}; charset=utf-8")
    } else {
        kind
    }
}

/// `FileResponse(path)`; a file that cannot be read is a 500, as Starlette's `stat` raises.
pub(crate) async fn file_response(path: &Path) -> Result<Response<Body>, ApiError> {
    let bytes = tokio::fs::read(path).await.map_err(|error| {
        fav_core::logging::get("fav-api").error(format!("Unhandled API error: {error}"));
        ApiError::internal()
    })?;
    let length = bytes.len();
    let mut response = response(StatusCode::OK, &media_type(path), bytes);
    response.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    Ok(response)
}

/// `StaticFiles(directory=dist/assets)` for the part of the path after `/assets/`: the file,
/// or a 404 (which the SPA fallback then answers) for anything else, including a path that
/// leaves the directory.
pub(crate) async fn asset(dist: &Path, method: &str, relative: &str) -> Result<Response<Body>, ApiError> {
    if method != "GET" && method != "HEAD" {
        return Err(ApiError::new(405, "method_not_allowed", "Method Not Allowed"));
    }
    let directory = dist.join("assets");
    let mut path = directory.clone();
    for part in relative.split('/') {
        match Path::new(part).components().next() {
            None | Some(Component::CurDir) => {}
            Some(Component::Normal(_)) if !part.contains('\\') => path.push(part),
            _ => return Err(ApiError::http(404)),
        }
    }
    let (Ok(full), Ok(root)) = (tokio::fs::canonicalize(&path).await, tokio::fs::canonicalize(&directory).await) else {
        return Err(ApiError::http(404));
    };
    if !full.starts_with(&root) || !full.is_file() {
        return Err(ApiError::http(404));
    }
    file_response(&full).await
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{is_backend_path, media_type};

    #[test]
    fn backend_paths_are_never_the_front_end() {
        for path in ["/api", "/api/v2/nope", "/mcp", "/docs/x", "/openapi.json", "/healthz", "/readyz/"] {
            assert!(is_backend_path(path), "{path}");
        }
        for path in ["/", "/records", "/apis", "/healthzz", "/logins/x"] {
            assert!(!is_backend_path(path), "{path}");
        }
    }

    #[test]
    fn media_types_follow_python() {
        assert_eq!(media_type(Path::new("a/index-1.js")), "text/javascript; charset=utf-8");
        assert_eq!(media_type(Path::new("a/index.css")), "text/css; charset=utf-8");
        assert_eq!(media_type(Path::new("icon.svg")), "image/svg+xml");
        assert_eq!(media_type(Path::new("unknown.zzz")), "text/plain; charset=utf-8");
    }
}

//! The built front end, the documentation pages and the plain responses around them.
//!
//! Python origin: `_mount_web_ui` and `_spa_fallback_response` in `src/api/app.py`,
//! Starlette's `StaticFiles` and `FileResponse`, and FastAPI's `/docs`,
//! `/docs/oauth2-redirect` and `/redoc` pages (their HTML is FastAPI's, byte for byte;
//! `goldens/api/docs.json` holds Python's).

use std::path::{Path, PathBuf};

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

/// Python 3.12's built-in `mimetypes` table (`MimeTypes().types_map[True]`, printed by
/// Python 3.12.3). The system `mime.types` files are consulted first, as Python reads
/// them over its defaults; the slim production image has none, so this table decides.
const PYTHON_TYPES: [(&str, &str); 149] = [
    (".3g2", "audio/3gpp2"),
    (".3gp", "audio/3gpp"),
    (".3gpp", "audio/3gpp"),
    (".3gpp2", "audio/3gpp2"),
    (".a", "application/octet-stream"),
    (".aac", "audio/aac"),
    (".adts", "audio/aac"),
    (".ai", "application/postscript"),
    (".aif", "audio/x-aiff"),
    (".aifc", "audio/x-aiff"),
    (".aiff", "audio/x-aiff"),
    (".ass", "audio/aac"),
    (".au", "audio/basic"),
    (".avi", "video/x-msvideo"),
    (".avif", "image/avif"),
    (".bat", "text/plain"),
    (".bcpio", "application/x-bcpio"),
    (".bin", "application/octet-stream"),
    (".bmp", "image/bmp"),
    (".c", "text/plain"),
    (".cdf", "application/x-netcdf"),
    (".cpio", "application/x-cpio"),
    (".csh", "application/x-csh"),
    (".css", "text/css"),
    (".csv", "text/csv"),
    (".dll", "application/octet-stream"),
    (".doc", "application/msword"),
    (".dot", "application/msword"),
    (".dvi", "application/x-dvi"),
    (".eml", "message/rfc822"),
    (".eps", "application/postscript"),
    (".etx", "text/x-setext"),
    (".exe", "application/octet-stream"),
    (".gif", "image/gif"),
    (".gtar", "application/x-gtar"),
    (".h", "text/plain"),
    (".h5", "application/x-hdf5"),
    (".hdf", "application/x-hdf"),
    (".heic", "image/heic"),
    (".heif", "image/heif"),
    (".htm", "text/html"),
    (".html", "text/html"),
    (".ico", "image/vnd.microsoft.icon"),
    (".ief", "image/ief"),
    (".jpe", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".jpg", "image/jpeg"),
    (".js", "text/javascript"),
    (".json", "application/json"),
    (".ksh", "text/plain"),
    (".latex", "application/x-latex"),
    (".loas", "audio/aac"),
    (".m1v", "video/mpeg"),
    (".m3u", "application/vnd.apple.mpegurl"),
    (".m3u8", "application/vnd.apple.mpegurl"),
    (".man", "application/x-troff-man"),
    (".me", "application/x-troff-me"),
    (".mht", "message/rfc822"),
    (".mhtml", "message/rfc822"),
    (".mif", "application/x-mif"),
    (".mjs", "text/javascript"),
    (".mov", "video/quicktime"),
    (".movie", "video/x-sgi-movie"),
    (".mp2", "audio/mpeg"),
    (".mp3", "audio/mpeg"),
    (".mp4", "video/mp4"),
    (".mpa", "video/mpeg"),
    (".mpe", "video/mpeg"),
    (".mpeg", "video/mpeg"),
    (".mpg", "video/mpeg"),
    (".ms", "application/x-troff-ms"),
    (".n3", "text/n3"),
    (".nc", "application/x-netcdf"),
    (".nq", "application/n-quads"),
    (".nt", "application/n-triples"),
    (".nws", "message/rfc822"),
    (".o", "application/octet-stream"),
    (".obj", "application/octet-stream"),
    (".oda", "application/oda"),
    (".opus", "audio/opus"),
    (".p12", "application/x-pkcs12"),
    (".p7c", "application/pkcs7-mime"),
    (".pbm", "image/x-portable-bitmap"),
    (".pdf", "application/pdf"),
    (".pfx", "application/x-pkcs12"),
    (".pgm", "image/x-portable-graymap"),
    (".pl", "text/plain"),
    (".png", "image/png"),
    (".pnm", "image/x-portable-anymap"),
    (".pot", "application/vnd.ms-powerpoint"),
    (".ppa", "application/vnd.ms-powerpoint"),
    (".ppm", "image/x-portable-pixmap"),
    (".pps", "application/vnd.ms-powerpoint"),
    (".ppt", "application/vnd.ms-powerpoint"),
    (".ps", "application/postscript"),
    (".pwz", "application/vnd.ms-powerpoint"),
    (".py", "text/x-python"),
    (".pyc", "application/x-python-code"),
    (".pyo", "application/x-python-code"),
    (".qt", "video/quicktime"),
    (".ra", "audio/x-pn-realaudio"),
    (".ram", "application/x-pn-realaudio"),
    (".ras", "image/x-cmu-raster"),
    (".rdf", "application/xml"),
    (".rgb", "image/x-rgb"),
    (".roff", "application/x-troff"),
    (".rtx", "text/richtext"),
    (".sgm", "text/x-sgml"),
    (".sgml", "text/x-sgml"),
    (".sh", "application/x-sh"),
    (".shar", "application/x-shar"),
    (".snd", "audio/basic"),
    (".so", "application/octet-stream"),
    (".src", "application/x-wais-source"),
    (".srt", "text/plain"),
    (".sv4cpio", "application/x-sv4cpio"),
    (".sv4crc", "application/x-sv4crc"),
    (".svg", "image/svg+xml"),
    (".swf", "application/x-shockwave-flash"),
    (".t", "application/x-troff"),
    (".tar", "application/x-tar"),
    (".tcl", "application/x-tcl"),
    (".tex", "application/x-tex"),
    (".texi", "application/x-texinfo"),
    (".texinfo", "application/x-texinfo"),
    (".tif", "image/tiff"),
    (".tiff", "image/tiff"),
    (".tr", "application/x-troff"),
    (".trig", "application/trig"),
    (".tsv", "text/tab-separated-values"),
    (".txt", "text/plain"),
    (".ustar", "application/x-ustar"),
    (".vcf", "text/x-vcard"),
    (".vtt", "text/vtt"),
    (".wasm", "application/wasm"),
    (".wav", "audio/x-wav"),
    (".webm", "video/webm"),
    (".webmanifest", "application/manifest+json"),
    (".wiz", "application/msword"),
    (".wsdl", "application/xml"),
    (".xbm", "image/x-xbitmap"),
    (".xlb", "application/vnd.ms-excel"),
    (".xls", "application/vnd.ms-excel"),
    (".xml", "text/xml"),
    (".xpdl", "application/xml"),
    (".xpm", "image/x-xpixmap"),
    (".xsl", "application/xml"),
    (".xwd", "image/x-xwindowdump"),
    (".zip", "application/zip"),
];

/// `FileResponse`'s media type: `guess_type(path)[0] or 'text/plain'`, with a charset on
/// text types.
fn media_type(path: &Path) -> String {
    let name = path.to_string_lossy();
    let guessed = fav_core::mimetypes::MimeTypes::system().guess_type(&name).or_else(|| {
        // `guess_type` tries the extension as written, then lowercased.
        let extension = format!(".{}", path.extension()?.to_string_lossy());
        let lookup = |extension: &str| {
            PYTHON_TYPES
                .iter()
                .find(|(known, _)| *known == extension)
                .map(|(_, kind)| (*kind).to_owned())
        };
        lookup(&extension).or_else(|| lookup(&extension.to_lowercase()))
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
    // `os.path.normpath(os.path.join(directory, path))`, refused when it leaves the
    // directory: `x/../a.js` is `a.js`, `../a.js` is a 404.
    let mut parts: Vec<&str> = Vec::new();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(ApiError::http(404));
                }
            }
            // A backslash is an ordinary character on Linux, so no such file exists.
            _ if part.contains('\\') => return Err(ApiError::http(404)),
            _ => parts.push(part),
        }
    }
    let mut path = directory.clone();
    path.extend(parts);
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
        assert_eq!(media_type(Path::new("no-extension")), "text/plain; charset=utf-8");
        assert_eq!(
            super::PYTHON_TYPES
                .iter()
                .find(|(extension, _)| *extension == ".wasm")
                .map(|(_, kind)| *kind),
            Some("application/wasm")
        );
    }
}

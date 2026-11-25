//! What the Emby plugin reads for Huangguo (`emby-plugin/`): a series' details and saved
//! episodes, its poster and each episode's thumbnail, as the Huangguo job stored them.
//!
//! Read-only, and outside the documented API on purpose: that one mirrors Python's, which
//! never had these routes. Only a `CREDENTIAL_CLIENTS` client whose sites include
//! [`SCOPE`] may read it; `API_TOKEN` does not, and the scope gives no stored login (no
//! site is called that).
//!
//! - `GET /media/huangguo/series?dir_name=<folder>[&category=<folder>]`
//! - `GET /media/huangguo/series/{id}`
//! - `GET /media/huangguo/series/{id}/poster`
//! - `GET /media/huangguo/series/{id}/episodes/{ep}/thumbnail`
//!
//! Emby fetches an image URL a provider gave it on its own, without the provider's
//! `Authorization` header (`/Images/Remote`, and an image it stored as a URL and fetches when it
//! is first shown). So the series answer also carries each image's URL signed with a key from
//! `API_TOKEN` (`poster_url`, `thumbnail_url`): an image route with a valid `sig` needs no
//! token. A signature opens that one image and nothing else; it changes with `API_TOKEN`.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use fav_core::pyjson::Value;
use fav_sources::huangguo::{POSTER_EP, category_dir_name};
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;
use sqlx::Row as _;

use crate::error::ApiError;
use crate::handlers::unhandled;
use crate::router::Request;
use crate::{ApiState, credentials, json};

/// Where the routes live.
pub(crate) const PREFIX: &str = "/media/huangguo";
/// The `CREDENTIAL_CLIENTS` site that grants these routes.
pub const SCOPE: &str = "huangguo-media";
/// The query parameter that carries an image URL's signature.
const SIGNATURE: &str = "sig";
/// Bytes of the HMAC kept in a signature (hex, so twice as many characters).
const SIGNATURE_BYTES: usize = 16;

/// The answer, when the path is one of these routes.
pub(crate) async fn answer(state: &ApiState, request: &Request) -> Option<Result<Response, ApiError>> {
    let rest = request.path.strip_prefix(PREFIX)?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
    Some(route(state, request, &parts).await)
}

async fn route(state: &ApiState, request: &Request, parts: &[&str]) -> Result<Response, ApiError> {
    if request.method != "GET" && request.method != "HEAD" {
        return Err(ApiError::http(405).with_header("allow", "GET"));
    }
    let is_image = matches!(parts, ["series", _, "poster"] | ["series", _, "episodes", _, "thumbnail"]);
    match query(request, SIGNATURE) {
        Some(signature) if is_image => {
            if !signature_valid(&state.token, &request.path, signature) {
                return Err(ApiError::new(403, "invalid_signature", "The image signature is not valid."));
            }
        }
        _ => authorize(state, request)?,
    }
    match parts {
        ["series"] => {
            let dir_name = query(request, "dir_name").ok_or_else(|| ApiError::new(422, "missing_dir_name", "dir_name is required."))?;
            let id = find_series(state, dir_name, query(request, "category")).await?;
            series(state, id).await
        }
        ["series", id] => series(state, number(id)?).await,
        ["series", id, "poster"] => image(state, number(id)?, POSTER_EP).await,
        ["series", id, "episodes", ep, "thumbnail"] => image(state, number(id)?, number(ep)?).await,
        _ => Err(ApiError::http(404)),
    }
}

/// A `CREDENTIAL_CLIENTS` client with the [`SCOPE`].
fn authorize(state: &ApiState, request: &Request) -> Result<(), ApiError> {
    let client = credentials::authenticate_client(&state.credential_clients, request.header("authorization").as_deref())?;
    if !client.sites.contains(SCOPE) {
        return Err(ApiError::new(
            403,
            "scope_not_allowed",
            format!("This client may not read {SCOPE}."),
        ));
    }
    Ok(())
}

/// The HMAC of an image route's path under a key only fav holds.
fn mac(token: &str, path: &str) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes any key length");
    mac.update(b"fav huangguo-media image\0");
    mac.update(path.as_bytes());
    mac
}

/// `path?sig=<signature>`: the image at `path`, readable without a token.
fn signed(token: &str, path: &str) -> String {
    use std::fmt::Write as _;
    let digest = mac(token, path).finalize().into_bytes();
    digest[..SIGNATURE_BYTES]
        .iter()
        .fold(format!("{path}?{SIGNATURE}="), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Whether `signature` is the one [`signed`] gives `path` (compared in constant time).
fn signature_valid(token: &str, path: &str, signature: &str) -> bool {
    if token.is_empty() || signature.len() != SIGNATURE_BYTES * 2 {
        return false;
    }
    let Some(bytes) = (0..signature.len())
        .step_by(2)
        .map(|at| signature.get(at..at + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok()))
        .collect::<Option<Vec<u8>>>()
    else {
        return false;
    };
    mac(token, path).verify_truncated_left(&bytes).is_ok()
}

/// The signed URL of a saved image, `null` when there is none.
fn image_url(state: &ApiState, saved: bool, path: &str) -> Value {
    if saved && !state.token.is_empty() {
        Value::Str(signed(&state.token, path))
    } else {
        Value::Null
    }
}

fn query<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .query
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
}

fn number(text: &str) -> Result<i64, ApiError> {
    text.parse().map_err(|_| ApiError::http(404))
}

fn not_found(what: &str) -> ApiError {
    ApiError::new(404, "not_found", format!("No such Huangguo {what}."))
}

/// The series saved in this folder: its directory name, and the category folder when given
/// (a name can repeat across categories).
async fn find_series(state: &ApiState, dir_name: &str, category: Option<&str>) -> Result<i64, ApiError> {
    let rows = sqlx::query("SELECT video_id, category FROM huangguo_series WHERE dir_name = $1 ORDER BY video_id")
        .bind(dir_name)
        .fetch_all(state.db.pool())
        .await
        .map_err(|error| unhandled(&error))?;
    let mut matching = Vec::new();
    for row in &rows {
        let stored: String = row.try_get("category").map_err(|error| unhandled(&error))?;
        if category.is_none_or(|category| category == category_dir_name(&stored)) {
            matching.push(row.try_get::<i64, _>("video_id").map_err(|error| unhandled(&error))?);
        }
    }
    match matching.as_slice() {
        [id] => Ok(*id),
        [] => Err(not_found("series in that folder")),
        _ => Err(ApiError::new(
            409,
            "ambiguous_folder",
            "Several series use that folder; give its category.",
        )),
    }
}

/// `2026-11-25` of `2026-11-25 14:59:54`, when that is what the site wrote.
fn date_part(premiered: &str) -> Option<&str> {
    let date = premiered.get(..10)?;
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok().map(|_| date)
}

async fn series(state: &ApiState, id: i64) -> Result<Response, ApiError> {
    let pool = state.db.pool();
    let row = sqlx::query(
        "SELECT category, title, author, tags, dir_name, plot, score, premiered, finished FROM huangguo_series WHERE video_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|error| unhandled(&error))?
    .ok_or_else(|| not_found("series"))?;
    let get_text = |name: &str| row.try_get::<String, _>(name).map_err(|error| unhandled(&error));
    let tags: Vec<Value> = get_text("tags")?
        .split(',')
        .filter(|tag| !tag.is_empty())
        .map(|tag| Value::Str(tag.to_owned()))
        .collect();
    let premiered = get_text("premiered")?;
    let score: Option<f64> = row.try_get("score").map_err(|error| unhandled(&error))?;
    let finished: bool = row.try_get("finished").map_err(|error| unhandled(&error))?;
    let images: Vec<i32> = sqlx::query_scalar("SELECT ep_num FROM huangguo_images WHERE video_id = $1")
        .bind(id)
        .fetch_all(pool)
        .await
        .map_err(|error| unhandled(&error))?;
    let episodes = sqlx::query("SELECT ep_num, title, duration FROM huangguo WHERE video_id = $1 ORDER BY ep_num")
        .bind(id)
        .fetch_all(pool)
        .await
        .map_err(|error| unhandled(&error))?
        .iter()
        .map(|episode| {
            let ep_num: i32 = episode.try_get("ep_num")?;
            let title: String = episode.try_get("title")?;
            let duration: i32 = episode.try_get("duration")?;
            let has_thumbnail = images.contains(&ep_num);
            Ok(json::dict([
                ("ep_num", Value::Int(i128::from(ep_num))),
                ("title", Value::Str(title)),
                ("duration", Value::Int(i128::from(duration))),
                ("has_thumbnail", Value::Bool(has_thumbnail)),
                (
                    "thumbnail_url",
                    image_url(state, has_thumbnail, &format!("{PREFIX}/series/{id}/episodes/{ep_num}/thumbnail")),
                ),
            ]))
        })
        .collect::<Result<Vec<Value>, sqlx::Error>>()
        .map_err(|error| unhandled(&error))?;
    let poster_ep = i32::try_from(POSTER_EP).unwrap_or_default();
    let body = json::dict([
        ("id", Value::Int(i128::from(id))),
        ("title", Value::Str(get_text("title")?)),
        ("category", Value::Str(get_text("category")?)),
        ("dir_name", Value::Str(get_text("dir_name")?)),
        ("plot", Value::Str(get_text("plot")?)),
        ("author", Value::Str(get_text("author")?)),
        ("tags", Value::List(tags)),
        ("score", score.map_or(Value::Null, Value::Float)),
        (
            "premiere_date",
            date_part(&premiered).map_or(Value::Null, |date| Value::Str(date.to_owned())),
        ),
        ("finished", Value::Bool(finished)),
        ("url", Value::Str(format!("https://huangguoai.com/video/{id}/"))),
        ("has_poster", Value::Bool(images.contains(&poster_ep))),
        (
            "poster_url",
            image_url(state, images.contains(&poster_ep), &format!("{PREFIX}/series/{id}/poster")),
        ),
        ("episodes", Value::List(episodes)),
    ]);
    Ok(json::response(StatusCode::OK, &body))
}

async fn image(state: &ApiState, id: i64, ep_num: i64) -> Result<Response, ApiError> {
    let row = sqlx::query("SELECT content_type, data FROM huangguo_images WHERE video_id = $1 AND ep_num = $2")
        .bind(id)
        .bind(i32::try_from(ep_num).map_err(|_| ApiError::http(404))?)
        .fetch_optional(state.db.pool())
        .await
        .map_err(|error| unhandled(&error))?
        .ok_or_else(|| not_found("image"))?;
    let content_type: String = row.try_get("content_type").map_err(|error| unhandled(&error))?;
    let data: Vec<u8> = row.try_get("data").map_err(|error| unhandled(&error))?;
    let length = data.len();
    let mut response = Response::new(Body::from(data));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=3600"));
    Ok(response)
}

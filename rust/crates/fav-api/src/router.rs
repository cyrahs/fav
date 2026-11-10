//! Request dispatch in Starlette's order.
//!
//! Python origin: Starlette 0.52's `Router` as FastAPI 0.135 builds it for `create_app`,
//! and the exception handlers in `src/api/app.py`. Routes are tried in registration order:
//! the first whose path and method match handles the request; when only paths matched,
//! the first of those answers 405 with its `Allow`; when nothing matched, the path with
//! its trailing slash toggled is tried and a match there is a 307 redirect. A 404 from
//! routing or the static files becomes the front end's `index.html` for a `GET`/`HEAD`
//! outside the backend's paths (the SPA fallback). CORS wraps everything.
//!
//! axum serves the connection; [`app`] hands every request to [`handle`].

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Request as HttpRequest, StatusCode, header};
use axum::response::Response;
use percent_encoding::percent_decode_str;

use crate::error::ApiError;
use crate::handlers;
use crate::web::{self, latin1};
use crate::{ApiState, json};

/// The largest body read; FastAPI has no limit, but nothing fav accepts comes close.
pub(crate) const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// One request, as the handlers see it.
#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// The percent-decoded path (Starlette's `scope['path']`).
    pub path: String,
    pub query_string: String,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Request {
    /// The first value of a header, decoded as latin-1.
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).map(|value| latin1(value.as_bytes()))
    }

    /// `query_params.get(name)`: the last value given.
    pub fn query_last(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// `query_params.getlist(name)`.
    pub fn query_all(&self, name: &str) -> Vec<&str> {
        self.query
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// What a matched route does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endpoint {
    OpenApi,
    Docs,
    OAuth2Redirect,
    Redoc,
    ListJobs,
    CreateJobRequest,
    GetJobRequest,
    ListJobRequests,
    ListSettings,
    GetSettingsSection,
    UpdateSettingsSection,
    TestTelegramNotification,
    TestTikHub,
    TestAzurLaneProxy,
    ListArchiveSources,
    ListArchiveItems,
    ListHanime1Videos,
    CreateHanime1Seed,
    ListHanime1Seeds,
    DeleteHanime1Seed,
    CreateHanime1Author,
    ListHanime1Authors,
    DeleteHanime1Author,
    ResolveKemonoCreator,
    ListAzurLaneCharacters,
    ListAzurLaneSidebarCharacters,
    GetAzurLaneSkinUpdates,
    GetAzurLaneCharacter,
    GetAzurLaneShipDetail,
    ListNikkeCharacters,
    ListNikkeSidebarCharacters,
    GetNikkeCharacter,
    GetNikkeLive2DViewOverride,
    PutNikkeLive2DViewOverride,
    DeleteNikkeLive2DViewOverride,
    ListBd2Characters,
    ListBd2SidebarCharacters,
    GetBd2Character,
    GetBd2Live2DViewOverride,
    PutBd2Live2DViewOverride,
    DeleteBd2Live2DViewOverride,
    ListSiteSessions,
    GetSiteCredential,
    /// The MCP endpoint, a plain route for every method (`crate::mcp`).
    Mcp,
    Health,
    Readiness,
    Icon(&'static str),
}

/// One registered route.
#[derive(Debug, Clone, Copy)]
struct Route {
    path: &'static str,
    methods: &'static [&'static str],
    endpoint: Endpoint,
}

const GET: &[&str] = &["GET"];
const GET_HEAD: &[&str] = &["GET", "HEAD"];
const POST: &[&str] = &["POST"];
const PUT: &[&str] = &["PUT"];
const DELETE: &[&str] = &["DELETE"];
/// A route without methods (an ASGI app as the endpoint) takes every method.
const ANY: &[&str] = &[];

const BD2_VIEW_OVERRIDE_PATH: &str = "/api/v2/bd2/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}";
const NIKKE_VIEW_OVERRIDE_PATH: &str = "/api/v2/nikke/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}";

/// The routes in Python's registration order: FastAPI's documents first, then the API
/// router, the credentials router, the MCP endpoint, the system routes, and the front
/// end. Routes owned by later work packages are absent (`openapi::PENDING`).
const ROUTES: &[Route] = &[
    Route {
        path: "/openapi.json",
        methods: GET_HEAD,
        endpoint: Endpoint::OpenApi,
    },
    Route {
        path: "/docs",
        methods: GET_HEAD,
        endpoint: Endpoint::Docs,
    },
    Route {
        path: "/docs/oauth2-redirect",
        methods: GET_HEAD,
        endpoint: Endpoint::OAuth2Redirect,
    },
    Route {
        path: "/redoc",
        methods: GET_HEAD,
        endpoint: Endpoint::Redoc,
    },
    Route {
        path: "/api/v2/jobs",
        methods: GET,
        endpoint: Endpoint::ListJobs,
    },
    Route {
        path: "/api/v2/job-requests",
        methods: POST,
        endpoint: Endpoint::CreateJobRequest,
    },
    Route {
        path: "/api/v2/job-requests/{request_id}",
        methods: GET,
        endpoint: Endpoint::GetJobRequest,
    },
    Route {
        path: "/api/v2/job-requests",
        methods: GET,
        endpoint: Endpoint::ListJobRequests,
    },
    Route {
        path: "/api/v2/settings",
        methods: GET,
        endpoint: Endpoint::ListSettings,
    },
    Route {
        path: "/api/v2/settings/{section}",
        methods: GET,
        endpoint: Endpoint::GetSettingsSection,
    },
    Route {
        path: "/api/v2/settings/{section}",
        methods: PUT,
        endpoint: Endpoint::UpdateSettingsSection,
    },
    Route {
        path: "/api/v2/notifications/telegram/test",
        methods: POST,
        endpoint: Endpoint::TestTelegramNotification,
    },
    Route {
        path: "/api/v2/services/tikhub/test",
        methods: POST,
        endpoint: Endpoint::TestTikHub,
    },
    Route {
        path: "/api/v2/azurlane/proxy/test",
        methods: POST,
        endpoint: Endpoint::TestAzurLaneProxy,
    },
    Route {
        path: "/api/v2/archive/sources",
        methods: GET,
        endpoint: Endpoint::ListArchiveSources,
    },
    Route {
        path: "/api/v2/archive/items",
        methods: GET,
        endpoint: Endpoint::ListArchiveItems,
    },
    Route {
        path: "/api/v2/hanime1/videos",
        methods: GET,
        endpoint: Endpoint::ListHanime1Videos,
    },
    Route {
        path: "/api/v2/hanime1/seeds",
        methods: POST,
        endpoint: Endpoint::CreateHanime1Seed,
    },
    Route {
        path: "/api/v2/hanime1/seeds",
        methods: GET,
        endpoint: Endpoint::ListHanime1Seeds,
    },
    Route {
        path: "/api/v2/hanime1/seeds/{canonical_video_id}",
        methods: DELETE,
        endpoint: Endpoint::DeleteHanime1Seed,
    },
    Route {
        path: "/api/v2/hanime1/authors",
        methods: POST,
        endpoint: Endpoint::CreateHanime1Author,
    },
    Route {
        path: "/api/v2/hanime1/authors",
        methods: GET,
        endpoint: Endpoint::ListHanime1Authors,
    },
    Route {
        path: "/api/v2/hanime1/authors/{author_id}",
        methods: DELETE,
        endpoint: Endpoint::DeleteHanime1Author,
    },
    Route {
        path: "/api/v2/kemono/creators/resolve",
        methods: POST,
        endpoint: Endpoint::ResolveKemonoCreator,
    },
    Route {
        path: "/api/v2/azurlane/characters",
        methods: GET,
        endpoint: Endpoint::ListAzurLaneCharacters,
    },
    Route {
        path: "/api/v2/azurlane/sidebar/characters",
        methods: GET,
        endpoint: Endpoint::ListAzurLaneSidebarCharacters,
    },
    Route {
        path: "/api/v2/azurlane/skin-updates",
        methods: GET,
        endpoint: Endpoint::GetAzurLaneSkinUpdates,
    },
    Route {
        path: "/api/v2/azurlane/characters/{character_key}",
        methods: GET,
        endpoint: Endpoint::GetAzurLaneCharacter,
    },
    Route {
        path: "/api/v2/azurlane/characters/{character_key}/ship-detail",
        methods: GET,
        endpoint: Endpoint::GetAzurLaneShipDetail,
    },
    Route {
        path: "/api/v2/nikke/characters",
        methods: GET,
        endpoint: Endpoint::ListNikkeCharacters,
    },
    Route {
        path: "/api/v2/nikke/sidebar/characters",
        methods: GET,
        endpoint: Endpoint::ListNikkeSidebarCharacters,
    },
    Route {
        path: "/api/v2/nikke/characters/{content_id}",
        methods: GET,
        endpoint: Endpoint::GetNikkeCharacter,
    },
    Route {
        path: NIKKE_VIEW_OVERRIDE_PATH,
        methods: GET,
        endpoint: Endpoint::GetNikkeLive2DViewOverride,
    },
    Route {
        path: NIKKE_VIEW_OVERRIDE_PATH,
        methods: PUT,
        endpoint: Endpoint::PutNikkeLive2DViewOverride,
    },
    Route {
        path: NIKKE_VIEW_OVERRIDE_PATH,
        methods: DELETE,
        endpoint: Endpoint::DeleteNikkeLive2DViewOverride,
    },
    Route {
        path: "/api/v2/bd2/characters",
        methods: GET,
        endpoint: Endpoint::ListBd2Characters,
    },
    Route {
        path: "/api/v2/bd2/sidebar/characters",
        methods: GET,
        endpoint: Endpoint::ListBd2SidebarCharacters,
    },
    Route {
        path: "/api/v2/bd2/characters/{content_id}",
        methods: GET,
        endpoint: Endpoint::GetBd2Character,
    },
    Route {
        path: BD2_VIEW_OVERRIDE_PATH,
        methods: GET,
        endpoint: Endpoint::GetBd2Live2DViewOverride,
    },
    Route {
        path: BD2_VIEW_OVERRIDE_PATH,
        methods: PUT,
        endpoint: Endpoint::PutBd2Live2DViewOverride,
    },
    Route {
        path: BD2_VIEW_OVERRIDE_PATH,
        methods: DELETE,
        endpoint: Endpoint::DeleteBd2Live2DViewOverride,
    },
    Route {
        path: "/api/v2/site-sessions",
        methods: GET,
        endpoint: Endpoint::ListSiteSessions,
    },
    Route {
        path: "/api/v2/credentials/{site}",
        methods: GET,
        endpoint: Endpoint::GetSiteCredential,
    },
    Route {
        path: crate::mcp::MCP_PATH,
        methods: ANY,
        endpoint: Endpoint::Mcp,
    },
    Route {
        path: "/healthz",
        methods: GET,
        endpoint: Endpoint::Health,
    },
    Route {
        path: "/readyz",
        methods: GET,
        endpoint: Endpoint::Readiness,
    },
];

impl Endpoint {
    /// Routes under the API router carry its bearer-token dependency.
    pub(crate) fn needs_api_token(self) -> bool {
        matches!(
            self,
            Self::ListJobs
                | Self::CreateJobRequest
                | Self::GetJobRequest
                | Self::ListJobRequests
                | Self::ListSettings
                | Self::GetSettingsSection
                | Self::UpdateSettingsSection
                | Self::TestTelegramNotification
                | Self::TestTikHub
                | Self::TestAzurLaneProxy
                | Self::ListArchiveSources
                | Self::ListArchiveItems
                | Self::ListHanime1Videos
                | Self::CreateHanime1Seed
                | Self::ListHanime1Seeds
                | Self::DeleteHanime1Seed
                | Self::CreateHanime1Author
                | Self::ListHanime1Authors
                | Self::DeleteHanime1Author
                | Self::ResolveKemonoCreator
                | Self::ListAzurLaneCharacters
                | Self::ListAzurLaneSidebarCharacters
                | Self::GetAzurLaneSkinUpdates
                | Self::GetAzurLaneCharacter
                | Self::GetAzurLaneShipDetail
                | Self::ListNikkeCharacters
                | Self::ListNikkeSidebarCharacters
                | Self::GetNikkeCharacter
                | Self::GetNikkeLive2DViewOverride
                | Self::PutNikkeLive2DViewOverride
                | Self::DeleteNikkeLive2DViewOverride
                | Self::ListBd2Characters
                | Self::ListBd2SidebarCharacters
                | Self::GetBd2Character
                | Self::GetBd2Live2DViewOverride
                | Self::PutBd2Live2DViewOverride
                | Self::DeleteBd2Live2DViewOverride
                | Self::ListSiteSessions
        )
    }
}

/// Path parameters, in the order the path declares them.
pub type PathParams = Vec<(&'static str, String)>;

fn match_path(pattern: &'static str, path: &str) -> Option<PathParams> {
    let mut params = Vec::new();
    let mut pattern_parts = pattern.split('/');
    let mut path_parts = path.split('/');
    loop {
        match (pattern_parts.next(), path_parts.next()) {
            (None, None) => return Some(params),
            (Some(expected), Some(actual)) => {
                if let Some(name) = expected.strip_prefix('{').and_then(|rest| rest.strip_suffix('}')) {
                    if actual.is_empty() {
                        return None;
                    }
                    params.push((name, actual.to_owned()));
                } else if expected != actual {
                    return None;
                }
            }
            _ => return None,
        }
    }
}

/// `Match.FULL` (the route handles it), `Match.PARTIAL` (path only) or nothing.
enum Matched {
    Full(Endpoint, PathParams),
    Partial(&'static [&'static str]),
    Mount(String),
    None,
}

fn match_route(route: &Route, method: &str, path: &str) -> Matched {
    match match_path(route.path, path) {
        Some(params) if route.methods.is_empty() || route.methods.contains(&method) => Matched::Full(route.endpoint, params),
        Some(_) => Matched::Partial(route.methods),
        None => Matched::None,
    }
}

/// The routes the front end adds when `web/dist` is present: the `/assets` mount (any
/// method; `StaticFiles` checks it) and the root icons (`GET` only).
fn match_front_end(state: &ApiState, method: &str, path: &str) -> Matched {
    let Some(dist) = &state.web_dist else {
        return Matched::None;
    };
    if let Some(rest) = path.strip_prefix("/assets/") {
        return Matched::Mount(rest.to_owned());
    }
    for name in web::ROOT_ICONS {
        if path.strip_prefix('/') == Some(name) && dist.join(name).is_file() {
            return if method == "GET" {
                Matched::Full(Endpoint::Icon(name), Vec::new())
            } else {
                Matched::Partial(GET)
            };
        }
    }
    Matched::None
}

/// Route a method and path: the first full match, else the first partial one.
fn resolve(state: &ApiState, method: &str, path: &str) -> Matched {
    let mut partial = None;
    for route in ROUTES {
        match match_route(route, method, path) {
            Matched::None => {}
            Matched::Partial(methods) => {
                partial.get_or_insert(methods);
            }
            full => return full,
        }
    }
    match match_front_end(state, method, path) {
        Matched::Partial(methods) => {
            partial.get_or_insert(methods);
        }
        Matched::None => {}
        full => return full,
    }
    partial.map_or(Matched::None, Matched::Partial)
}

/// The axum application: every request goes through [`handle`].
pub fn app(state: Arc<ApiState>) -> Router {
    Router::new().fallback(serve).with_state(state)
}

pub(crate) async fn serve(State(state): State<Arc<ApiState>>, request: HttpRequest<Body>) -> Response {
    let (parts, body) = request.into_parts();
    // The MCP endpoint refuses large bodies itself (`RequestBodyLimitMiddleware`). Routes
    // match the percent-decoded path, so `/%6Dcp` is the endpoint too.
    let path = percent_decode_str(parts.uri.path()).decode_utf8_lossy().into_owned();
    let is_mcp = path == crate::mcp::MCP_PATH;
    let limit = if is_mcp { crate::mcp::MAX_BODY_BYTES + 1 } else { MAX_BODY_BYTES };
    let Ok(body) = axum::body::to_bytes(body, limit).await else {
        if is_mcp {
            return crate::mcp::body_too_large();
        }
        return ApiError::new(400, "http_error", "There was an error parsing the body").into_response_value();
    };
    let query_string = parts.uri.query().unwrap_or_default().to_owned();
    let request = Request {
        method: parts.method.as_str().to_owned(),
        path,
        query: form_urlencoded::parse(query_string.as_bytes()).into_owned().collect(),
        query_string,
        headers: parts.headers,
        body,
    };
    handle(&state, request).await
}

/// Answer one request.
pub async fn handle(state: &ApiState, request: Request) -> Response {
    if let Some(cors) = &state.cors
        && let Some(response) = cors.preflight(&request.method, &request.headers)
    {
        return response;
    }
    let mut response = match route(state, &request).await {
        Ok(response) => response,
        Err(error) => error_response(state, &request, error).await,
    };
    if let Some(cors) = &state.cors {
        cors.decorate(&request.headers, &mut response);
    }
    response
}

async fn route(state: &ApiState, request: &Request) -> Result<Response, ApiError> {
    match resolve(state, &request.method, &request.path) {
        Matched::Full(endpoint, params) => handlers::call(state, endpoint, &params, request).await,
        Matched::Mount(relative) => match &state.web_dist {
            Some(dist) => web::asset(dist, &request.method, &relative).await,
            None => Err(ApiError::http(404)),
        },
        Matched::Partial(methods) => Err(ApiError::http(405).with_header("allow", methods.join(", "))),
        Matched::None => redirect_slashes(state, request).ok_or_else(|| ApiError::http(404)),
    }
}

/// `redirect_slashes`: a 307 to the same path with its trailing slash toggled, when a
/// route matches that.
fn redirect_slashes(state: &ApiState, request: &Request) -> Option<Response> {
    if request.path == "/" {
        return None;
    }
    let toggled = if request.path.ends_with('/') {
        request.path.trim_end_matches('/').to_owned()
    } else {
        format!("{}/", request.path)
    };
    if matches!(resolve(state, &request.method, &toggled), Matched::None) {
        return None;
    }
    let host = request.header("host").unwrap_or_else(|| "localhost".to_owned());
    let mut location = format!("http://{host}{toggled}");
    if !request.query_string.is_empty() {
        location.push('?');
        location.push_str(&request.query_string);
    }
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    let value = HeaderValue::from_str(&location)
        .or_else(|_| HeaderValue::from_str(&percent_encoding::utf8_percent_encode(&location, percent_encoding::CONTROLS).to_string()))
        .ok()?;
    response.headers_mut().insert(header::LOCATION, value);
    Some(response)
}

/// The response for an error: the envelope, or the SPA fallback.
async fn error_response(state: &ApiState, request: &Request, error: ApiError) -> Response {
    if error.http_exception
        && error.status == 404
        && let Some(dist) = &state.web_dist
        && (request.method == "GET" || request.method == "HEAD")
        && !web::is_backend_path(&request.path)
    {
        return match web::file_response(&dist.join("index.html")).await {
            Ok(response) => response,
            Err(error) => error.into_response_value(),
        };
    }
    error.into_response_value()
}

/// A JSON response with a status.
pub(crate) fn json_response(status: u16, value: &fav_core::pyjson::Value) -> Response {
    json::response(StatusCode::from_u16(status).unwrap_or(StatusCode::OK), value)
}

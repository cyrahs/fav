//! The error envelope every failure is answered with.
//!
//! Python origin: `src/api/errors.py` (`ApiError`) and the exception handlers in
//! `src/api/app.py`. Every error, whether the service raised it, request validation failed,
//! routing found nothing or something unexpected broke, has the same shape:
//! `{"error": {"code", "message", "details"}}`.

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use fav_core::pyjson::Value;

use crate::json;

/// `WWW-Authenticate` on every 401.
pub const WWW_AUTHENTICATE_BEARER: &str = "Bearer realm=\"fav-api\"";

/// `ApiError`: a status, a machine-readable code, a message, optional details and headers.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub details: Value,
    pub headers: Vec<(&'static str, String)>,
    /// Raised by routing or the static files (Starlette's `HTTPException`), which the SPA
    /// fallback may answer instead; an error a handler raises never is.
    pub(crate) http_exception: bool,
}

impl ApiError {
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: Value::Null,
            headers: Vec::new(),
            http_exception: false,
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    #[must_use]
    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// The 500 every unexpected failure becomes; the cause is logged, never returned.
    pub fn internal() -> Self {
        Self::new(500, "internal_server_error", "Internal server error.")
    }

    /// The 401 of the bearer checks.
    pub fn unauthorized(code: &str, message: &str) -> Self {
        Self::new(401, code, message).with_header("www-authenticate", WWW_AUTHENTICATE_BEARER)
    }

    /// `RequestValidationError`: 422 with pydantic's error entries.
    pub fn validation(errors: Vec<Value>) -> Self {
        Self::new(422, "validation_error", "Request validation failed.").with_details(Value::List(errors))
    }

    /// Starlette's `HTTPException` for a status routing produced (404, 405).
    pub fn http(status: u16) -> Self {
        let (code, message) = match status {
            404 => ("not_found", "Not Found"),
            405 => ("method_not_allowed", "Method Not Allowed"),
            400 => ("http_error", "Bad Request"),
            _ => ("http_error", "Error"),
        };
        let mut error = Self::new(status, code, message);
        error.http_exception = true;
        error
    }

    pub fn envelope(&self) -> Value {
        json::dict([(
            "error",
            json::dict([
                ("code", Value::Str(self.code.clone())),
                ("message", Value::Str(self.message.clone())),
                ("details", self.details.clone()),
            ]),
        )])
    }

    pub(crate) fn into_response_value(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = json::response(status, &self.envelope());
        for (name, value) in &self.headers {
            if let Ok(value) = HeaderValue::from_str(value) {
                response.headers_mut().insert(HeaderName::from_static(name), value);
            }
        }
        response
    }
}

impl std::fmt::Display for ApiError {
    /// `ApiError.__str__`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.into_response_value()
    }
}

//! The seam to the runtime layer capture (`src/web/nikke_runtime.py`, WP17).
//!
//! `capture_gamekee_runtime_layers` opens a character page in headless Chromium and reads
//! the stacking order of its Live2D players. WP17 ports it; until then the production
//! capturer reports it unavailable (DIVERGENCES.md D90). The scheduled crawl never asks
//! for a capture (`allow_runtime_capture=False`), so only `download_character` and the
//! `nikke-layers` command's capture modes meet that error.

use fav_core::pyjson::Dict;
use futures_util::future::BoxFuture;

use crate::pydecode::PyError;

/// `RuntimeCaptureRequest`.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeCaptureRequest {
    pub content_id: i128,
    pub models: Vec<Dict>,
    pub title: String,
    pub timeout_ms: i64,
    pub headless: bool,
    pub source_url: String,
    pub expected_layer_count: Option<i64>,
}

impl RuntimeCaptureRequest {
    /// The defaults of the Python dataclass for the fields callers leave out.
    pub fn new(content_id: i128, models: Vec<Dict>, title: impl Into<String>, timeout_ms: i64) -> Self {
        Self {
            content_id,
            models,
            title: title.into(),
            timeout_ms,
            headless: true,
            source_url: String::new(),
            expected_layer_count: None,
        }
    }
}

/// `capture_gamekee_runtime_layers`: the capture payload for a page, or the error.
pub trait RuntimeCapturer: Send + Sync {
    fn capture(&self, request: RuntimeCaptureRequest) -> BoxFuture<'_, Result<Dict, PyError>>;
}

/// The message the capture fails with until WP17 lands.
pub const UNAVAILABLE_MESSAGE: &str = "Nikke runtime layer capture is not available in this build yet (WP17)";

/// The production capturer before WP17: every capture fails with
/// [`UNAVAILABLE_MESSAGE`], as a failed browser capture would.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableCapturer;

impl RuntimeCapturer for UnavailableCapturer {
    fn capture(&self, _request: RuntimeCaptureRequest) -> BoxFuture<'_, Result<Dict, PyError>> {
        Box::pin(async { Err(PyError::new("RuntimeError", UNAVAILABLE_MESSAGE)) })
    }
}

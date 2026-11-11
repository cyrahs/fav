//! Foundation shared by every other fav crate.
//!
//! The bootstrap environment ([`env`](mod@env), `src/core/env.py`), the settings models
//! ([`settings`], `src/core/settings.py`; their `app_settings` store is in fav-db),
//! logging ([`logging`], `src/core/logger.py`), `mimetypes.guess_type` ([`mimetypes`]), filename helpers ([`filename`],
//! `src/tool/filename.py`), and the Python-compatible pieces the rest is built on:
//! JSON read and written like Python ([`pyjson`]), pydantic's coercion and error
//! entries ([`validate`]) and Python's string predicates ([`pytext`]). `golden` (feature
//! `test-support`) reads the Python-generated goldens in tests.

pub mod env;
pub mod filename;
#[cfg(any(test, feature = "test-support"))]
pub mod golden;
pub mod logging;
pub mod mimetypes;
pub mod pycasefold;
pub mod pyhtml;
pub mod pyint;
pub mod pyjson;
pub mod pypath;
pub mod pytext;
pub mod pyurl;
pub mod settings;
pub mod validate;

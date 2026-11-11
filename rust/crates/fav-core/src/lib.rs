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

/// The Python commit this rewrite ports, as recorded in `rust/BASELINE`.
pub const BASELINE_COMMIT: &str = trim_ascii_newline(include_str!("../../../BASELINE"));

const fn trim_ascii_newline(text: &str) -> &str {
    match text.as_bytes() {
        [rest @ .., b'\n'] => match std::str::from_utf8(rest) {
            Ok(trimmed) => trimmed,
            Err(_) => text,
        },
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::BASELINE_COMMIT;

    #[test]
    fn baseline_is_a_full_commit_hash() {
        assert_eq!(BASELINE_COMMIT.len(), 40, "{BASELINE_COMMIT:?}");
        assert!(BASELINE_COMMIT.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }
}

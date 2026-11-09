//! Foundation shared by every other fav crate.
//!
//! Scope (filled in by WP1, see `rust/docs/PLAN.md`): the bootstrap environment
//! (`src/core/env.py`), the settings models and their `app_settings` store
//! (`src/core/settings.py`), logging (`src/core/logger.py`), filename helpers
//! (`src/tool/filename.py`) and a JSON writer that is byte-compatible with
//! Python's `json.dumps`.

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

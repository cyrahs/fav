//! PostgreSQL access for fav.
//!
//! Scope (filled in by WP1, see `rust/docs/PLAN.md`): the connection pool, the
//! one consolidated schema that replaces every per-source `_ensure_table()`,
//! and advisory locks keyed exactly like `src/tool/database.py`
//! (`advisory_lock_id`: blake2b with an 8-byte digest, read as a signed
//! big-endian integer). SQL is native PostgreSQL; there is no SQLite-dialect
//! translation layer.

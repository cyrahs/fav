//! The fav content sources, one module per entry of `JOB_SPECS`.
//!
//! Scope (filled in by WP5 to WP17, see `rust/docs/PLAN.md`). Every source
//! implements the runtime's `Source` trait and receives its HTTP client,
//! database, browser, Telegram client and subprocess runner by injection.

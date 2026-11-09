//! The worker runtime for fav.
//!
//! Scope (filled in by WP2, see `rust/docs/PLAN.md`): the notification outbox
//! and its Telegram Bot delivery, the control-request queue, the site-session
//! store, the scheduler (APScheduler `from_crontab` semantics) and the worker
//! loop, plus the source registry that mirrors `JOB_SPECS`.

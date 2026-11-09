//! Networking for fav.
//!
//! Scope (filled in by WP1 to WP7, see `rust/docs/PLAN.md`): the one factory
//! every HTTP client comes from (per-source proxy, `connect_to` target swap,
//! a swappable TLS backend), downloads streamed to `.part` files, and the
//! subprocess runner used for yt-dlp, gallery-dl and ffmpeg.

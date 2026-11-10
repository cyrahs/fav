//! WP0 API-coverage probe for the crates the plan flags as risks (the webp check moved
//! into the ugoira port in WP6, the CDP check into WP16 and the grammers check into WP15).
//!
//! Each module names, in compiled code, the API surface the matching Python
//! feature needs, so `cargo check` fails if a pinned version does not provide
//! it. The tests exercise what can be exercised offline: no Telegram server,
//! no website, no real account is ever contacted. Findings and the Python call
//! sites behind every item are in `rust/docs/API-COVERAGE.md`.
//!
//! Nothing here is product code: the real implementations land in their work
//! packages (Telegram in WP15, the CDP driver in WP16, MCP in WP4) and this
//! crate is deleted once they do.

pub mod mcp;

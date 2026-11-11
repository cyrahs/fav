# Conventions

How code in this repository is written and tested. fav was rewritten from Python to Rust in November 2026
(history in `rewrite/`); the last Python commit is tag `python-final` (67609f6).

## Ground rules

- Behaviour that users, stored data or other services see (API responses, MCP tools, file names, database
  rows, notification text) stays as it is unless a change is meant. Much of it is still Python's, copied on
  purpose; `DIVERGENCES.md` lists every place the Rust build behaves differently from Python and why. Add an
  entry when you change such behaviour on purpose.
- Never point a test or a dev build at the production database, the production `collection/` volume or a
  real account. Account-bound sources are tested under fakes only.
- Code, comments and docs in English (repository rule from `AGENTS.md`).

## Layout

```
Cargo.toml              workspace, shared dependency versions and lints
rust-toolchain.toml     pinned toolchain
crates/fav-core         env, settings, logging, filenames, Python-compatible JSON and text
crates/fav-db           pool, consolidated schema, advisory locks
crates/fav-net          HTTP client factory, streamed downloads, subprocess runner
crates/fav-runtime      scheduler, control queue, notifications, source registry
crates/fav-sources      one module per source
crates/fav-browser      CDP driver, login window
crates/fav-api          axum routes, MCP
crates/fav              the binary
goldens/                frozen expected outputs of the Python app (see Goldens)
tools/smoke             page smoke test of the front end against the API
tools/codegen           generator of fav-core's HTML entity table
docker/downloaders      pins of yt-dlp, gallery-dl and Playwright for the image
web/                    React front end, served by `fav serve` from web/dist
```

Many modules name the Python module they were ported from in their doc comment (for example
`src/web/kemono.py`); read it at tag `python-final`.

## Code

- Edition 2024, toolchain from `rust-toolchain.toml`. Add dependencies to `[workspace.dependencies]` in
  `Cargo.toml` and use `dep.workspace = true` in the crate; never two versions of one crate on purpose.
- Lints are workspace-wide (`[workspace.lints]`), CI runs clippy with `-D warnings`. `unsafe` is forbidden.
  `todo!()`, `unimplemented!()`, `dbg!()` are denied. No `#[ignore]` (CI's test step fails on any). An
  `#[allow(...)]` needs a comment saying why.
- No placeholder implementations in merged code.
- Errors: `thiserror` enums in library crates, `anyhow` only in the binary and tests. Error messages that
  Python surfaces to users (API `detail`, MCP tool errors, notification text) are copied verbatim.
- Logging through `fav_core::logging`: `let log = logging::get("jandan");` then `log.info(...)`,
  `log.notice(...)` and so on, which keeps Python's `embyx.<name>` logger names, the NOTICE level, `LOG_LEVEL`
  and the file format the MCP log tools parse. Other crates' `tracing` events from WARN up land in the same
  outputs. Tests read what was logged with `logging::capture`. Never log secrets (cookies, tokens, API
  hashes, proxy credentials, DSNs). Types holding one get a `Debug` that redacts it (see the settings
  models with `SENSITIVE_FIELDS`).
- Python-compatible data: `fav_core::pyjson` reads and writes JSON the way Python does (`1` and `1.0` stay
  apart, `json.dumps` byte formats), `fav_core::validate` reproduces pydantic's lax coercion and its error
  entries, `fav_core::pytext` has Python's `isspace`/`isdigit`/`strip`/`split`. Use them wherever Python's
  behaviour is observable (stored rows, API errors, filenames). For scraped pages:
  `fav_core::pyhtml` (CPython 3.12's `HTMLParser` tokenizer behind a handler trait, `html.unescape`), `pyurl`
  (`unquote`, `urlsplit`, `urljoin`), `pypath` (`PurePosixPath` joins, `name`, `suffix`, `stem`) and
  `pycasefold` (`str.casefold`). Python `re` patterns: `fav-sources/src/pyre.rs` compiles them as
  written, with `regex`, or `fancy-regex` where they need look-around or back-references, and rewrites `\s`
  to Python's class; see D73 for what is not translated.
- Database: `fav_db::Db` (pool and dedicated connections), `Db::ensure_schema` at startup (the only schema
  change mechanism: add tables, columns and indexes to `fav-db/src/schema.sql` in its three blocks, then
  update the snapshot below by hand), `Db::try_advisory_lock`, `Db::query_batch`/`insert_batch`, and
  `fav_db::SettingsStore` for `app_settings`. Write native PostgreSQL with `$n` placeholders.
- Test helpers live behind the `test-support` feature: `fav_core::golden` and `fav_db::testdb`. Add
  `fav-core = { workspace = true, features = ["test-support"] }` (or `fav-db`) to a crate's
  `[dev-dependencies]` to use them.
- `fav-runtime` has its own test helpers in `src/testutil.rs` (crate-private, `cfg(test)`): `database()`,
  `golden_json()`, and `FakeBotApi`, a local axum server that scripts Telegram Bot API answers and records
  every request (JSON or multipart). Telegram delivery is only ever tested against it, never the real bot.
- `fav-sources` has `src/testkit.rs` (crate-private, `cfg(test)`): `FakeServer`, a local axum server that
  answers every request through a closure and records it (httpx's `MockTransport`), and `RecordingNotifier`.
  Sources take a base URL override so tests point them at it.
- `fav-sources` has `src/testutil.rs` (crate-private, `cfg(test)`): `FakeWeb`, a local HTTP/1.1 proxy that
  every test client is pointed at (`ClientOptions::proxy`), so sources keep their real `http://` hosts; a
  handler returns `Reply::status`/`bytes`/`json`, `Reply::Close` (httpx's `RemoteProtocolError`) or
  `Reply::Stall` (a `ReadTimeout` with a short client timeout), and `seen()` records each request.
  Retry and pacing sleeps go through `fav_net::retry::Sleeper`; tests pass `RecordingSleeper` and assert
  the delays. A test that never touches the database can use `Db::connect_lazy` on an unreachable URL.
- Async tests that read `logging::capture` (thread-local) run on the current-thread runtime
  (`#[tokio::test]`) and poll the future inside the capture.
- All external effects are injected: HTTP client, database pool, clock, browser, Telegram client, subprocess
  runner, filesystem root. A source never builds its own client.
- Memory: stream downloads to `.part` and rename; stream large result sets; write manifests in batches; never
  hold a whole table in memory.
- `serde_json` with `preserve_order` (schemas and payloads keep key order). Python's `json.dumps` byte format,
  where stored data depends on it, goes through `fav-core`'s writer, never `serde_json::to_string`.
- Format with `cargo fmt` (`rustfmt.toml`: width 140).

## Tests

- Tests that were ported from Python carry the Python test id on the line(s) directly above their test
  attribute (`// py: tests/test_kemono.py::test_parses_creator_page`). Keep these lines; they map to the
  Python tests at tag `python-final`. Never weaken an assertion: if a test checks an exact string, keep it.
- Database tests run against real PostgreSQL, one temporary database per test, created through
  `FAV_TEST_DATABASE_URL` (CI: a PostgreSQL 17 service). Locally, start one from the PostgreSQL binaries or
  Docker. Tests that need it and do not find it fail; they do not skip.
- Chromium tests use `FAV_TEST_CHROMIUM` (CI sets it; cloud sessions fall back to `/opt/pw-browsers/chromium`).
- No test talks to the internet. HTTP is faked with a local server or an injected client.
- No `#[ignore]`d tests.

## Goldens

`goldens/<area>/` holds deterministic outputs of the Python app (JSON byte formats, settings validation,
scheduler fire times, page parsers, API and MCP exchanges, and so on). Rust tests read them with
`include_str!` or from `env!("CARGO_MANIFEST_DIR")/../../goldens/...` (`fav_core::golden`) and compare.

They are frozen: the generator (`rust/tools/goldens/` at commit 805f5c0, the last commit with the Python app
and the Rust port side by side) ran the real Python code, which is gone from this branch. To regenerate an
area, check out 805f5c0 and run `UV_PYTHON=3.12 uv run python rust/tools/goldens/generate.py` there. When behaviour changes on purpose,
edit the affected golden by hand in the same PR and say so in the PR and in `DIVERGENCES.md`.

## Schema snapshot

`crates/fav-db/src/schema/snapshot.txt` describes the production schema, one line per table, column,
constraint and index, as `crates/fav-db/src/schema/describe.sql` prints it. It was generated from the Python
app (`rust/tools/schema/dump_schema.py` at commit 805f5c0) and is now maintained by hand: `fav-db`'s
schema test applies `schema.sql` to a temporary database and compares the description with it, so a schema
change updates both files in one PR.

## Before you push

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
FAV_TEST_DATABASE_URL=postgresql://... cargo test --workspace --locked
```

`fav-core/src/pyhtml/entities.rs` is generated: `UV_PYTHON=3.12 uv run tools/codegen/html_entities.py --check`.
The page smoke test (`tools/smoke/web_smoke.py`, see its docstring) runs in CI; run it locally when a change
touches the API the front end calls.

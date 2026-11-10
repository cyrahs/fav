# fav Rust rewrite: plan and status

Agreed with Max on 2026-11-09. The Chinese original is authoritative for the wording Max approved:
https://claude.ai/artifact/UDKBVGgzKHb9yZhVoAHBhR

This file is the source of truth for status. Every work-package PR updates the status table and the
experiment log below in the same PR. Read this file, `CONVENTIONS.md`, `PARITY.md`, `DIVERGENCES.md` and
`API-COVERAGE.md` before starting a work package.

## Fixed decisions (Max)

1. Scope: full parity. All 13 jobs in `JOB_SPECS` + API + MCP + site-login page. Checkpoint A after phase 2.
2. Verification: level 1 offline checks (mandatory) + level 2 read-only replay on a scrubbed copy of the
   production DB (ask Max before taking the export) + level 3 capped live runs for anonymous sources only.
3. Baseline: commit `67609f60ab9fd1065601aaaa60ac04a22044c406` (main HEAD and the deployed commit on 2026-11-09).
   Do not chase `main` during the rewrite. A catch-up is decided after the end.
4. Merging: PRs may be merged into branch `rust` without asking Max. The plan's own gate still applies:
   CI green + one independent review by a fresh session. `rust` -> `main` always needs Max.
5. Parallelism: phases 0-2 serial. From phase 3 at most 2 threads in parallel.
6. Models: coding with Opus 5.5 at high effort (`claude-opus-5-5`). Planning and checkpoint reviews with Fable 5.2 high.
7. Telegram: whether a real account may be logged in for verification is asked before phase 5 starts.

## Hard safety rules

- Never deploy. No image publish, no change to charys117/nas-gitops. The `rust` branch CI has checks only.
  (fav CI publishes `latest` only from `main`; Flux follows `latest` by digest.)
- Never point the Rust build at the production database, the production `collection/` volume, or a real account.
- Never run live: Bilibili (clears watch-later), WeChat (inbox ack deletes the host copy, `/link/open` makes the
  account open links), RedNote (irreplaceable account, risk control), X, Pixiv, TikHub (billed per call).
  These are verified under mocks only and reported as "not verified against the real service".
- Azur Lane: replay only (origin reachable only from nas-us-1).
- The Python tree on `rust` stays untouched. It is the reference and the golden generator. Do not merge `main` into `rust`.
- PRs target `rust`, never `main`.
- Never run `ci.yml` by hand (`workflow_dispatch`) and never push a tag from this branch: `ci.yml` cannot be
  changed here, and either would build and push an image from `rust` (not tagged `latest`, so Flux would not
  pick it up, but it is still a publish).

## Goals and non-goals

Goals: behaviour parity with the baseline; drop-in replaceable (same env vars, same DB schema, same HTTP and MCP
surface, same file names and directory layout); idle RSS under 50 MiB (Python: ~300 MiB across 3 processes);
Azur Lane full walk no longer bloats the worker; an experiment log (rework count and review findings per WP).

Non-goals: rewriting yt-dlp / gallery-dl / ffmpeg / Chromium; touching `web/` (React) or `script/`;
new features or behaviour changes (Python bugs are logged, copied unless they corrupt data);
legacy baggage (SQLite-dialect translation layer, CookieCloud cleanup, old-field startup migrations, tqdm bars).

## Current system (baseline facts)

- 41,887 lines Python (`run.py` + `src/`), 29,788 lines of tests (1,074 tests), 4,460 lines React.
- `run.py` + `src/core` + `src/service`: 2,286 lines. `src/tool`: 8,495. `src/web`: 22,126. `src/api`: 8,980.
- 3 processes (supervisor, API, worker) coordinating only through PostgreSQL. 43 tables, 209 SQL call sites,
  no migration framework, SQL written in SQLite dialect and translated.
- 44 FastAPI routes, 18 MCP tools. Consumers: bundled React UI, game-view, RSSHub (credentials endpoint),
  Hanime1 userscript, LiteLLM MCP gateway.
- External programs: yt-dlp (Bilibili, Hanime1), gallery-dl (X), ffmpeg, Chromium (RedNote crawl, Nikke runtime
  capture, login windows via Xvnc + noVNC).

## Technology mapping

| Python | Rust | Notes |
| --- | --- | --- |
| FastAPI, uvicorn, pydantic | axum + serde, utoipa for OpenAPI | Same paths, fields, error codes; OpenAPI diff enforces it |
| `mcp` | rmcp, stateless streamable HTTP, JSON responses | Tool names and input schemas golden-diffed |
| psycopg + SQLite-dialect translation | sqlx, native PostgreSQL SQL | Same schema; no translation layer |
| APScheduler | own scheduler loop + cron parser | APScheduler `from_crontab` semantics (e.g. weekday numbering) locked by next-fire-time goldens; coalesce, max_instances=1, misfire grace 300 s |
| httpx | reqwest | Per-source proxy; `connect_to` needs a custom connector (swap the TCP target, keep SNI/Host, also through a proxy tunnel); TLS fingerprint differs from Python |
| Telethon | grammers | Biggest unknown: album grouping, FloodWait, media download, reconnect, session format |
| Playwright | raw CDP (chromiumoxide or similar) | Same Chromium build and launch arguments as Playwright uses today; only the driver changes |
| yt-dlp, gallery-dl | unchanged, subprocess | Image keeps a small Python env with only these two; dependabot path for gallery-dl stays |
| ffmpeg, Xvnc, tini | unchanged | |
| bilibili-api-python | direct calls to the 5 endpoints used | watch-later list and clear, favourite list content, video info and detail |
| Pillow (ugoira -> animated webp) | libwebp binding | Not byte-identical; compare frame count, durations, size |
| pycryptodome | RustCrypto (AES-CBC, AES-GCM, RSA-OAEP) | Golden vectors |
| opencc-python (t2s) | port the package's own dictionaries + same matching algorithm | Hanime1 directory names depend on it; must match exactly |
| `re` | `regex`, `fancy-regex` where look-around is needed | Parser outputs golden-diffed |
| colorlog, tqdm | tracing | Log files still under `log/` (MCP `read_logs`); progress bars become periodic log lines |

Versions of the key crates are pinned in `rust/Cargo.toml` after the WP0 API-coverage check; the findings, and
what each later work package must do differently because of them, are in `API-COVERAGE.md`.

## Target architecture

- One binary `fav`: `fav serve` (default, API + worker in one process), `fav api`, `fav worker`, `fav trigger <job|all>`.
- Cargo workspace under `rust/`, 8 crates: `fav-core` (env, settings models, logging, filenames, Python-compatible
  JSON), `fav-db` (pool, schema, advisory locks), `fav-net` (HTTP client factory, streamed downloads, subprocess
  runner), `fav-runtime` (scheduler, control-request queue, notifications, source registry), `fav-sources`
  (one module per source), `fav-browser` (CDP driver, login window, RedNote browser), `fav-api` (axum routes, MCP),
  `fav` (entry point).
- A `Source` trait (`update()`, optional `close()`); registry mirrors `JOB_SPECS`. All external dependencies
  (HTTP, DB, browser, Telegram client, subprocess) are injected so tests can replace them.
- DB: identical table names, columns, types, indexes; identical advisory-lock id algorithm (blake2b-8, signed BE).
  One consolidated schema instead of per-source `_ensure_table()`.
- Settings: identical `app_settings` JSON shape, defaults, validation, secret masking round trip; edits apply
  without restart.
- Memory rules: stream downloads to `.part`; stream large result sets with cursors; write manifests in batches;
  never hold a full table in memory. Allocator chosen by measurement in WP18.
- Code rules: no `unsafe`; clippy warnings are errors; no `todo!()`, `unimplemented!()` or ignored tests in merged code.
- Repo rule from AGENTS.md still applies: code, comments and docs in English.

## Verification

Level 1, offline, all in CI:
1. Test parity. Every Python test has a Rust counterpart annotated with its origin
   (`// py: tests/test_kemono.py::test_x`). A script checks all 1,074 are covered; non-applicable ones go to a
   waiver list with a reason. Port the intent of the assertions, not the mocking mechanics: DB-touching tests run
   against real PostgreSQL (one temporary database per test).
2. Goldens. A Python script run on the baseline tree dumps deterministic outputs that are committed; Rust tests
   compare byte for byte. Cover: settings defaults and validation results, filename sanitising, notification
   rendering, cron next-fire times, signatures and decryption (WBI, Tieba sign, AZP2, ISAAC-64, Jandan AES),
   page-parser outputs, t2s conversion, manifest generation for the three games, and a JSON writer byte-compatible
   with Python `json.dumps` (stored hashes are sha256 over `json.dumps(sort_keys=True, separators=(',',':'),
   ensure_ascii=False, default=str)` bytes). Goldens come from Python, never hand-written by the porting session.
3. API contract. Structural diff of `/openapi.json` and the MCP tool list against Python's. The unmodified React
   front end passes a page smoke test against the Rust API with a seeded database.
4. Schema. Both sides create their schema on an empty database; the descriptions are identical (since WP1 a
   catalog query, `rust/tools/schema/describe.sql`, instead of a `pg_dump --schema-only` diff; `CONVENTIONS.md`).

Level 2, read-only replay: scrubbed production dump (no `site_sessions` data, `app_settings` secrets masked) in a
temporary database; Python API and Rust API both point at it and every GET endpoint is diffed; game manifests are
regenerated from real data and compared with the production files. No external requests, no production writes.

Level 3, capped live runs into a temporary directory and database, anonymous sources only: Jandan, Kemono,
Stella Sora, BD2, Nikke, Hanime1 (parse only, no downloads).

## Work packages

| WP | Phase | Content | Python lines (approx.) | Status |
| --- | --- | --- | --- | --- |
| WP0 | 0 foundation | Workspace skeleton, CI, in-repo plan and conventions docs, golden generator, test-parity script, API-coverage check of the four key crates | new | done |
| WP1 | 1 core runtime | Env, settings models and store, DB layer and consolidated schema, logging, filenames, Python-compatible JSON | 1,900 | done |
| WP2 | 1 core runtime | Notification outbox and Telegram Bot delivery, control-request queue, site-session store, scheduler and worker loop, `trigger` | 2,900 | done |
| WP3 | 2 API and MCP | axum skeleton, auth, settings, jobs, archive, credentials, health endpoints, serving the front end | 3,700 | pending |
| WP4 | 2 API and MCP | 18 MCP tools and ops queries | 900 | pending |
| WP5 | 3 HTTP sources | Jandan, Kemono | 1,700 | pending |
| WP6 | 3 HTTP sources | Stella Sora, Pixiv | 1,700 | pending |
| WP7 | 3 HTTP sources | Bilibili, X, with the yt-dlp and gallery-dl subprocess wrappers | 1,500 | pending |
| WP8 | 3 HTTP sources | Hanime1 with series, author subscriptions, ranking discovery and its endpoints | 2,600 | pending |
| WP9 | 4 game sources | Nikke crawl, layer metadata, manifests, API reader | 4,300 | pending |
| WP10 | 4 game sources | BD2 crawl, manifests, API reader, Live2D view overrides | 4,200 | pending |
| WP11 | 4 game sources | Azur Lane source parsing, AZP2, `connect_to` | 3,900 | pending |
| WP12 | 4 game sources | Azur Lane crawl, manifests, API reader | 3,400 | pending |
| WP13 | 5 message sources | Post links: WeChat articles, Tieba, NGA, Zhihu, TikHub | 1,300 | pending |
| WP14 | 5 message sources | WeChat with Channels videos and stickers | 1,700 | pending |
| WP15 | 5 message sources | Telegram: grammers client, durable queue, realtime listener | 1,800 | pending |
| WP16 | 6 browser | CDP driver layer, login window (Xvnc, VNC relay, cookie polling) | 1,100 | pending |
| WP17 | 6 browser | RedNote, Nikke runtime capture | 3,100 | pending |
| WP18 | 7 wrap-up | Dockerfile (build only, never published), memory benchmark, parity audit, final report | new | pending |

Status values: pending, in progress (PR open), done (merged into `rust`).

Checkpoints (a short review for Max, who decides to continue, adjust or stop):
- A after WP4: the existing front end works against the Rust backend (no sources yet). Cheapest stop point.
- B after WP12: level 2 replay matches; memory comparison numbers.
- C after WP18: final report; Max decides on catch-up with `main` and anything further.

Each WP includes porting the tests in its scope (test code is roughly 0.7x the listed lines). WP9 and WP10 may be
split into "crawl" and "manifests + API" if one thread cannot finish them.

## How threads work

- One WP = one thread = one PR. Branch from `rust`, PR into `rust`.
- Handoff lives in the repo, not in memory: `rust/docs/PLAN.md` (WP list and status), `CONVENTIONS.md` (coding and
  test conventions), `PARITY.md` (generated test-parity table), `DIVERGENCES.md` (intentional differences from
  Python). Read them first, update them last. `rust/BASELINE` holds the baseline commit.
- Definition of done for a PR: CI green (fmt, clippy, tests, parity script, goldens); every Python test in scope
  mapped or waived with a reason; no placeholder implementations; `PLAN.md` status updated.
- Independent review before merge by a fresh session against the Python source, looking for weakened test
  assertions and missed behaviour. Findings are recorded on the PR as part of the experiment log.
- Cloud sessions already have Rust 1.97, PostgreSQL 16 server binaries, Docker, uv and Chromium, and can reach crates.io.
- Tooling from WP0: `rust/tools/goldens/generate.py` (goldens), `rust/tools/parity/parity.py` (test parity),
  `rust/tools/check_tree.sh` (Python tree untouched, no ignored tests), `.github/workflows/rust.yml` (CI).
  `CONVENTIONS.md` says how to use each.

## Known risks

- Account-bound sources cannot be run live, so green checks do not prove production readiness. Report the
  verification level per source.
- grammers may lack something Telegram needs. If so, mark Telegram "not ported" rather than block the other 12.
- Dropping Playwright changes how Chromium is driven; copy today's launch arguments and init scripts.
- AI weakening tests: goldens from Python, parity script in CI, independent review.
- Hidden Python semantics that stored data depends on: `json.dumps` byte format, APScheduler weekday numbering,
  the t2s dictionaries, filename sanitising.
- reqwest vs httpx TLS fingerprint: keep the client in one factory so the TLS backend can be swapped.
- Size: an estimated 50-60k lines of Rust plus tests, about 19 threads.

## Experiment log

One row per merged work package. "Rework" counts pushes after the PR was opened that fixed something CI or
the independent review found; review findings are summarised with a link to the PR comment.

| WP | PR | Rework | Independent review findings | Notes |
| --- | --- | --- | --- | --- |
| WP0 | #133 | 1 push after review | 0 blockers, 8 should-fix, 7 nits: goldens could be generated from a shadow copy of `src/` under `rust/`; `cfg_attr(..., ignore)` slipped past the ignore check; tests could be parked in an invented WP or moved out of a done one; `ci.yml` dispatch/tags could publish from `rust`; APScheduler DST repeat recorded without a note; JSON and filename goldens missed formats and edge cases; `min_id` advice wrong and RedNote click/scroll/reload/cookie-filter APIs missing from the coverage check. All fixed in the PR except `ci.yml` (rule added above) | Probe crate exercises a real Chromium in CI |
| WP1 | #134 | 1 push after review | 0 blockers, 6 should-fix, 8 nits: schema script ran as one implicit transaction (now statement by statement, failures collected) and the bilibili constraint lacked Python's lock; sqlx's forced UTC session time zone unrecorded (D11, server is UTC); `.env` parser skipped bad lines from the wrong place; logging golden bypassed Python's float `msecs` (now copied exactly); key/value DSNs refused (now parsed); secrets reachable through `Debug`; plus weak/narrow test checks, D9 wording, uncommented `allow`, test helpers public outside `test-support`, sequence ownership missing from the schema description. All fixed in the PR | Schema compared through a catalog description (`rust/tools/schema`) instead of `pg_dump` |

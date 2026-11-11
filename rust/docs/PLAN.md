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

- Never deploy. No image publish, no change to charys117/nas-gitops. The `rust` branch CI had checks only.
  (fav CI publishes `latest` only from `main`; Flux follows `latest` by digest.)
  Lifted by Max on 2026-11-10 23:34Z for the direct production deployment: `.github/workflows/rust-image.yml`
  publishes `ghcr.io/cyrahs/fav:rust-<short sha>` on every push to `rust` (never `latest`), and nas-gitops pins it
  by tag and digest. The one hard rule from then on is that no data is lost or damaged.
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
| `mcp` | stateless streamable HTTP with JSON responses, written in `fav-api` (rmcp checked, not used: D31) | Tool list, protocol answers and tool calls golden-diffed |
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
  never hold a full table in memory. Allocator chosen by measurement in WP18: jemalloc (`tikv-jemallocator`), see docs/REPORT.md.
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
| WP3 | 2 API and MCP | axum skeleton, auth, settings, jobs, archive, credentials, health endpoints, serving the front end | 3,700 | done |
| WP4 | 2 API and MCP | 18 MCP tools and ops queries | 900 | done |
| WP5 | 3 HTTP sources | Jandan, Kemono | 1,700 | done |
| WP6 | 3 HTTP sources | Stella Sora, Pixiv | 1,700 | done |
| WP7 | 3 HTTP sources | Bilibili, X, with the yt-dlp and gallery-dl subprocess wrappers | 1,500 | done |
| WP8 | 3 HTTP sources | Hanime1 with series, author subscriptions, ranking discovery and its endpoints | 2,600 | done |
| WP9 | 4 game sources | Nikke crawl, layer metadata, manifests, API reader | 4,300 | done |
| WP10 | 4 game sources | BD2 crawl, manifests, API reader, Live2D view overrides | 4,200 | done |
| WP11 | 4 game sources | Azur Lane source parsing, AZP2, `connect_to` | 3,900 | done |
| WP12 | 4 game sources | Azur Lane crawl, manifests, API reader | 3,400 | done |
| WP13 | 5 message sources | Post links: WeChat articles, Tieba, NGA, Zhihu, TikHub | 1,300 | done |
| WP14 | 5 message sources | WeChat with Channels videos and stickers | 1,700 | done |
| WP15 | 5 message sources | Telegram: grammers client, durable queue, realtime listener | 1,800 | done |
| WP16 | 6 browser | CDP driver layer, login window (Xvnc, VNC relay, cookie polling) | 1,100 | done |
| WP17 | 6 browser | RedNote, Nikke runtime capture | 3,100 | done |
| WP18 | 7 wrap-up | Dockerfile (build only at the time; published as `rust-<sha>` since 2026-11-10), memory benchmark, parity audit, final report | new | done |

Status values: pending, in progress (PR open), done (merged into `rust`).

Checkpoints (a short review for Max, who decides to continue, adjust or stop):
- A after WP4: the existing front end works against the Rust backend (no sources yet). Cheapest stop point.
- B after WP12: level 2 replay matches; memory comparison numbers.
- C after WP18: final report; Max decides on catch-up with `main` and anything further.

Decision (Max, 2026-11-10): no stops on the way. A and B are not reviews any more, only notes in the
experiment log when they are passed; the work continues straight through to WP18, and only the final report
(C) goes to Max. Threads still ask Max for what only he can decide: the production DB export for level 2 and
the Telegram real-account question before phase 5. A WP is never sent to Max for review.

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

## Cleanup notes

Shared pieces that parallel work packages built twice. WP18 closed them; what it left on purpose is listed
with the reason (also in docs/REPORT.md).

Closed in WP18:

- WP6's `fav-sources/src/kit.rs` is gone: its callers use `fav-net`'s httpx-shaped errors, streamed `.part`
  downloads and `move_file`; `kit::status_error_message`'s golden test (`pyweb/httpx_status_errors.json`)
  runs against `fav-net`. The notification helpers it carried moved to `fav-sources/src/notifier.rs`.
- One httpx proxy-scheme rule in `fav-net`: Pixiv now applies it, and RedNote's `proxy_probe` and Azur Lane's
  `check_origin_proxy` call it instead of their own copies.
- The JSON byte-encoding detection (`json.detect_encoding`) lives once in `fav-core`; `fav-api`'s MCP and
  `fav-sources`' `pydecode` both use it.
- Pixiv, Stella Sora, Bilibili and X sleep through `fav_net::retry::Sleeper`, and their tests assert the waits.
- Hanime1 runs yt-dlp through `fav_net::subprocess::Runner`; its private `CommandRunner` is gone and the
  universal-newline test runs on the shared runner.
- NIKKE and BD2 share one GameKee module (`fav-sources/src/gamekee.rs`), each side's golden tests kept; Azur
  Lane's crawl takes its blob and hashing helpers from there.
- Jandan's base64 decoding uses `pybase64` (Jandan's golden kept).
- The BD2-L2D-Viewer client is injected instead of built inside the crawler.
- The `probes/api-coverage` crate and its `rmcp` pin are removed.

Left on purpose:

- Azur Lane's crawl still takes `retry_after_delay_seconds` / `retry_delay_seconds` from `bd2::extract`: they
  are BD2's copies of the same ladder, and moving two small functions is not worth a third golden run.
- `pyre`, `pyval`, `mcp::fromisoformat` and the two loopback test servers (`fav-sources` `FakeWeb`,
  `fav-browser` `serve`) stay where they are: each note said "move when a third caller appears", and none did.
- RedNote's `sleep_request_seconds` pause, the Telegram protocol waits (flood waits, reconnect backoff, the
  queue's idle poll) and the browser's millisecond polls call `tokio::time::sleep` directly: they are protocol
  timing, not retry ladders, which is what the Sleeper note was about.
- `ClientOptions::trust_env` (WP13), `ClientOptions::roots` and `FakeWeb`'s TLS tunnel (WP14), `Reply::after`
  (WP17), the summary cache, the Live2D overrides and the shared schema table (WP9 to WP12) were notes of
  where a shared piece lives, not duplicates; nothing to fold.
- WP16's note for WP18 is met: the image puts Chromium at Playwright's registry path and Xvnc on `PATH`.

## Experiment log

One row per merged work package. "Rework" counts pushes after the PR was opened that fixed something CI or
the independent review found; review findings are summarised with a link to the PR comment.

| WP | PR | Rework | Independent review findings | Notes |
| --- | --- | --- | --- | --- |
| WP0 | #133 | 1 push after review | 0 blockers, 8 should-fix, 7 nits: goldens could be generated from a shadow copy of `src/` under `rust/`; `cfg_attr(..., ignore)` slipped past the ignore check; tests could be parked in an invented WP or moved out of a done one; `ci.yml` dispatch/tags could publish from `rust`; APScheduler DST repeat recorded without a note; JSON and filename goldens missed formats and edge cases; `min_id` advice wrong and RedNote click/scroll/reload/cookie-filter APIs missing from the coverage check. All fixed in the PR except `ci.yml` (rule added above) | Probe crate exercises a real Chromium in CI |
| WP1 | #134 | 1 push after review | 0 blockers, 6 should-fix, 8 nits: schema script ran as one implicit transaction (now statement by statement, failures collected) and the bilibili constraint lacked Python's lock; sqlx's forced UTC session time zone unrecorded (D11, server is UTC); `.env` parser skipped bad lines from the wrong place; logging golden bypassed Python's float `msecs` (now copied exactly); key/value DSNs refused (now parsed); secrets reachable through `Debug`; plus weak/narrow test checks, D9 wording, uncommented `allow`, test helpers public outside `test-support`, sequence ownership missing from the schema description. All fixed in the PR | Schema compared through a catalog description (`rust/tools/schema`) instead of `pg_dump` |
| WP2 | #135 | 1 push after review | 0 blockers, 5 should-fix, ~10 nits: misfire lateness subtracted wall times, so a run in a spring-forward gap was always skipped (now instants, gap test added); two ported tests could not catch their bug under mutation (startup order before the control consumer, server `retry_after` vs backoff), both strengthened and re-checked by mutation; D16 described statuses wrongly; the notification loop's survival of config errors was an unrecorded Python-bug fix (D20). Nits fixed: never-firing jobs are kept paused, first fire times computed at scheduler start, `fav trigger all` checks every runner first, readiness wait honours the stop signal, Bot client no longer follows redirects, `round(v, 3)` and `-0` expiry match Python, D13/D18 corrected. Left: `repr_str` escapes fewer non-printables than `repr` (logs only) | Telegram delivery replayed against a local fake Bot API; Python's delivery goldens record the requests it makes |
| WP3 | #136 | 1 push after review | 0 blockers, 3 should-fix, 4 nits: graceful shutdown had no deadline, so a stalled connection kept `fav api`/`fav serve` up after SIGTERM (now 10 s, as the launcher's kill timeout, with a stalled-connection test); `/openapi.json` paths and operations were not in Python's order and the contract test compared order-blind values (utoipa `preserve_order`/`preserve_path_order`, operations reordered, order now asserted; keyword order inside a schema recorded as D29); a non-UTF-8 raw body is a 500 in Python and a 422 in Rust (D30). Nits: `/assets` dot segments now resolved like `normpath`; the media-type table is now Python 3.12's full built-in one (D25 was wrong about `.webp`/`.woff`); the smoke test also fails on requests that get no response; launcher log differences added to D28 | Golden replay of every recorded request over real PostgreSQL; the unmodified React front end is smoke-tested in CI |
| WP4 | #137 | 1 push after review | 0 blockers, 5 should-fix, 6 nits: `list_jobs`/`run_job`/`send_test_notification` show `Internal server error.` on a database error where Python shows the driver's message (now in D37); the SDK's own log lines were not written (the three fixed-text warnings now are, under the SDK's logger names and recorded in the goldens; the repr/traceback lines are D38); `read_logs` read files on the async workers (now `spawn_blocking`); the `update_settings` test did not merge a nested object (now `web.hanime1` `ranking.deep_scan`); no golden ran outside UTC (now `zoned` cases: log stamps and next fire times in five zones around the 2026 DST changes, skipped and ambiguous local times included). Nits fixed: an empty `Last-Event-ID` opens the stream; non-finite floats in the parameters become `None` before validation, as `model_dump(mode='json')` does; the 413 body limit matches the decoded path; `casefold` is Python 3.12's full table (1,530 characters, checked against every code point; Rust's newer Unicode lowercases 55 letters Python 3.12 leaves alone); the database-time pinning in the replay is bounded to the last 10 minutes; D24 now covers `search_archive` | The FastMCP transport is reimplemented on axum (rmcp unused, `API-COVERAGE.md`); the replay compares every statement Python's `query_db` would run |
| WP5 | #139 | 1 push after review | 1 blocker, 3 should-fix, 8 nits: reqwest wraps every body-read failure as a decode error, so a connection cut mid-body was `DecodingError` and Kemono and Jandan gave up where httpx retries (now classed from the error's source chain: `RemoteProtocolError` for an incomplete message, `ReadError` for a reset; fake-server truncate and reset replies, tests for each path, D18 corrected); two ported retry tests used a different failure from Python's (now a reset for Kemono's `ReadError`; Jandan's `ConnectTimeout` checked on the predicate, since a local proxy cannot produce one, plus a body cut short end to end and a reset that must not be retried); D40 claimed no decision could differ; a Jandan comment misdescribed what is retried. Nits fixed: pacing test, `int()`'s 4300-digit limit for bare creator ids (golden), `Path('.') / x` stored without `./` (`PyPath::join`), a redirect body read failure now ends the request, `regex` a workspace dependency, header-name case and order and orphaned `.part` files recorded (D41, D39) | Shared fav-net pieces (`HttpClient`, `download::to_temp_file`, `retry`) and fav-sources' `FakeWeb` are for the later HTTP sources; WP6 ran in parallel. Checkpoint A (after WP4) passed as a note only, per Max's no-stop decision |
| WP6 | #138 | 1 push after review | 0 blockers, 4 should-fix, 8 nits: a Pixiv ajax body that failed while being read was not retried (httpx reads it inside `client.get()`; now inside the retry loop, and a body cut short maps to `RemoteProtocolError` as in httpx, with a truncating-server test); the masked-bookmark test passed without the `UPDATE` it targets (now seeds pending and downloaded rows); Stella Sora's JSON decode error lacked Python's line and column (`LoadError::python_message`, golden `pyweb/json_errors.json`); the collection walk and cross-device moves blocked the async threads (now `spawn_blocking`). Nits fixed: POSIX `zipfile` keeps backslashes in member names, `imageinfo[0]` on an object is a `KeyError`, Pillow's frame-size error text, cross-device moves keep times and go through `.part`, a test name, a missing class assertion; proxy and same-host-redirect cookie differences added to D50. Left: `urlsplit`'s IPv6 error text does not repr-escape the host (malformed URLs only) | Ugoira via webp-animation; Python-exact `HTMLParser`, `urllib.parse` and `pathlib` helpers in fav-core; differential fuzz of them against CPython (about 39k inputs) by the reviewer found that one mismatch only |
| WP7 | #140 | 1 push after review | 1 blocker, 7 should-fix, 6 nits: `run_lines` lost a last line without a newline when the other pipe won the `select!` race (now flushed at the end, with a scripted race test that fails without the fix); the fake server swallowed handler panics and recorded a request only after its handler, so "nothing is asked" checks were vacuous (now recorded first, and a handler panic fails the test when the server is dropped); the renewal tests did not check that a renewed ticket is stored on its own, nor take Python's "ticket not due" path, and nothing caught a lost `ON CONFLICT ... DO UPDATE` (tests added, each checked by mutation); D67 was wrong (reqwest takes `host:port` and unknown schemes; now httpx's scheme rule, tested); file moves and cache cleanup ran on the async threads (now `spawn_blocking`, as Python's `to_thread`); crate-local `hmac`/`rsa`/`sha2` versions (now workspace dependencies, sha2 0.11 everywhere fav uses it). Nits fixed: `{"bvid": null}` is bilibili-api's `ArgsException`, D62/D63/D65 corrected, D68 added, no-request tests point at an unroutable host. Left: sleeps do not go through `retry::Sleeper` (as Pixiv and Stella Sora), so the batch pause and the 30 s lookup delay are not asserted | Bilibili's web API calls are made directly (bilibili-api-python's request shapes held to goldens); yt-dlp and gallery-dl run through a `Runner` trait, scripted in tests; WP8 ran in parallel |
| WP8 | #141 | 1 push after review | 0 blockers, 1 should-fix, 7 nits: `/readyz` loaded its targets through the pool, whose connect retries outlast the 12 s budget, so a refused database answered `upstream_timeout` after 12 s instead of `database_unavailable` at once (now a connection of its own, as Python's `psycopg.connect`, with a test). Nits fixed: a malformed link on a seed's watch page (`urlsplit`'s `ValueError`) is 422 `invalid_seed` as in Python, not 500; `strip_uploader_prefix` matched through a regex built from the page's uploader, which panicked on a huge one (now matched by hand, golden cases unchanged); golden arms that only record Python's answer for non-string arguments are marked as such; the cache directory, `ensure_unique_path` and the move run off the async threads; a missing watch URL in the download notification raises as in Python. Left as recorded (D77): a `/readyz` whose client disconnects is not cached | t2s held to every dictionary key of the Python package; 576 parser cases; ran in parallel with WP7, so `yt-dlp` uses a private runner (cleanup note) |
| WP9 | #142 | 1 push after review | 0 blockers, 2 should-fix, 3 nits: the asset concurrency test measured the CDN's own limit of 3, not the asset-group limit, so it passed with any group limit (now the CDN limit is lifted in the test, checked by mutation); D84/D85 named a `runtime-capture` command the script does not have (now `capture-missing` and `backfill-runtime-layers`); D83 left out that the same `to_int` also changes the crawl's tj-list filter and the script's content-id lookups; D87 said "64 bits" for "past `i64::MAX`". Left: the crawl's manifest and snapshot reads and writes run on the async threads as Python's do on its event loop (hashing and blob links already go through `spawn_blocking`) | API responses shaped by a schema table that also generates their OpenAPI components; goldens replay the Python routes through TestClient; ran in parallel with WP10, which wires the Live2D view overrides (cleanup notes) |
| WP10 | #143 | 1 push after review | 0 blockers, 2 should-fix, 5 nits: the ported `update_reports_failures_after_attempting_remaining_pages` lost Python's check that the run upserts the tree with `deactivate_missing=True`, so a crawl that never retires vanished pages passed (now a seeded vanished page must end inactive, checked by mutation); a huge `offset` on `/bd2/characters` was echoed as `i64::MAX` without an entry (now echoed up to 128 bits, D99). Nits fixed: the dropped `get_asset_file` gets its own entry (D100); D94 lists `_filter_gamekee_character_rows`; a doc comment named a `bd2-l2d-viewer` command that does not exist; request pacing (limiter spacing, `Retry-After`, backoff) and the circuit-breaker pause are now asserted through the recording sleeper; the cooldown test checks the asset row was touched; D97 also covers the crawl's `_to_int`. Left as a cleanup note: the BD2-L2D-Viewer fetch builds its own `HttpClient`, as Python does | Merged second after WP9: folded onto WP9's summary cache, Spine probe and content discovery, and wired the Nikke view overrides and detail overlay on the shared `live2d_overrides` module (the Nikke golden now replays Python's overlay) |
| WP11 | #144 | 1 push after review | 0 blockers, 3 should-fix, 3 nits: `_ServerNameContext` swaps the server name on every handshake made to the connect host, so Python verifies the origin's own `static.l2d.su` URLs as `l2d.su` while Rust sent them with their own name (now an HTTPS request for the connect host takes the route too, with a TLS test, D110); a refused tunnel without a connect host reads `tunnel error: unsuccessful` on reqwest's path, not the status line D111 claimed (D111 corrected); the probe accepted a bare `host:port` proxy that httpx refuses (now `check_origin_proxy`, D113 for SOCKS as D67). Nits fixed: the connect_to test's other host shared the substitute's address, so moving every host passed (now on 127.0.0.2, checked by mutation); `BuildError::ProxyUrl` kept the proxy URL with its credentials in `Debug` (now dropped); a golden case pins `pin_archived_entries`' priority order (each part checked by mutation) | WP12 builds the origin client with `check_origin_proxy` + `l2d_su_origin_client_options` and reuses `fetch_azurlane_l2d_health_report`; the API's proxy test is already wired through `ApiState::azurlane_origin` |
| WP12 | #147 | 1 push after review | 0 blockers, 1 should-fix, 3 nits: the API reused BD2's `_normalize_asset_path`, which collapses `//`, `.` and a trailing `/`, where Azur Lane's rejects them (a manifest path `assets/x/./a.png` got a URL and could be available; now its own normaliser, the ported root-safety test checks Python's unsafe paths plus those segments, through a manifest too). Nits fixed: the manifest batch cursor and model join used the stripped `character_key` against the raw column (now the stored key); the ship-detail fingerprint and class reads were materialised (now streamed); `archived_models` keeps one parsed entry per model because pinning needs the set (documented) | The crawl golden runs Python's `AzurLane.update()` over the test fake made PostgreSQL-like (cooldowns, COALESCE, `completed_at` kept on upsert); Python's own fake never reports an asset cooldown, so tests that lapse a model wait lapse the asset wait too. BD2's blob and retry helpers are `pub(crate)` and shared |
| WP13 | #145 | 2 pushes (1 CI, 1 after review) | 0 blockers, 2 should-fix, 3 nits: no test covered a transport failure, so a broken video download could leave its `.part` file and every "did not answer"/"unreachable" message could change unnoticed (now FakeWeb hang-ups and cut-short bodies for the WeChat page, the TikHub lookup and key check, Zhihu's lens fallback, NGA's session check and `fetch_video`, the download one checked by mutation); a digit string past 128 bits in an article share's `<piccount>` or a Zhihu playlist size raised `ValueError` without an entry (now in D129). Nits fixed: write errors in `fetch_video` no longer name the file (as Python's), D130 says which do; `_SESSION_CHECK_URL` and `_REQUEST_TIMEOUT_SECONDS` are in the constants golden (the lookup, lens and session-check timeouts are literals in Python's calls and checked by inspection); D131 covers `fetch_nga_post` with a non-ASCII cookie | CI caught the page smoke still listing the TikHub test route as pending; it now expects the key-less database's `422 tikhub_not_configured`. Ran in parallel with WP11 |
| WP14 | #146 | 1 push after review | 0 blockers, 2 should-fix, 4 nits: a TikHub key that is not ASCII is `UnicodeEncodeError` in httpx (not an `HTTPError`, so the run stops uncounted) but was a counted `LocalProtocolError` try here (now raised as Python's, same text, with a test); D139 claimed the "already stored" lookups refuse a bool or a 64-bit-overflowing id as the `INSERT`s do, while PostgreSQL compares a `numeric` false (Python then writes an orphan image before the `INSERT` fails) and refuses a bool as `UndefinedFunction` (D139 rewritten). Nits fixed: a log line's wording, `ensure_unique_path`/`unused_directory`/`exists` now off the async threads (`spawn_blocking`, as `to_thread`), a wrong D-number, and an unconfigured source no longer fails on a token it never uses (D140). The reviewer fuzzed `pybase64` against CPython 3.12's `binascii` (300k inputs, no mismatch) and checked five mutations of `update()` | `FakeWeb` gained CONNECT + TLS from a test CA (`ClientOptions::roots`), so the tests use the real `https://` addresses; `HttpClient`'s `Debug` no longer prints header values. Ran alone (WP15 not started) |
| WP15 | #149 | 2 pushes (1 after review, 1 after merging WP16/WP17) | 0 blockers, 2 should-fix, 4 nits: a document whose file reference expired mid-download failed and started over from zero on every retry, where Telethon fetches the message again and resumes (now ported, `_DirectDownloadIter._request`); the update pump raced `next_raw` against the state-save timer, and grammers' `next_raw` is not cancel-safe, so a batch of new messages could be lost (now the state is saved after an update once a minute, D154). Nits fixed: D152 misstated Telethon's partial files (it writes ` (1)` beside them), `MessageIdsEmptyError` from `channels.getMessages` is `None` for every id as in `_IDsIter`, D149 now records the `takeout_id` bytes, and two 5 s test deadlines were raised to 30 s for loaded runners. One nit (the `get_entity` `KeyError` text) was checked and already matches Telethon's bare id. The reviewer ran two mutations, both caught. After merging WP16/WP17, a full run exposed a panic: when the queue worker ended first (as on a stop), the account runtime awaited its finished `JoinHandle` again (now taken out of an `Option`, with a test); the persistent-client test no longer races the worker | Telethon's whole RPC error table and its flood-wait memory are copied (golden `telegram/rpc_errors.json`). Telegram is verified under mocks and goldens only: no real account, no Bot API |
| WP16 | #148 | 1 push after review | 0 blockers, 2 should-fix, 7 nits ([review](https://github.com/cyrahs/fav/pull/148#issuecomment-6769523425)): Chromium got 30 s to start and about 5-10 s to exit where Playwright 1.61 allows 180 s (launch timeout) and 30 s (`closeOrKill`), so a slow headed start on Xvnc failed the login and a slow exit could lose a RedNote session that lives only in the profile (now 180 s and 30 s); `API-COVERAGE.md` claimed the driver tests cover everything the deleted CDP probe did, but the network listeners and `Network.getResponseBody` are not in `fav-browser` yet (now said, left for WP17). Nits fixed: the VNC path check and the verify-modal branch had no test that a mutation could fail (both added, mutations now caught); `LoginAttempt`'s `Debug` printed the VNC ticket (redacted, with a test); a relative profile path is resolved as Playwright's `_user_data_dir` does; a non-ASCII ticket is D165; a wrong comment about chromiumoxide's handler ending; this status line. The reviewer checked the argv against Playwright 1.61's `coreBundle.js` and ran four mutations | Ran beside WP15. Chromium spawned by fav with Playwright's exact argv (golden captured from Python's Playwright through a recorder executable); the CDP probe crate module is gone |
| WP17 | #150 | 1 push after review | 0 blockers, 3 should-fix, 5 nits ([review](https://github.com/cyrahs/fav/pull/150#issuecomment-6772378996)): a SOCKS proxy is an `ImportError` (no `socksio`) in Python, a 500 from the proxy test and a failed start, but was dialled here with no divergence entry (now D181, as D67/D113); D175 said Python decodes a like page with the response's charset, while Playwright's `json()` is strict UTF-8 and refuses a BOM, which the Rust stripped (now refused as Python does, D175 rewritten); the out-of-order `loadingFinished`/`responseReceived` pairing D172 claims had no test (pulled out as `LikePageMatcher`, with a test). Nits fixed: `format_published_at` answered every out-of-range time with one `OverflowError` where `fromtimestamp` raises four different errors by size (all reproduced, CPython's `int` wrap of the year included, golden cases added); a profile lock that cannot be removed is now Python's `PermissionError`/`IsADirectoryError` text with the path (login window too); three mutations the ported tests missed (`proxy_media` choosing the CDN proxy and the start's `ValueError`, 410 as a stale status, the refresh leaving an unchanged URL marked stale) now fail tests; two stale comments. The reviewer ran twelve mutations and compared both Python modules line by line | Ran alone. RedNote and the Nikke runtime capture run on `fav_browser::driver` against loopback pages only; the CDP network events (`Page::network_events`, `response_body`) are in `fav-browser` now |
| WP18 | #151 | 1 push after review | 0 blockers, 5 should-fix, 9 nits: `fav-net` no longer built on its own (reqwest's `stream` feature came only from `fav-sources`; now its own); folding `kit` made an I/O error without an errno (what `tempfile` returns) `OSError` where kit and Python said `PermissionError` (now classed by kind, with a test); the route completeness test compared only the hand-built OpenAPI document, not the router's dispatch table (now both, checked by mutation); D50 was stale for Pixiv's proxy rule and its SOCKS case (rewritten); the image's Chromium check took any revision while `fav` launches 1228 (now that path, with a test tying `uv.lock`'s Playwright, `fav_browser::chromium` and the Dockerfile together). Nits fixed: `tokio_sleeper` panicked on an absurd delay, REPORT's behaviour sentence, route count and code-size rule, a stale `hyper` dependency, API-COVERAGE's probe sentence, the Dockerfile cache comment, this row. Left: the commit trailer names the model as every package's did (the session's required attribution line) | Level 2 replay, level 3 smoke, memory benchmark and image size in docs/REPORT.md; level 2 export deleted after the comparison |
| Fix D97/D15/D53 | #154 | 1 push after review | 0 blockers, 2 should-fix, 8 nits: removing the error path also made a digit string past 128 bits no number where the old Rust raised `ValueError` (now stated in D97); D97's decision column did not say that the viewer and Azur Lane follow only from the shared helpers (now says so, and Max is told). Nits fixed: this row's placeholders, "(decided)" in REPORT's choices, a golden assert message, a stray line break. Left: no test for the unreadable-settings fallback of D15, no Azur Lane route test (same helper), the `unnecessary_wraps` allow on Azur Lane's `list_characters`, and REPORT's table still lacks D183 (from #153, out of scope) | Max's 「先修三个照抄的bug」 (2026-11-10): the three Python bugs Rust had copied are now fixed (DIVERGENCES.md, now visible divergences). D97 reaches the BD2-L2D-Viewer and Azur Lane's API reader too, which share the helpers. Parity unchanged (1065 ported, 9 waived, 0 pending); the one golden case that encodes D97 is kept and stated in the test |
| Image workflow | (this PR) | - | - | Max's direct deployment (2026-11-10 23:34Z): `.github/workflows/rust-image.yml` publishes `rust-<short sha>` only; `check_tree.sh` exempts it as it does `rust.yml` |
| RedNote goto fix | (this PR) | - | - | First production run (2026-11-11): RedNote failed with chromiumoxide's "Request timed out." because chromiumoxide answers its own `Page.navigate` only after `load` in every frame (and queues the next navigation behind it), where Playwright's `goto` waits for `DOMContentLoaded`. `Page.goto` now sends `Page.navigate` on the page target's own DevTools socket and waits for the `DOMContentLoaded` lifecycle event of the new document's loader (same-document: commit), so a document still loading when `goto` starts cannot end the wait with its own (review finding); pages whose proxy needs credentials keep chromiumoxide's path, because Chromium cancels the auth challenge of a navigation another session started. Test `goto_waits_for_dom_content_loaded_not_load` fails before the fix |
| RedNote CORS preflight | (this PR) | - | - | Second production run (2026-11-11, rust-b69491e): RedNote failed with "RedNote returned a body that is not an object". The likes list comes from another origin with a signature header, so Chromium sends a CORS preflight, which the Network domain reports as a request of its own (initiator `preflight`, type `Other`, empty body) and Playwright does not report at all; the capture took the preflight for the page. `NetworkEvent::Request` now names such a request `preflight`; the RedNote capture and the Nikke request recorder skip it. A likes-list body that cannot be read is logged with its reason (never its contents), and a lone surrogate escape becomes U+FFFD instead of losing the page. `walks_a_local_likes_list_like_the_site` now fetches cross-origin with a signature header and fails without the fix |

# Agent Guide (fav)

A Rust service that crawls several content sources on a schedule, downloads their media, and tracks
what it already has in self-hosted PostgreSQL. A React front end configures it. It was ported from
Python in November 2026; the last Python commit is tag `python-final` (see `docs/rewrite/`).

The authoritative list of sources is `JOB_SPECS` in `crates/fav-runtime/src/jobs.rs` — read it rather
than any list in a document, including this one.

## Repo Rules (Must Follow)

- English only: write code, comments, and documentation in English.
- Read `docs/CONVENTIONS.md` before changing Rust code, and add an entry to `docs/DIVERGENCES.md` when
  you change behaviour that users, stored data or other services can see.
- Never point a test or a dev build at the production database, collection volume or a real account.

## Quick Start

### Prereqs

- Rust: `rustup` installs the toolchain pinned in `rust-toolchain.toml` on first use.
- PostgreSQL for the database tests (`FAV_TEST_DATABASE_URL`, see below).
- To run sources locally: `ffmpeg`, `yt-dlp` and `gallery-dl` on `PATH` (the image installs the two
  downloaders at the versions `docker/downloaders/uv.lock` pins), and a Chromium for the browser-driven
  sources (the image carries Playwright's build under `PLAYWRIGHT_BROWSERS_PATH`). Jobs declare what they
  shell out to in `JobSpec::required_commands`.
- Node 22 for the front end in `web/`.

### Configure

There is **no config file**. Only bootstrap values come from the environment (`fav_core::env`:
`POSTGRES_DSN`, `API_TOKEN`, `API_BIND`, `API_PORT`, `LOG_LEVEL`, read from the process environment or
`.env`); everything else lives in the `app_settings` table and is edited from the web UI at
`http://<host>:<API_PORT>/`. Copy `.env.example` to `.env`. `API_TOKEN` is required — it protects the
settings API, so an empty one would leave a fresh instance world-writable.

The settings models are in `crates/fav-core/src/settings.rs` (`SECTIONS`, `sensitive_fields`), the
`app_settings` store in `crates/fav-db/src/settings_store.rs`. Settings are re-read per use, so UI edits
apply without a restart. `HTTP_PROXY` / `HTTPS_PROXY` apply to every client and to the downloaders; sources
that route only their own origin have a per-source proxy field (`web.azurlane.origin_proxy`,
`web.twitter.proxy`, `web.rednote.proxy` — the last one is required, because RedNote answers datacenter
addresses with HTTP 461).

### Build, test, run

```bash
cargo build -p fav
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
FAV_TEST_DATABASE_URL=postgresql://user@127.0.0.1:5432/postgres cargo test --workspace --locked

cargo run -p fav                    # `fav serve`: API, web UI and worker in one process
cargo run -p fav -- api             # API and web UI only
cargo run -p fav -- worker          # scheduler, manual triggers, notification delivery
cargo run -p fav -- trigger <job_key|all>   # run once and exit
```

Database tests create and drop their own temporary databases through `FAV_TEST_DATABASE_URL`; they
fail rather than skip without it. Chromium tests read `FAV_TEST_CHROMIUM`. CI (`.github/workflows/ci.yml`)
also runs the page smoke test, `uv run tools/smoke/web_smoke.py` (see its docstring).

Front end: `cd web && npm ci && npm run lint && npm run build`. `fav` serves `web/dist` when it exists.

## Architecture

One binary, `fav` (`crates/fav`). `fav serve` (the image's `CMD`, under `tini` as PID 1, which reaps
orphaned browser processes) runs the API and the worker in one process; they coordinate through
PostgreSQL rather than through memory:

- **Scheduler** (`fav-runtime/src/scheduler.rs`, `worker.rs`): cron jobs built from the settings with
  `build_jobs()`, re-read so `enabled`/`cron` edits apply in place without a restart.
- **Manual triggers**: the UI posts to `/api/v2/job-requests`, which inserts a row into
  `control_requests`; the worker claims it (`FOR UPDATE SKIP LOCKED`) within about a second. On startup,
  before it schedules or claims anything, the worker marks every row still `running` as `interrupted`:
  the deployment runs one worker (`Recreate`), so those rows belong to a worker a deploy or crash stopped
  mid-run. A run cancelled during a graceful shutdown is recorded the same way.
- **Notifications** (`fav-runtime/src/notifications.rs`, `telegram_bot.rs`): sources enqueue into a
  durable outbox table; the worker delivers to Telegram. Job failures are enqueued under one dedupe key
  per job (`job_failed:<job>:run`), so a job that keeps failing bumps one row instead of adding a message
  per run; the next clean run resolves the job's rows. A row remembers the Telegram message it pinned and
  the next delivery on that row moves the pin, so a failure holds at most one pin at a time. Only the
  first occurrence rings; repeats are silent. A cancelled run is logged but not reported. Every message
  is `FAV · <source>` (the `JOB_SPECS` name), then the title with its link, then the body. Enqueuing is
  dropped when the source's `web.<source>.notify` toggle is off.

The API (`crates/fav-api`, axum) also serves an MCP endpoint at `/mcp` (`fav-api/src/mcp/`), registered
in the cluster's LiteLLM MCP gateway as `fav`. It is how a developer session reads runtime state (deployed
commit, runs, logs, WeChat state) and performs repairs without a shell in the pod; see the README's "MCP
endpoint". When a source grows state worth inspecting or resetting, add a fixed-query tool there rather
than a raw SQL escape hatch.

`.mcp.json` points Claude Code at `https://llm.s117.me/mcp/fav` (the gateway's credentials come from the
environment, e.g. the cloud session's proxy), and `.claude/settings.json` enables it, so sessions in this
repo get the tools as `mcp__fav__*`. Where they are not loaded, `script/fav_mcp.sh <tool> '<json>'` calls
the same server with curl.

## Project Layout

- `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml`: the workspace
- `crates/fav`: the binary (`serve`, `api`, `worker`, `trigger`, `nikke-layers`)
- `crates/fav-core`: env, settings models, logging, filenames, Python-compatible JSON/text helpers
- `crates/fav-db`: pool, the consolidated schema (`schema.sql`), advisory locks, `app_settings` store
- `crates/fav-net`: HTTP client factory, streamed downloads, retries, subprocess runner
- `crates/fav-runtime`: `JOB_SPECS`, scheduler, worker, control queue, notifications, site sessions
- `crates/fav-sources`: one module per source, plus the post-link parsers WeChat and Telegram share
- `crates/fav-browser`: Chromium over CDP, the 登录态 login window (Xvnc + headed Chromium + noVNC relay),
  the RedNote profile
- `crates/fav-api`: axum routes, archive browsing, settings masking, MCP
- `goldens/`: frozen expected outputs of the Python app that the tests compare against
- `docker/downloaders/`: `pyproject.toml` + `uv.lock` pinning yt-dlp, gallery-dl and Playwright for the image
- `tools/smoke/`: page smoke test of the front end against the API; `tools/codegen/`: HTML entity table
- `web/`: React + Vite front end, built to `web/dist` and served by the API
- `script/`: standalone helpers, not part of the binary — FavInbox (WeChat, runs on the oracle host),
  redroid web, Telegram login and channel listing (`uv run` with Telethon), `fav_mcp.sh`, a Hanime1
  userscript
- `docs/`: `CONVENTIONS.md`, `DIVERGENCES.md`, `azurlane-api.md`; `docs/rewrite/` is the history of the port

## Conventions (Important for Agents)

See `docs/CONVENTIONS.md` for code and test rules. The ones that bite most often:

### Database

`Db::ensure_schema` at startup is the only migration there is: `crates/fav-db/src/schema.sql`, additive
changes only. Keep it in three blocks: tables, then every `ALTER TABLE ADD COLUMN`, then every
`CREATE INDEX`. On an existing database the `CREATE TABLE` is a no-op, so a column added later arrives
through its `ALTER` alone, and an index naming that column ahead of the `ALTER` fails. Update
`crates/fav-db/src/schema/snapshot.txt` in the same change; the schema test compares the two.

### Logging

`fav_core::logging::get("name")`, then `log.info(...)` and so on. Never log secrets.

### Filenames and paths

Use `fav_core::filename` for anything written to disk. Outputs are rooted at a path from the settings
table, usually `./collection/...`.

### Network + external services

Nearly every module talks to a real external service. Everything external is injected (HTTP client,
database, clock, browser, Telegram client, subprocess runner); tests use local fake servers and never
touch the internet.

## Adding a Source

`fav-sources/src/jandan.rs` and `twitter.rs` are the closest references. Registration is spread across
several registries, and missing one fails in a way that is not obvious:

1. `crates/fav-core/src/settings.rs`: the section's model with a `path` and `cron` default and its
   runnable check; register it in `SECTIONS`, and its secrets in `sensitive_fields`.
2. `crates/fav-sources/src/<source>.rs`: the crawler implementing `fav_runtime::source::Source`;
   register it in `fav_sources::registry()`.
3. `crates/fav-runtime/src/jobs.rs`: a `JobSpec`, including `required_commands` for any external binary.
4. `crates/fav-api/src/handlers.rs`: add the key to `JOB_REQUEST_TARGETS`, or manual triggers 422.
5. `crates/fav-api/src/archive.rs`: an `ARCHIVE_SOURCES` entry with its external URL, so rows show up
   on the records page with a working link back to the origin.
6. `crates/fav-api/src/masking.rs`: masking for each secret the section holds.
7. `crates/fav-api/src/site_logins.rs`: for a source that needs a signed-in session, a `LoginSite`.
8. `crates/fav-db/src/schema.sql` (and `schema/snapshot.txt`): its tables.
9. `web/src/labels.ts`: the display name. Then a form in `web/src/components/sectionForms.tsx` or an
   entry in `JOBS_PAGE_ONLY_SECTIONS` if cron and enabled are all it has.
10. Tests next to the module, and a mention in `README.md`.

Store dedupe state in PostgreSQL keyed on whatever the origin considers an item's identity, and let an
enabled-but-unconfigured source stay parked rather than crashing the worker.

## Docker

`Dockerfile` builds the front end in a Node stage and the binary in a Rust stage, then a runtime image
carrying `ffmpeg`, yt-dlp and gallery-dl (from `docker/downloaders/uv.lock`), a Playwright-managed
Chromium (used by the NIKKE runtime capture, the RedNote profile, and headed on TigerVNC's `Xvnc` for
登录态 logins) and `tini`. Configuration comes from environment variables.

```bash
docker run --rm \
  --env-file .env \
  -v "$PWD/collection:/app/collection" \
  -v "$PWD/log:/app/log" \
  -v "$PWD/data:/app/data" \
  ghcr.io/cyrahs/fav:latest
```

`data/` carries the state that has to outlive the container: the Telegram session and the browser
profiles. Without it, Telegram re-authorises and RedNote has to be signed in again on the 登录态 page
after every restart. It wants real block storage — a Chromium profile is LevelDB and SQLite, neither of
which survives NFS locking.

CI builds the image on every pull request and publishes `latest` from `main`; Flux deploys it
(charys117/nas-gitops `apps/fav`). Verify a change with the cargo commands above rather than a local
image build.

## Making Changes Safely

- Avoid logging secrets (site-session cookies, PostgreSQL credentials, Telegram API hash and bot
  token, proxy URLs with embedded credentials).
- Keep `.env` out of git history (it is gitignored; do not override that).
- Secrets in `app_settings` are masked on read and restored on write. Preserve that round trip when
  touching the settings API — a masked or omitted value means "keep what is stored", never "clear it".
- `PUT /api/v2/settings/{section}` replaces a whole section, so a partial payload resets omitted
  fields to their defaults.

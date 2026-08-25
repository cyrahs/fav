# Agent Guide (fav)

Python 3.12+ automation that crawls several content sources on a schedule, downloads their media, and
tracks what it already has in self-hosted PostgreSQL. A React front end configures it.

The authoritative list of sources is `JOB_SPECS` in `src/service/jobs.py` — read it rather than any
list in a document, including this one.

## Repo Rules (Must Follow)

- English only: write code, docstrings, comments, and documentation in English.
- Use `.venv` (Python): use the project's virtual environment, not the system Python.
- Use `uv` for packages: `uv sync`, `uv run ...` — never call `pip` directly.

## Quick Start

### Prereqs

- Python `>=3.12` (see `pyproject.toml`)
- `uv` (required; the repo has a `uv.lock`)
- `ffmpeg` on `PATH`, for the separate audio and video streams Bilibili serves. It is the only
  external binary the app needs: both downloaders — `yt-dlp` (Bilibili, Hanime1) and `gallery-dl`
  (X) — are uv dependencies whose console scripts land in `.venv/bin` from `uv sync`, so they are
  version-locked and upgraded through `uv.lock` rather than installed separately.
- Jobs declare what they shell out to in `required_commands`, resolved with `shutil.which`. `--trigger`
  checks it up front via `_validate_commands`; the scheduler path reports it per job as
  `missing_commands` instead of refusing to start.

```bash
uv sync
```

### Configure

There is **no `config.toml`**. Only bootstrap values come from the environment; everything else lives
in the `app_settings` table and is edited from the web UI at `http://<host>:<API_PORT>/`.

Create a `.env` (gitignored; copy `.env.example`) with at least `POSTGRES_DSN` and `API_TOKEN`.
`API_TOKEN` is required — it protects the settings API, so an empty one would leave a fresh instance
world-writable.

- `src/core/env.py` holds the bootstrap model; it is import-time and immutable.
- `src/core/settings.py` holds the full model tree plus the `app_settings` read/write helpers.
- `settings.load()` returns a snapshot with a 2s TTL cache. Resolve it per use — never snapshot a
  section at module import, or UI edits will not apply until a restart.
- `settings.use(snapshot)` pins a snapshot and bypasses the database; `tests/conftest.py` uses it in
  an autouse fixture so the suite never needs PostgreSQL.
- `credentials.cookiecloud` is a deployment-wide CookieCloud credential. A source (or bilibili
  account) whose own `cookiecloud` block is not fully filled in gets it substituted when the
  `Settings` snapshot is built (`Settings.apply_shared_cookiecloud`); the stored sections keep only
  what the operator typed. The API mirrors the fallback when computing `missing_fields`
  (`FavApiService._missing_fields_after_fallback`).
- `httpx`, `yt-dlp` and `gallery-dl` all read `HTTP_PROXY` / `HTTPS_PROXY`, so there is no global
  proxy setting. Sources that need to route only their own origin have a per-source proxy field
  (`web.azurlane.origin_proxy`, `web.twitter.proxy`, `web.rednote.proxy` -- the last one is
  required rather than optional, because RedNote answers datacenter addresses with HTTP 461).

### Run

```bash
uv run python run.py            # worker + scheduler, long-running
uv run python -m src.api        # API + web UI, long-running
uv run python run.py --trigger <job_key|all>   # run once and exit
```

`python -m src.service` (the Docker `CMD`) supervises both processes; `src/service/launcher.py`
spawns them and forwards shutdown signals.

## Architecture

Three moving parts, all coordinating through PostgreSQL rather than through memory:

- **Scheduler** (`run.py`): builds APScheduler cron jobs from `build_jobs()`, and polls
  `app_settings` every 15s so `enabled`/`cron` edits apply in place without a restart.
- **Manual triggers**: the UI posts to `/api/v2/job-requests`, which inserts a row into
  `control_requests`; the worker claims it (`FOR UPDATE SKIP LOCKED`) within ~1s. Job state is
  re-read per request, so "configure, then press Run" works without waiting for the settings poll.
- **Notifications**: sources call `enqueue_notification(...)` into a durable outbox table; the worker
  delivers it via `src/tool/telegram_bot.py`. Job failures are enqueued by `run.py` itself, and an
  exception carrying a `notification_dedupe_key` attribute is deduplicated instead of spamming.
  Every message follows one template: a `FAV · <source>` line from `header`, then `title` carrying
  the `link_url` hyperlink, then `body`. Pass `header` from every call site — the source's `JOB_SPECS`
  name, so one source is never labelled two ways — and keep that name out of `title`. The immediate
  sends in `telegram_bot.py` (`send_text_now`, `send_photo_now`) take the same `header`. Rendering
  runs off the stored columns, and the worker re-renders at claim time, so anything the template
  needs has to be a column rather than a call-time-only argument. `enqueue_notification` drops the
  message and returns `None` when the source's `web.<source>.notify` toggle is off — `source` is the
  job key, so pass it rather than a prettier label, or the toggle will not find its section. Call
  sites that pass a `source` which is not a job key (`run.py` passes `worker`) gate themselves.

A source is a duck type, not a base class. `JobSpec.factory()` must return an object with an
`async update()`; an optional `async aclose()` is called afterwards if present.

## Project Layout

- `run.py`: worker — scheduler, control-request consumer, notification delivery, settings watcher
- `src/service/`: `launcher.py` (process supervisor), `jobs.py` (`JOB_SPECS`, `build_jobs`)
- `src/core/env.py`: bootstrap env model (`POSTGRES_DSN`, `API_TOKEN`, bind/port)
- `src/core/settings.py`: typed settings models + `app_settings` read/write
- `src/core/logger.py`: tqdm-friendly logging (avoid `print()`)
- `src/web/*.py`: per-source crawlers; each exposes an `update()` coroutine
- `src/api/`: FastAPI app — `routes.py`, `service.py` (logic), `schemas.py`, `archive.py`
  (read-only archive browsing over whitelisted tables/columns), `settings_masking.py`
- `src/tool/database.py`: async pooled PostgreSQL helpers
- `src/tool/cookiecloud.py`: fetch + decrypt CookieCloud cookies (Bilibili and X sessions)
- `src/web/rednote_browser.py`: the signed-in Chromium profile that source reads through
- `src/tool/notifications.py`, `control_queue.py`, `telegram_bot.py`: the outbox and trigger queue
- `src/tool/filename.py`: sanitization and `[uploader]title [id].ext` formatting helpers
- `web/`: React + Vite front end, built to `web/dist` and served by the API
- `script/`: standalone helpers (BD2 viewer comparison, Nikke layer metadata, Telegram login shell
  scripts, a Hanime1 userscript) — not imported by the app
- `tests/`: one file per source plus API, settings, database, and notification coverage

## Conventions (Important for Agents)

### Formatting / lint

Ruff, line length `140`, single quotes, `select = ALL` minus an ignore list (see `pyproject.toml`).
CI runs all three of these, so run them before proposing a change:

```bash
uv run ruff format .
uv run ruff check .
uv run pytest -q
```

Front end: `cd web && npm run lint` (which is `tsc --noEmit`) and `npm run build`.

### Tests

The suite never needs PostgreSQL — `tests/conftest.py` pins a defaults-only settings snapshot and
sets fake bootstrap env vars before `src.*` is imported. Tests mutate that yielded snapshot to
configure a source. Fake the network by injecting `httpx.MockTransport` or a stub client; fake the
database by monkeypatching the module's `database` attribute.

`tests/test_cookiecloud.py::test_save_to_netscape_format_live_bilibili` is a live test, skipped
unless a real deployment is reachable. To skip it explicitly:

```bash
uv run pytest -k 'not test_save_to_netscape_format_live_bilibili'
```

### Database

`src/tool/database.py` accepts **SQLite-flavoured SQL and translates it**, which is easy to trip
over: `?` placeholders become `%s`, `INSERT OR IGNORE` becomes `ON CONFLICT DO NOTHING`, and
`PRAGMA table_info(t)` becomes an `information_schema` query. Write new queries in that dialect to
match the existing sources.

There is no migration framework. Each source owns its schema in an `_ensure_table()` coroutine:
`CREATE TABLE IF NOT EXISTS`, then `PRAGMA table_info` plus `ALTER TABLE ADD COLUMN` for anything
added later. Additive changes only — preserve backward compatibility.

Statement order is the only migration there is, so keep the script in three blocks: tables, then
every `ALTER TABLE ADD COLUMN`, then every `CREATE INDEX`. On an existing database the `CREATE TABLE`
is a no-op, so a column added later arrives through its `ALTER` alone; an index naming that column
ahead of the `ALTER` fails with `UndefinedColumn` and, because the connection is autocommit and
`_ensure_table()` runs first, takes down the rest of the script — including the `ALTER` that would
have repaired the table — on every run. `tests/test_database.py` enforces the ordering.

### Logging

Use `from src.core import logger` and `logger.get('name')`. The handler is designed to coexist with
`tqdm` progress bars. Never log secrets.

### Filenames and paths

Use `src/tool/filename.py` (`sanitize`, `format_video_filename`, `ensure_unique_path`) for anything
written to disk. Outputs are rooted at a path from the settings table, usually `./collection/...`.

### Network + external services

Nearly every module talks to a real external service (PostgreSQL, CookieCloud, Bilibili, Telegram,
Kemono, l2d.su, X). Prefer dependency injection and fakes in tests. If an integration test is truly
necessary, make it skip cleanly when its config or secrets are absent.

## Adding a Source

`src/web/twitter.py` and `src/web/jandan.py` are the closest references. Registration is spread
across several hand-maintained registries, and missing one fails in a way that is not obvious:

1. `src/core/settings.py`: a `ScheduleJob` subclass with a `path` and `cron` default, plus
   `validate_runnable()` returning the field names that must be filled in first. Register it in the
   `Web` model, in `SECTION_MODELS`, and — if it has secrets — in `SENSITIVE_FIELDS`.
2. `src/web/<source>.py`: the crawler, with `async update()` and an optional `async aclose()`.
   Export it from `src/web/__init__.py`. Any `enqueue_notification` it makes passes
   `header='<JobSpec name>'`.
3. `src/service/jobs.py`: a `JobSpec`, including `required_commands` for any external binary.
4. `src/api/schemas.py`: add the key to `JobRequestTarget`, or manual triggers 422.
5. `src/api/archive.py`: an `ARCHIVE_SOURCES` entry and an `_EXTERNAL_URL_BUILDERS` entry, so rows
   show up on the records page with a working link back to the origin.
6. `src/api/settings_masking.py`: a branch per section that holds a secret.
7. `src/tool/cookiecloud.py`: for a cookie-based source, a `CookieProfile` in `PROFILES`. That dict
   is what backs the settings page's 测试连接 button; without an entry the endpoint rejects the source.
8. `web/src/labels.ts`: the display name — this is the single place a source is named for the UI.
   Then either a form in `web/src/components/sectionForms.tsx` (registered in `SECTION_FORMS`, with
   any client-side rules in `validateSection`) or an entry in `JOBS_PAGE_ONLY_SECTIONS` if cron and
   enabled are all it has.
9. `tests/test_<source>.py`, and a mention in `README.md`.

Store dedupe state in PostgreSQL keyed on whatever the origin considers an item's identity, and let
an enabled-but-unconfigured source stay parked via `validate_runnable()` rather than crashing the
worker.

## Docker

`Dockerfile` builds the front end in a Node stage, then a runtime image carrying `ffmpeg` and a
Playwright-managed Chromium (used by `src/web/nikke_runtime.py` and by the signed-in profile in
`src/web/rednote_browser.py`). The downloaders arrive with the virtualenv, whose `bin` is on
`PATH`. Configuration comes from environment variables.

```bash
docker run --rm \
  --env-file .env \
  -v "$PWD/collection:/app/collection" \
  -v "$PWD/log:/app/log" \
  -v "$PWD/data:/app/data" \
  fav
```

`data/` carries the state that has to outlive the container: the Telethon session and the
RedNote browser profile. Without it, Telegram re-authorises and RedNote asks for a QR scan
after every restart. It wants real block storage — a Chromium profile is LevelDB and SQLite, neither
of which survives NFS locking.

Do not build or run the image locally to verify a change — CI builds it, and the test job gates the
build. Verify with `uv run pytest` and the commands above instead.

## Making Changes Safely

- Avoid logging secrets (CookieCloud passwords, PostgreSQL credentials, Telegram API hash and bot
  token, proxy URLs with embedded credentials).
- Keep `.env` out of git history (it is gitignored; do not override that).
- Secrets in `app_settings` are masked on read and restored on write. Preserve that round trip when
  touching the settings API — a masked or omitted value means "keep what is stored", never "clear it".
- `PUT /api/v2/settings/{section}` replaces a whole section, so a partial payload resets omitted
  fields to their defaults.

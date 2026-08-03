# Agent Guide (fav)

This repository is a small Python 3.12+ automation tool that downloads and archives content from several sources (Bilibili favorites + "watch later", Telegram channels, and Kemono). It tracks what has been downloaded using self-hosted PostgreSQL.

The main entrypoint is `run.py`.

## Repo Rules (Must Follow)

- English only: Write code, docstrings, comments, and documentation in English.
- Use `.venv` (Python): For local development and tooling, use the project's `.venv` virtual environment (do not rely on the system Python).
- Use `uv` for packages: Manage dependencies and run tooling via `uv` (e.g. `uv sync`, `uv run ...`) rather than calling `pip` directly.

## Quick Start

### Prereqs

- Python `>=3.12` (see `pyproject.toml`)
- `uv` (required; repo contains `uv.lock`)
- `yt-dlp` on `PATH` (required for Bilibili downloads)

### Install deps

```bash
# Creates/uses .venv by default.
uv sync
```

### Configure

There is **no `config.toml`**. Only bootstrap values come from the environment; everything else lives
in the `app_settings` table and is edited from the web UI at `http://<host>:<API_PORT>/`.

Create a `.env` (gitignored; see `.env.example`):

```bash
POSTGRES_DSN=postgresql://user:password@127.0.0.1:5432/fav
API_TOKEN=replace-with-a-strong-random-token
API_BIND=127.0.0.1
API_PORT=8091
```

`API_TOKEN` is required — it protects the settings API, so an empty one would leave a fresh instance
world-writable.

Notes:
- `src/core/env.py` holds the bootstrap model; it is import-time and immutable.
- `src/core/settings.py` holds the full model tree plus the `app_settings` read/write helpers.
- `settings.load()` returns a snapshot with a 2s TTL cache. Resolve it per use — never snapshot a
  section at module import, or UI edits will not apply until a restart.
- `settings.use(snapshot)` pins a snapshot and bypasses the database; `tests/conftest.py` uses it in
  an autouse fixture so the suite never needs PostgreSQL.
- There is no proxy setting: `httpx` and `yt-dlp` both read `HTTP_PROXY` / `HTTPS_PROXY`.

### Run

```bash
uv run python run.py
```

`run.py` is a long-running scheduler process intended for Deployment.
It schedules jobs using cron expressions from the `app_settings` table, and reschedules them in place within ~15s of a change:
- `web.bilibili.cron` -> `src/web/bilibili.py` (`Bilibili().update()`)
- `web.telegram.cron` -> `src/web/telegram.py` (`Telegram().update()`)
- `web.stellasora.cron` -> `src/web/stellasora.py` (`StellaSora().update()`)
- `web.azurlane.cron` -> `src/web/azurlane.py` (`AzurLane().update()`)

Kemono is implemented in `src/web/kemono.py` but is not called from `run.py` in the current version.

## Project Layout

- `run.py`: long-running scheduler for update jobs
- `src/core/env.py`: bootstrap env model (`POSTGRES_DSN`, `API_TOKEN`, bind/port)
- `src/core/settings.py`: typed settings models + `app_settings` table read/write
- `src/tool/telegram_bot.py`: direct Telegram Bot API notification delivery
- `src/api/archive.py`: read-only archive browsing (whitelisted tables/columns)
- `web/`: React + Vite front end, built to `web/dist` and served by the API
- `src/core/logger.py`: tqdm-friendly logging (avoid `print()`)
- `src/tool/cookiecloud.py`: fetch + decrypt CookieCloud cookies (used for Bilibili)
- `src/tool/database.py`: PostgreSQL database helpers
- `src/tool/filename.py`: filename sanitization and `[uploader]title [id].ext` formatting helpers
- `src/web/*.py`: per-source downloaders; each exposes an `update()` coroutine
- `script/bilibili.py`: manual CLI to download a single BV with `yt-dlp` using CookieCloud cookies
- `tests/`: CookieCloud decryption/formatting, Bilibili update behavior, and StellaSora parsing/download helpers

## Conventions (Important for Agents)

### Formatting / lint

This repo uses Ruff with:
- line length `140`
- single quotes (`ruff format`)
- `select = ALL` with an ignore list (see `pyproject.toml`)

Run locally:

```bash
uv run ruff format .
uv run ruff check .
```

### Tests

Run:

```bash
uv run pytest
```

`tests/test_cookiecloud.py` includes a "live" test that reads CookieCloud settings from the database and is skipped when they are unset or the database is unreachable. If you want to skip it explicitly:

```bash
uv run pytest -k 'not test_save_to_netscape_format_live_bilibili'
```

### Logging

Use `from src.core import logger` and `logger.get('name')`. The logging handler is designed to work with `tqdm` progress bars.

### Filenames and paths

- Use `src/tool/filename.py` helpers (`sanitize`, `format_video_filename`, `ensure_unique_path`) for anything written to disk.
- Outputs are typically rooted under paths from the settings table (often `./collection/...`).

### Network + external services

Most modules talk to real external services:
- PostgreSQL database
- CookieCloud server
- Bilibili API
- Telegram API (Telethon)
- Kemono API

When adding tests, prefer dependency injection / fakes to avoid live calls. If you must add an integration test, make it skippable when secrets/config are not present.

## Docker

`Dockerfile` builds the front end in a Node stage, then a runtime image with a `yt-dlp` binary. Configuration is supplied through environment variables.

Example:

```bash
docker build -t fav .
docker run --rm \
  --env-file .env \
  -v "$PWD/collection:/app/collection" \
  -v "$PWD/log:/app/log" \
  fav
```

## Making Changes Safely

- Avoid logging secrets (CookieCloud password, PostgreSQL credentials, Telegram API hash, Telegram Bot token).
- Keep `.env` out of git history (it is gitignored; do not override that).
- Secrets in `app_settings` are masked on read; preserve that when touching the settings API.
- If you change database schemas (`CREATE TABLE ...` blocks), preserve backward compatibility or provide a migration strategy.

Any new source should:
- Have a dedicated `src/web/<source>.py` with an `update()` coroutine.
- Store dedupe state in PostgreSQL (like existing sources do).
- Write files using the filename utilities and config-defined paths.

# Agent Guide (fav)

This repository is a small Python 3.12+ automation tool that downloads and archives content from several sources (Bilibili favorites + "watch later", a "Tangxin" site using m3u8 parts, Telegram channels, and Kemono). It tracks what has been downloaded using Cloudflare D1 (SQLite) and stores some raw artifacts (for Tangxin) in Cloudflare KV.

The main entrypoint is `run.py`.

## Repo Rules (Must Follow)

- English only: Write code, docstrings, comments, and documentation in English.
- Use `.venv` (Python): For local development and tooling, use the project's `.venv` virtual environment (do not rely on the system Python).
- Use `uv` for packages: Manage dependencies and run tooling via `uv` (e.g. `uv sync`, `uv run ...`) rather than calling `pip` directly.

## Quick Start

### Prereqs

- Python `>=3.12` (see `pyproject.toml`)
- `uv` (required; repo contains `uv.lock`)
- `ffmpeg` on `PATH` (required for Tangxin merge)
- `yt-dlp` on `PATH` (required for Bilibili downloads)

### Install deps

```bash
# Creates/uses .venv by default.
uv sync
```

### Configure

This project **requires** a local `config.toml` in the repo root. It is intentionally gitignored (`.gitignore`).

Do not commit secrets. Use placeholders when sharing examples or logs.

Minimal template (fill in your own values):

```toml
proxy = ""

[cookiecloud]
server_url = "https://your-cookiecloud.example"
uuid = "..."
password = "..."

[web.bilibili]
id = 123
fav_id = 456
path = "./collection/bilibili"
enabled = true
cron = "*/30 * * * *"

[web.tangxin]
host = "https://example.txh*.com"
path = "./collection/tangxin"
enabled = true
cron = "*/30 * * * *"

[cloudflare]
api_key = "..."
account_id = "..."
d1_id = "..."

# Namespaces used by the code
[cloudflare.kv_id]
tangxin = "..."
cookie = "..."

[web.telegram]
channels = [1234567890]
api_id = 123
api_hash = "..."
path = "./collection/telegram"
session_path = "./data/telethon-session"
enabled = true
cron = "*/30 * * * *"

[web.stellasora]
path = "./collection/stellasora"
enabled = true
cron = "0 */6 * * *"

[web.kemono]
enabled = false
cron = "0 */6 * * *"
path = "./collection/kemono"

[[web.kemono.creators]]
service = "fanbox"
id = "..."
name = "..."
```

Notes:
- Config is loaded in `src/core/config.py` via `pydantic-settings` from `./config.toml` only.
- Cloudflare D1 is used to dedupe downloads and store metadata.
- Tangxin expects the m3u8 text to be stored in Cloudflare KV under the configured namespace.

### Run

```bash
uv run python run.py
```

`run.py` is a long-running scheduler process intended for Deployment.
It schedules jobs using cron expressions from `config.toml`:
- `web.tangxin.cron` -> `src/web/tangxin.py` (`Tangxin().update()`)
- `web.bilibili.cron` -> `src/web/bilibili.py` (`Bilibili().update()`)
- `web.telegram.cron` -> `src/web/telegram.py` (`Telegram().update()`)
- `web.stellasora.cron` -> `src/web/stellasora.py` (`StellaSora().update()`)

Kemono is implemented in `src/web/kemono.py` but is not called from `run.py` in the current version.

## Project Layout

- `run.py`: long-running scheduler for update jobs
- `src/core/config.py`: typed config models; loads `config.toml` at import time
- `src/core/logger.py`: tqdm-friendly logging (avoid `print()`)
- `src/tool/cookiecloud.py`: fetch + decrypt CookieCloud cookies (used for Bilibili)
- `src/tool/cloudflare.py`: Cloudflare D1/KV client helpers
- `src/tool/filename.py`: filename sanitization and `[uploader]title [id].ext` formatting helpers
- `src/web/*.py`: per-source downloaders; each exposes an `update()` coroutine
- `script/bilibili.py`: manual CLI to download a single BV with `yt-dlp` using CookieCloud cookies
- `script/tx.js`: Tampermonkey userscript to help export Tangxin favorites and capture/store m3u8 data in Cloudflare
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

`tests/test_cookiecloud.py` includes a "live" test that uses your `config.toml` CookieCloud settings and may be skipped if CookieCloud is unreachable. If you want to skip it explicitly:

```bash
uv run pytest -k 'not test_save_to_netscape_format_live_bilibili'
```

### Logging

Use `from src.core import logger` and `logger.get('name')`. The logging handler is designed to work with `tqdm` progress bars.

### Filenames and paths

- Use `src/tool/filename.py` helpers (`sanitize`, `format_video_filename`, `ensure_unique_path`) for anything written to disk.
- Outputs are typically rooted under paths from `config.toml` (often `./collection/...`).

### Network + external services

Most modules talk to real external services:
- Cloudflare API (D1 + KV)
- CookieCloud server
- Bilibili API
- Telegram API (Telethon)
- Kemono API

When adding tests, prefer dependency injection / fakes to avoid live calls. If you must add an integration test, make it skippable when secrets/config are not present.

## Docker

`Dockerfile` builds a runtime image that includes `ffmpeg` and downloads a `yt-dlp` binary at build time. It does **not** bake in `config.toml`; you should mount it.

Example:

```bash
docker build -t fav .
docker run --rm \
  -v "$PWD/config.toml:/app/config.toml:ro" \
  -v "$PWD/collection:/app/collection" \
  -v "$PWD/log:/app/log" \
  fav
```

## Making Changes Safely

- Avoid logging secrets (CookieCloud password, Cloudflare API tokens, Telegram API hash).
- Keep `config.toml` out of git history (it is gitignored; do not override that).
- If you change database schemas (D1 `CREATE TABLE ...` blocks), preserve backward compatibility or provide a migration strategy.

Any new source should:
- Have a dedicated `src/web/<source>.py` with an `update()` coroutine.
- Store dedupe state in D1 (like existing sources do).
- Write files using the filename utilities and config-defined paths.

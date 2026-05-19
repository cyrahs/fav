# fav

Automation toolkit for collecting content from multiple sources and deduplicating with PostgreSQL.

## API Backend

This repository exposes an internal FastAPI backend for control, archived catalog reads, and Hanime1 scan-target creation.

- `run.py` remains the worker and scheduler process.
- `python -m src.api` starts the FastAPI server with OpenAPI support.
- The default container entrypoint still starts both worker and API through `python -m src.service`.
- The API is intended for internal K8s access and is not designed as a public internet-facing surface.

### V2 endpoints

Public endpoints:

- `GET /healthz`
- `GET /docs`
- `GET /openapi.json`

Protected endpoints:

- `GET /api/v2/hanime1/videos`
- `GET /api/v2/jobs`
- `POST /api/v2/job-requests`
- `GET /api/v2/job-requests/{id}`
- `POST /api/v2/hanime1/seeds`
- `GET /api/v2/nikke/characters`
- `GET /api/v2/nikke/sidebar/characters`
- `GET /api/v2/nikke/characters/{content_id}`
- `GET /api/v2/bd2/characters`
- `GET /api/v2/bd2/sidebar/characters`
- `GET /api/v2/bd2/characters/{content_id}`
- `GET /api/v2/azurlane/characters`
- `GET /api/v2/azurlane/sidebar/characters`
- `GET /api/v2/azurlane/characters/{character_key}`

All `/api/v2/*` endpoints require:

- `Authorization: Bearer <api.token>`

The API contract is OpenAPI-first. Use `/openapi.json` or `/docs` for the exact request and response schema.

NIKKE, BD2, and Azur Lane asset URLs in API responses point to `/static/{source}/{directory_name}/assets/...` and are intended to be served by the deployment's static file service, not by FastAPI.
For Azur Lane specifically, map `/static/azurlane/` to the configured `web.azurlane.path` directory so returned URLs resolve to archived files under each character directory.

Azur Lane support is backend-only in this repository. The crawler archives Live2D and Spine resources, writes deterministic manifests, and the API returns metadata plus static asset URLs. Browser rendering, model display, and any Azur Lane client UI belong in a separate application that consumes the API.

## BD2 L2D Viewer comparison

BD2 is collected from GameKee. To compare the archived GameKee Live2D resource stems with the resources listed by
[BD2-L2D-Viewer](https://jelosus2.github.io/BD2-L2D-Viewer/), run:

```bash
uv run python script/bd2_compare_l2d_viewer.py
```

To compare against the current GameKee API without downloading assets or touching PostgreSQL:

```bash
uv run python script/bd2_compare_l2d_viewer.py --source gamekee-live
```

Use `--format json` for machine-readable output.

During BD2 crawls, GameKee remains the primary source. BD2-L2D-Viewer is used only as a supplemental Live2D source when a
viewer resource can be anchored to an existing GameKee model for the same character/costume entry. Censored viewer resources are
stored as model variants on the anchored costume instead of separate characters.

### Config source

The API reads runtime settings from `config.toml`:

- `database.postgres_dsn` (required)
- `api.token` (required)
- `api.bind` (optional, default `127.0.0.1`)
- `api.port` (optional, default `8091`)
- `api.cors_origins` (optional, default `[]`)
- `api.cors_allow_credentials` (optional, default `false`)
- `notifications.webhook_base_url` (required for worker delivery)
- `notifications.webhook_token` (required for worker delivery)

Example:

```toml
[database]
postgres_dsn = "postgresql://user:password@127.0.0.1:5432/fav"

[api]
token = "replace-with-strong-random-token"
bind = "127.0.0.1"
port = 8091
cors_origins = ["https://game-view.s117.me"]
cors_allow_credentials = false

[notifications]
webhook_base_url = "https://internal.example.com"
webhook_token = "replace-with-webhook-bearer-token"
```

Use `cors_allow_credentials = true` only if the browser needs to send cookies or other credentialed requests to the API origin.

Azur Lane crawler/archive settings:

```toml
[web.azurlane]
enabled = false
path = "./collection/azurlane"
cron = "0 */6 * * *"
```

Set `enabled = true` to schedule Azur Lane archive updates. The API reads Azur Lane manifests from `web.azurlane.path` even when the scheduled job is disabled.

### Run

```bash
uv run python -m src.api
```

## Telegram Media Types

Telegram channels download videos by default. Add `media_types = ["video", "image"]` to a channel to also archive images.
`image` includes regular Telegram photos and image documents such as original PNG/JPEG files, while stickers are skipped.

```toml
[[web.telegram.accounts.channels]]
id = 1234567890
path = "./collection/telegram/channel-name"
media_types = ["video", "image"]
```

## Telegram Downloader Safety

Telegram channel downloads are incremental and paced by default.
The first run without an existing scan cursor only considers the latest `scan_limit` messages.
Later runs continue from the saved per-channel message cursor.
When upgrading from older versions with existing Telegram download rows, the initial cursor starts after the latest saved message.

```toml
[web.telegram]
scan_limit = 50
download_limit_per_channel = 2
download_delay_seconds = 60
channel_cooldown_seconds = 1800
history_wait_seconds = 1
flood_sleep_threshold_seconds = 300
```

## Hanime1 Ranking Discovery

Hanime1 can auto-add series targets from the configured weekly and monthly adult anime ranking pages before each downloader run.
This is disabled by default and starts with the first ranking page.

```toml
[web.hanime1.ranking]
enabled = true
periods = ["weekly", "monthly"]
pages = 1
```

### Container startup

Docker image default command starts both:

- scheduler worker: `run.py`
- API server: `python -m src.api`

Deploying the image without overriding the command will start both services.

## Hanime1 userscript

`script/hanime1_downloaded_marker.user.js` consumes the Hanime1 downloaded-list endpoint.

- Endpoint: `GET /api/v2/hanime1/videos`
- Auth: `Authorization: Bearer <api.token>`
- Response shape: `{"items":[{"video_id":"1001","title":"...","downloaded":true,"uploader":"...","release_date":"2024-01-01","plot":"...","watch_url":"https://hanime1.me/watch?v=1001"}],"total":1}`

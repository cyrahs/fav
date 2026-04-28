# fav

Automation toolkit for collecting content from multiple sources and deduplicating with PostgreSQL.

## API Backend

This repository exposes an internal FastAPI backend for control and Hanime1 scan-target creation.

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

All `/api/v2/*` endpoints require:

- `Authorization: Bearer <api.token>`

The API contract is OpenAPI-first. Use `/openapi.json` or `/docs` for the exact request and response schema.

### Config source

The API reads runtime settings from `config.toml`:

- `database.postgres_dsn` (required)
- `api.token` (required)
- `api.bind` (optional, default `127.0.0.1`)
- `api.port` (optional, default `8091`)
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

[notifications]
webhook_base_url = "https://internal.example.com"
webhook_token = "replace-with-webhook-bearer-token"
```

### Run

```bash
uv run python -m src.api
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

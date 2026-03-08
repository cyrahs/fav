# fav

Automation toolkit for collecting content from multiple sources and deduplicating with PostgreSQL.

## API Backend

This repository now exposes a control/read API backend instead of running an in-process Telegram interactive bot.

- `run.py` remains the only worker/scheduler process that executes jobs.
- `src/api/server.py` exposes authenticated control and notification endpoints for external clients.
- Download/job notifications are stored in PostgreSQL and can be polled by a frontend.

### Control endpoints

- `GET /api/v1/control/jobs`
  - Returns visible job metadata: `key`, `name`, `enabled`, `run_on_start`
- `POST /api/v1/control/requests`
  - JSON body: `{"kind":"trigger_job","target":"bilibili|hanime1|jandan|telegram|stellasora|all"}`
  - Returns `202` with request state
- `GET /api/v1/control/requests/{id}`
  - Returns request status: `pending|running|succeeded|failed|rejected`
- `GET /api/v1/control/runtime/hanime1/seeds`
  - Returns normalized Hanime1 runtime seeds
- `POST /api/v1/control/runtime/hanime1/seeds`
  - JSON body: `{"seed":"12488"}` or `{"seed":"屈辱 {id-12488}"}`
- `DELETE /api/v1/control/runtime/hanime1/seeds/{video_id}`
  - Deletes a normalized seed by canonical video ID

All control endpoints require:

- `Authorization: Bearer <api.token>`

Worker-trigger requests are stored in PostgreSQL and consumed by the worker process, so manual triggers reuse the same runner and locking path as scheduled jobs.

### Notification endpoints

- `GET /api/v1/notifications`
  - Query params:
    - `status=unread|all` (default `unread`)
    - `limit` (default `50`, max `200`)
    - `after_id` (optional, returns `id > after_id`)
  - Returns notifications ordered by `id ASC`
- `POST /api/v1/notifications/ack`
  - JSON body: `{"ids":[1,2,3]}`
  - Marks unread notifications as read

Notification polling also uses:

- `Authorization: Bearer <api.token>`

## Hanime1 Marker (Tampermonkey + Remote Read-Only API)

This repository includes:

1. `src/api/server.py`: read-only API backend for fav, including Hanime1 downloaded-ID endpoint.
2. `script/hanime1_downloaded_marker.user.js`: Tampermonkey userscript that marks Hanime1 cards as downloaded.

### API endpoint

- Method: `GET`
- Path: `/api/v1/runtime/hanime1/downloaded-ids`
- Auth: `Authorization: Bearer <token>`
- Optional request header: `If-None-Match: "<etag>"`
- Response:
  - `200`: JSON payload with `ids`, `count`, `generated_at`
  - `304`: no body when ETag is unchanged
  - `401`: missing/invalid auth scheme
  - `403`: invalid token
  - `500`: internal server error without DB details

Additional endpoint:

- Method: `GET`
- Path: `/api/v1/health`
- Response: `200` with `{"status":"ok","generated_at":"..."}` for health checks

### API config source

The API reads all runtime settings from `config.toml`:

- `database.postgres_dsn` (required): PostgreSQL DSN
- `api.token` (required): bearer token used by Tampermonkey
- `api.bind` (optional, default `127.0.0.1`)
- `api.port` (optional, default `8091`)

### Run the API

```bash
uv run python -m src.api
```

`config.toml` example:

```toml
[database]
postgres_dsn = "postgresql://user:password@127.0.0.1:5432/fav"

[api]
token = "replace-with-strong-random-token"
bind = "127.0.0.1"
port = 8091
```

### Container startup (k3s)

Docker image default command now starts both:

- scheduler worker (`run.py`)
- API server (`python -m src.api`)

So in k3s, deploying the image without overriding command will auto-start API.

No mode switching is supported. The container always starts both services.

### Reverse proxy examples

Nginx:

```nginx
server {
    listen 443 ssl http2;
    server_name marker.example.com;

    location /api/v1/runtime/hanime1/downloaded-ids {
        limit_req zone=api burst=20 nodelay;
        proxy_pass http://127.0.0.1:8091;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

Caddy:

```caddy
marker.example.com {
    reverse_proxy 127.0.0.1:8091
}
```

### Tampermonkey setup

1. Install Tampermonkey in your browser.
2. Create a new userscript and paste `script/hanime1_downloaded_marker.user.js`.
3. Open any `https://hanime1.me/*` page.
4. On first run, provide:
   - API base URL (for example `https://marker.example.com`)
   - API token (same value as `api.token` in `config.toml`)
5. The script will:
   - Scan `a[href*="/watch?v="]` anchors containing `img`
   - Add a top-right `已下载` badge for IDs found in the API
   - Rescan on DOM updates (infinite scroll)
   - Sync every 120 seconds

### Cache and fallback behavior

- The userscript stores:
  - `hanime1_marker_api_base_url`
  - `hanime1_marker_api_token`
  - `hanime1_marker_ids_cache`
  - `hanime1_marker_etag`
  - `hanime1_marker_last_success_at`
- If API sync fails, it keeps using local cached IDs and shows a stale-data banner.

### Token reset

Use Tampermonkey menu command:

- `Hanime1 Marker: Reset API Config`

Then reload the page to configure a new API base URL/token.

### Troubleshooting

1. No badges appear:
   - Verify the script is enabled and `@match` is `https://hanime1.me/*`.
   - Check browser console for `[Hanime1 Marker]` errors.
2. API returns 401/403:
   - Confirm `Authorization: Bearer <token>` matches `api.token` in `config.toml`.
3. API returns 500:
   - Validate DSN and database connectivity.
   - Ensure table `hanime1` exists and contains `id` values.

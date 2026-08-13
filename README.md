# fav

Automation toolkit for collecting content from multiple sources and deduplicating with PostgreSQL.

## Configuration

There is no `config.toml`. Bootstrap values come from the environment, and everything else lives in
the `app_settings` table and is edited from the web UI.

### Environment (`.env`)

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `POSTGRES_DSN` | yes | — | Database connection string |
| `API_TOKEN` | yes | — | Bearer token for every `/api/v2/*` endpoint and for the web UI login |
| `API_BIND` | no | `0.0.0.0` | API listen address |
| `API_PORT` | no | `8091` | API listen port |
| `API_CORS_ORIGINS` | no | empty | Comma-separated origins, for separate front ends such as the Live2D viewer |
| `API_CORS_ALLOW_CREDENTIALS` | no | `false` | Only if a browser must send credentialed requests |
| `TZ` | no | `UTC` | Scheduler timezone |

`API_TOKEN` is mandatory: an empty token would leave the settings API of a freshly provisioned
instance world-writable. See `.env.example`.

There is no global proxy setting. `httpx`, `yt-dlp` and `gallery-dl` all honour `HTTP_PROXY` /
`HTTPS_PROXY`, so set those in the environment if you need one. Azur Lane and X (Twitter) each have
their own per-source proxy field for the cases where only that one origin needs routing.

### Database-backed settings

Everything else — per-source `enabled`, `cron`, paths, Bilibili and Telegram accounts, Kemono
creators, Hanime1 ranking, and notification delivery — is stored per section in `app_settings`:

```sql
CREATE TABLE app_settings (section TEXT PRIMARY KEY, value JSONB NOT NULL, updated_at TIMESTAMPTZ NOT NULL);
```

Sections: `web.bilibili`, `web.telegram`, `web.stellasora`, `web.nikke`, `web.bd2`, `web.azurlane`,
`web.hanime1`, `web.jandan`, `web.kemono`, `web.twitter`, `notifications.telegram`.

#### Bilibili accounts

`web.bilibili` holds a list of accounts, each with its own favourite lists and watch-later setting:

```jsonc
{
  "cron": "*/30 * * * *",
  "enabled": true,
  "accounts": [
    {
      "name": "main",                                  // ASCII slug; names the cookie file
      "favorites": [
        { "fav_id": 123456789, "path": "collection/bilibili/main/fav" },
        { "fav_id": 987654321, "path": "collection/bilibili/main/music" }
      ],
      "toview_enabled": true,                          // watch-later is opt-in per account
      "toview_path": "collection/bilibili/main/later",  // its own directory, not a subdir of the favourites
      "cookiecloud": { "server_url": "...", "uuid": "...", "password": "..." }  // this account's login
    }
  ]
}
```

Each favourite list names its own directory, and watch-later has a separate one, so the two no longer
share a parent. An account needs complete CookieCloud credentials plus either a favourite list or
`toview_enabled` before it is runnable; the credentials *are* the account identity, since the
watch-later list is always the logged-in user's. There is no shared `cookiecloud` section — Bilibili
was its only consumer. Watch-later is still cleared after a successful pass, so only enable it for
accounts whose list you want consumed.

Deduplication is by `bvid` across the whole `bilibili` table, so a video favourited by two accounts
downloads once, into whichever directory is reached first.

`POST /api/v2/cookiecloud/test` checks one account's credentials without saving them, and the settings
page exposes it as a 测试连接 button inside each account. It fetches and
decrypts the vault and reports which failure it hit — server unreachable, decrypt failed (wrong UUID
or password), no `bilibili.com` cookies, or a vault missing some of `sessdata` / `bili_jct` /
`buvid3` / `dedeuserid`. A masked password in the request resolves to the one stored under that
account name, so what is checked is what the crawler would actually use.

#### X (Twitter) liked tweets

`web.twitter` archives the images and videos from your own liked tweets. X sells no affordable read
access to a likes timeline, so the crawl shells out to [gallery-dl](https://github.com/mikf/gallery-dl)
(a Python dependency, on `PATH` inside the image) with the browser session the account is signed in
with. gallery-dl owns the parts that break when X changes — GraphQL endpoints, the transaction-id
header, 429 back-off — so a break is usually fixed by a newer gallery-dl rather than by editing this
repository.

That upgrade is unattended: `.github/dependabot.yml` watches gallery-dl daily, and
`.github/workflows/dependabot-auto-merge.yml` enables auto-merge on the pull request so it lands once
the `test` check that main's branch protection requires has passed, then dispatches an image rebuild.
Nothing bypasses a check — a failing `test` just leaves the pull request open. To upgrade by hand:

```bash
uv lock --upgrade-package gallery-dl
```

```jsonc
{
  "cron": "0 */6 * * *",
  "enabled": true,
  "username": "yourhandle",                 // your own screen name, without the @
  "path": "collection/twitter",
  "video_path": null,                       // null keeps videos with the images
  "cookiecloud": { "server_url": "...", "uuid": "...", "password": "..." },
  "sleep_request_seconds": 2.0,             // spacing between X requests
  "abort_after": 20,                        // consecutive known files that end an incremental run
  "include_retweets": true,                 // liked retweets, filed under the original author
  "include_videos": true,                   // photos only when false
  "proxy": ""
}
```

The session comes from the same CookieCloud vault Bilibili uses, read from `x.com` (or `twitter.com`,
whichever the browser extension synced), and needs `auth_token` and `ct0`. It is re-fetched at the
start of every run, so signing in again in the browser is all it takes to recover from an expired
session. `POST /api/v2/cookiecloud/test` takes a `source` field (`bilibili` or `twitter`) and the
settings page exposes it as the same 测试连接 button.

Photos, videos and GIFs are all collected — X stores a GIF as a short video, so `include_videos`
covers both. Setting `video_path` keeps videos and GIFs on a different disk from the images; leave it
null and everything shares `path`. The split happens after the download rather than in gallery-dl's
config, because gallery-dl picks a directory once per tweet, before it knows whether the files in
that tweet are photos or videos. Moving them afterwards is safe: its download archive is keyed on the
tweet, not the path, so a relocated file is never fetched again.

Files land in `<root>/<author>/[<author>] <date> [<tweet_id>_<num>].<ext>`, where `<root>` is `path`
or `video_path` depending on the media type, and each one is recorded in the `twitter` table keyed by
`(tweet_id, num)`. `local_path` is relative to whichever of the two roots the file lives in, which the
`media_type` column identifies. Two pieces of state make runs incremental:
gallery-dl's own download archive at `path/.gallery-dl-archive.db`, and a `twitter_state` row marking
whether the first full walk of the timeline ever finished. Until it has, every run walks to the end of
the timeline so the history fills in; afterwards runs stop once `abort_after` already-archived files
come up in a row. A run that exits non-zero does not set that mark, so a failed backfill is retried
rather than assumed complete.

Caveats worth knowing: this is an unofficial path, so keep `sleep_request_seconds` conservative; X
truncates a deep likes timeline server-side, so the backfill reaches only as far back as X is willing
to serve; and unliking a tweet does not delete what was already downloaded.

Every section is constructible from defaults, so an empty database boots with all sources disabled.
Required fields are enforced by `validate_runnable()` only when a source is enabled: the API reports
them as `missing_fields`, and the scheduler keeps an enabled-but-incomplete source parked rather than
crashing.

Secrets (`web.bilibili.accounts[].cookiecloud.password`, `web.twitter.cookiecloud.password`,
`web.telegram.accounts[].api_hash`, `notifications.telegram.bot_token`) are stored in
plain text but are masked on read (`aa78••••`). Sending a masked value back — or omitting the field —
keeps the stored secret. Telegram secrets are matched by account name, so reordering accounts in the
UI cannot shuffle credentials between them; the same holds for Bilibili's per-account CookieCloud
password.

The worker polls `app_settings` every 15 seconds and reschedules APScheduler jobs in place, so
`enabled` and `cron` changes apply without a restart.

**Known limitation:** the Telegram realtime listener is created at process start. Toggling
`web.telegram.enabled` at runtime only affects its cron reconciliation job; the listener still needs
a worker restart.

## Web UI

`web/` is a React + TypeScript + Vite front end served by FastAPI from `web/dist` when that directory
exists (API-only otherwise). Pages: overview, archive records, jobs, settings. Cron fields show a
live natural-language description of the expression.

The jobs page owns `cron` and `enabled` for every source, plus manual 立即运行. Sources with an
incomplete configuration cannot be enabled: the toggle is disabled and `PUT /api/v2/settings/{section}`
answers `422 incomplete_settings` with the missing field names.

The settings page owns everything else and renders a typed form per section — checkboxes for toggles
and media-type routing, repeatable rows for Bilibili accounts/favourites, Telegram accounts/channels
and Kemono creators, credential blocks with a live connection test for Bilibili and X — validated
locally before submitting. Sources whose only settings are cron/enabled (StellaSora, BD2, Azur Lane)
do not appear there at all; their `path` keeps its default and can be changed through the API if a
deployment needs to. Every listed section also has a `JSON 编辑` toggle for raw editing, which is what
a section gets if this build predates it. Secret fields (`api_hash`, CookieCloud password, Telegram
bot token) read back masked; leaving the mask in place keeps the stored value. Note that `PUT` replaces a
whole section, so a partial payload resets omitted fields to their defaults — the UI always sends the
complete object.

```bash
cd web
npm install
npm run dev     # http://localhost:5173, proxies /api to http://127.0.0.1:8091
npm run build   # emits web/dist, which the API then serves at /
```

Log in with `API_TOKEN`; it is kept in `localStorage` for 30 days, renewed on use, and dropped on logout
or on the first 401/403 from the API.

## API Backend

This repository exposes an internal FastAPI backend for control, archived catalog reads, and Hanime1 scan-target creation.

- `run.py` remains the worker and scheduler process.
- `python -m src.api` starts the FastAPI server with OpenAPI support.
- The default container entrypoint still starts both worker and API through `python -m src.service`.
- The API is intended for internal K8s access and is not designed as a public internet-facing surface.

### V2 endpoints

Public endpoints:

- `GET /healthz`
- `GET /readyz`
- `GET /docs`
- `GET /openapi.json`

`/healthz` is a lightweight process liveness endpoint. `/readyz` performs a cached deep check of Hanime1 playlist parsing against up
to three configured scan targets and returns `503` with `status=degraded` when the parser or upstream watch pages are unavailable.

Protected endpoints:

- `GET /api/v2/hanime1/videos`
- `GET /api/v2/jobs`
- `POST /api/v2/job-requests`
- `GET /api/v2/job-requests/{id}`
- `POST /api/v2/hanime1/seeds`
- `GET /api/v2/hanime1/seeds`
- `DELETE /api/v2/hanime1/seeds/{canonical_video_id}`
- `GET /api/v2/job-requests`
- `GET /api/v2/settings`
- `GET /api/v2/settings/{section}`
- `PUT /api/v2/settings/{section}`
- `POST /api/v2/cookiecloud/test`
- `POST /api/v2/notifications/telegram/test`
- `GET /api/v2/archive/sources`
- `GET /api/v2/archive/items`
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

- `Authorization: Bearer <API_TOKEN>`

The API contract is OpenAPI-first. Use `/openapi.json` or `/docs` for the exact request and response schema.

Nikke, BD2, and Azur Lane asset URLs in API responses point to `/static/{source}/{directory_name}/assets/...` and are intended to be served by the deployment's static file service, not by FastAPI.
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

Bootstrap comes from the environment (see [Configuration](#configuration)); everything else is read
from the `app_settings` table.

### Telegram Bot notifications

The worker delivers queued notifications directly through Telegram's Bot API. It prefers a local
image, then a remote image URL, and finally text. Telegram rate-limit responses feed the PostgreSQL
outbox retry schedule. Pinning is best-effort so a missing pin permission does not resend an already
delivered message. The API origin is fixed to `https://api.telegram.org` so a retained masked token
cannot be redirected to another host.

Configure the `notifications.telegram` section in the web UI, save it, then use 发送测试消息 before
switching `enabled` on:

| Field | Purpose |
| --- | --- |
| `enabled` | Route outbox deliveries to Telegram |
| `bot_token` | BotFather token (masked after save) |
| `chat_id` | Destination chat, group, or channel ID; stored as a string |
| `message_thread_id` | Optional forum topic ID |

The test button sends through the **saved** credentials, not the form draft, so save before testing.
If delivery is disabled or incomplete, notifications stay queued and an unconfigured deployment
still boots.


### Azur Lane

Azur Lane crawler settings live in the `web.azurlane` section (`enabled`, `path`, `cron`). Set
`enabled` to true to schedule archive updates. The API reads Azur Lane manifests from the configured
path even when the scheduled job is disabled.

### Run

```bash
uv run python -m src.api
```

## Telegram Media Types

Telegram channels download videos by default. Add `media_types = ["video", "image"]` to a channel to archive both types in one directory,
or repeat the channel ID with disjoint media types to route each type to a separate directory. `image` includes regular Telegram photos and
image documents such as original PNG/JPEG files, while stickers are skipped.

```toml
[[web.telegram.accounts.channels]]
id = 3942401424
path = "./collection/image"
media_types = ["image"]

[[web.telegram.accounts.channels]]
id = 3942401424
path = "./collection/video"
media_types = ["video"]
```

Each `(channel ID, media type)` route must be unique within an account. Declaring the same media type more than once for a channel is a
configuration error, including conflicts between equivalent positive Telethon IDs and `-100...` Bot API IDs. Destination directories are
created lazily when their first media file is downloaded.

## Telegram Downloader Safety

Telegram accounts keep a long-running Telethon connection. New single-media messages and albums are persisted to
PostgreSQL immediately, then processed by one serial download worker per account. Different accounts run in parallel.
Completed queue rows remain in PostgreSQL for durable deduplication, and interrupted jobs are recovered when the
account session owner restarts.

The configured cron is a reconciliation scan for messages missed during disconnects; it only adds durable queue jobs.
The first reconciliation without an existing cursor considers the latest `scan_limit` messages. Later scans continue
from the saved per-channel cursor, which advances only after all selected media have been persisted. Existing Telegram
archive rows seed the initial cursor when upgrading. Use the jobs page's 立即运行 to force a reconciliation scan; event
listeners always start when Telegram is enabled.

`download_limit_per_channel` limits newly queued reconciliation jobs, not real-time events. Successful downloads are
paced by `download_delay_seconds`. `channel_cooldown_seconds` is reserved for FloodWait and error recovery. Image
notifications include the archived file as `image_path`; video notifications only include `saved_path`.

When a media type is first configured for a channel, Telegram scans the latest `scan_limit` messages for that type and
persists every missing result to the durable queue. This one-time backfill is not capped by `download_limit_per_channel`;
the serial worker and `download_delay_seconds` still pace the actual downloads. Completed backfills remain recorded
across path changes or temporary route removal.

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

[web.hanime1.ranking.deep_scan]
enabled = false
quota = 0.25
max_extra_pages = 5
```

### Deep scan (forced new-series quota)

When the ranking head is dominated by already-known series, the regular pages may stop producing new series for a long time.
Deep scan (disabled by default) guarantees a minimum intake rate of `quota` new series per run:

- `quota` accepts fractions and accumulates as debt across runs: `0.25` means one forced new series every 4 runs.
- New series found on the regular pages count against the quota first. Surplus does not carry over as credit (debt floors at 0).
- When accumulated debt reaches 1, extra ranking pages are scanned page-major (weekly page `pages+1`, monthly page `pages+1`, weekly page `pages+2`, ...) and scanning stops as soon as the debt is paid, so at most the required number of new series is added per run.
- Debt is capped at `max(1, ceil(quota))`, so after outages the scan never floods in a large backlog at once.
- `max_extra_pages` bounds how many pages beyond `pages` each period may be probed per run.

### Container startup

Docker image default command starts both:

- scheduler worker: `run.py`
- API server: `python -m src.api`

Deploying the image without overriding the command will start both services.

## Hanime1 userscript

`script/hanime1_downloaded_marker.user.js` consumes the Hanime1 downloaded-list endpoint.

- Endpoint: `GET /api/v2/hanime1/videos`
- Auth: `Authorization: Bearer <API_TOKEN>`
- Response shape: `{"items":[{"video_id":"1001","title":"...","downloaded":true,"uploader":"...","release_date":"2024-01-01","plot":"...","watch_url":"https://hanime1.me/watch?v=1001"}],"total":1}`

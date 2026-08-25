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
| `LOG_LEVEL` | no | `INFO` | `DEBUG` makes a run explain itself — every intercepted request, and the *shape* of the payloads a source parses (key names and types, never values) |
| `TZ` | no | `UTC` | Scheduler timezone |

`API_TOKEN` is mandatory: an empty token would leave the settings API of a freshly provisioned
instance world-writable. See `.env.example`.

There is no global proxy setting. `httpx`, `yt-dlp` and `gallery-dl` all honour `HTTP_PROXY` /
`HTTPS_PROXY`, so set those in the environment if you need one. Azur Lane, X and RedNote each
have their own per-source proxy field for the cases where only that one origin needs routing --
RedNote's is required rather than optional, see below.

### Database-backed settings

Everything else — per-source `enabled`, `notify`, `cron`, paths, Bilibili and Telegram accounts,
Kemono creators, Hanime1 ranking, and notification delivery — is stored per section in
`app_settings`:

```sql
CREATE TABLE app_settings (section TEXT PRIMARY KEY, value JSONB NOT NULL, updated_at TIMESTAMPTZ NOT NULL);
```

Sections: `web.bilibili`, `web.telegram`, `web.stellasora`, `web.nikke`, `web.bd2`, `web.azurlane`,
`web.hanime1`, `web.jandan`, `web.kemono`, `web.twitter`, `web.pixiv`, `web.rednote`,
`notifications.telegram`.

Every `web.*` section carries `cron`, `enabled` and `notify`, all three edited on the jobs page
rather than the settings page. `notify` defaults to true and covers everything that source sends to
Telegram — its per-item and per-run reports as well as its job-failure alerts — so a source that
runs often can be kept running quietly. It is per source, not per notification kind.

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
      "cookiecloud": "main"                            // name of an entry in the shared `cookiecloud` section
    }
  ]
}
```

Each favourite list names its own directory, and watch-later has a separate one, so the two no longer
share a parent. An account needs a reference to a complete CookieCloud config plus either a favourite
list or `toview_enabled` before it is runnable; the referenced vault *is* the account identity, since
the watch-later list is always the logged-in user's. Watch-later is still cleared after a successful
pass, so only enable it for accounts whose list you want consumed.

CookieCloud credentials themselves live in the shared `cookiecloud` section — a named list of
`{name, server_url, uuid, password}` entries edited at the bottom of the settings page — and every
consumer (each Bilibili account, X, pixiv) references one by name, so a vault used by several sources
is configured once. Legacy rows that still carry inline credentials are hoisted into the shared
section automatically on the first load after upgrading (identical credentials collapse into a single
entry).

Deduplication is by `bvid` across the whole `bilibili` table, so a video favourited by two accounts
downloads once, into whichever directory is reached first.

`POST /api/v2/cookiecloud/test` checks a config's credentials without saving them, and the settings
page exposes it as a 测试连接 button — inside the shared section (connectivity and decryption only)
and next to each reference (also checks the vault carries that source's cookies, e.g. `bilibili.com`'s
`sessdata` / `bili_jct` / `buvid3` / `dedeuserid`). A masked password in the request resolves to the
one stored under that config name, so what is checked is what the crawler would actually use.

#### X liked tweets

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
  "cookiecloud": "main",                    // name of an entry in the shared `cookiecloud` section
  "sleep_request_seconds": 2.0,             // spacing between X requests
  "abort_after": 20,                        // consecutive known files that end an incremental run
  "include_retweets": true,                 // liked retweets, filed under the original author
  "include_videos": true,                   // photos only when false
  "proxy": ""
}
```

The session comes from the referenced entry of the shared `cookiecloud` section (typically the same
vault Bilibili uses), read from `x.com` (or `twitter.com`, whichever the browser extension synced),
and needs `auth_token` and `ct0`. It is re-fetched at the start of every run, so signing in again in
the browser is all it takes to recover from an expired session. `POST /api/v2/cookiecloud/test`
takes a `source` field (`bilibili`, `twitter`, `pixiv`, or empty for connectivity only) and the
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

#### pixiv public bookmarks

`web.pixiv` archives your own public bookmarks (公開ブックマーク) through the `www.pixiv.net` ajax
API: illustrations and manga as their original files (one file per page), and ugoira synthesized from
their frame zip into a single animated webp with Pillow, honoring each frame's delay. Novels are not
collected, and R-18 bookmarks arrive with the session like any other.

```jsonc
{
  "cron": "0 */6 * * *",
  "enabled": true,
  "path": "collection/pixiv",
  "user_id": "",                            // empty: derived from the PHPSESSID cookie
  "cookiecloud": "main",                    // name of an entry in the shared `cookiecloud` section
  "sleep_request_seconds": 1.0,             // spacing between ajax requests; CDN downloads are not paced
  "proxy": ""                               // empty: direct (or the global HTTP_PROXY)
}
```

The bookmarks listing refuses anonymous requests, so the session is the `PHPSESSID` cookie read from
the referenced CookieCloud vault under `pixiv.net`, re-fetched at the start of every run. The logged-in user's
id is embedded in that cookie's value (`{user_id}_{hash}`), so `user_id` normally stays empty. Image
originals on `i.pximg.net` need only the `Referer` header, never the cookie.

Every output file is a row in the `pixiv` table keyed by `(illust_id, num)` with
`downloaded`/`unavailable` flags, and files land in `path/<author>/[<author>]<title> [<id>_p<num>].<ext>`
(ugoira: `[<id>_ugoira].webp`). A run walks the bookmarks newest-first and stops after two consecutive
pages whose every work is already settled, so the first run backfills everything and later runs stay
short. A bookmark whose work was deleted or made private shows up masked and is settled as
unavailable — an already-downloaded file is left alone, which is what the archive is for. Works a
previous run left pending are retried from the database at the end of every run.

#### RedNote liked notes

`web.rednote` archives the images and videos from your own liked notes on 小红书. A likes list is
only readable by the account that owns it, so this source drives your own account and account safety
is the constraint the design bends around.

An earlier revision replayed the browser session as signed HTTP from the cluster. RedNote answered
with HTTP 461 and invalidated the account's sessions everywhere, phone included. Its risk control
reads three things together -- address, device fingerprint, and request behaviour -- and a plain
HTTP client presents badly on all three. So the reading happens in a browser instead: a Chromium
profile that stays signed in on a volume, leaves through a residential proxy, and is scrolled so the
site issues its own requests. Nothing here computes a request signature, which also means there is
nothing to break when RedNote rotates one.

```jsonc
{
  "cron": "0 */6 * * *",
  "enabled": true,
  "path": "collection/rednote",
  "video_path": null,                       // null keeps videos with the images
  "profile_path": "./data/rednote-profile",
  "proxy": "http://user:pw@home.example:3128",
  "allow_direct_connection": false,         // true only on a residential host
  "proxy_media": false,                     // the CDN is unauthenticated; direct by default
  "user_id": "",                            // blank reads it off the signed-in page once
  "sleep_request_seconds": 3.0,
  "abort_after": 2,                         // consecutive archived pages that end a run
  "max_pages_per_run": 40,                  // memory valve for the first backfill
  "login_wait_seconds": 240,
  "login_prompt_cooldown_seconds": 3600,
  "headless": true
}
```

**The proxy is required** unless `allow_direct_connection` is set: most datacenter ranges answer 461,
and going out over one is what cost the account its sessions, so it is now a decision rather than a
default. Credentials have to be `http(s)` — Chromium cannot authenticate to a SOCKS proxy, and it
ignores credentials embedded in `--proxy-server`, so they are split out before launch. Only the
browser needs it: nothing else in this source ever contacts xiaohongshu.com.

**测试出口** beside the field checks the egress as typed, before saving and without the account: an
anonymous request to the site, reporting the exit address so a home line can be told from a
datacenter range, and calling out an HTTP 461 as the captcha wall rather than as a generic failure.
It tells a dead proxy apart from a proxy that works and a site that will not answer it, and refuses
an authenticated SOCKS proxy the same way the launcher will. It does not start Chromium, so it
proves the address rather than the browser — a signed-out profile or a wrong `user_id` still only
show up on a real run. With the field empty it probes the direct egress instead, which is how to
decide whether `allow_direct_connection` is defensible here.

**`profile_path` has to be on a volume.** It holds the cookies, localStorage, history and device
identity; on ephemeral storage every run starts from a QR scan. The documented `docker run` in
`AGENTS.md` mounts `data/`, and a Chromium profile wants real block storage — its LevelDB and SQLite
files do not survive NFS locking.

Signing in is interactive and happens **during** a run, and it takes **two scans**: the account QR,
and then an account-security QR that the site puts up on top of it — a separate modal, good for
about a minute, and expected rather than exceptional when signing in from a new device and address.
Both are read straight out of the page and sent to Telegram with captions that say which is which,
and the run waits up to `login_wait_seconds` for them. That send bypasses the notification outbox
deliberately — queued notifications are only flushed after a job returns, which for a run blocked on
a scan is long after the code has expired. So press 立即运行 with your phone to hand.

At most three codes are sent per stage, and `login_prompt_cooldown_seconds` stops a 04:00 cron from
sending one that will be dead before anyone sees it. The verification code is reminted on a timer
rather than when it is seen to expire: expiry leaves no mark in the page — same image bytes, no new
class, just a sentence in whatever language the profile runs in. There is no inbound Telegram path,
so a scan is detected by polling; and because the page does not update itself when a scan lands, the
wait reloads it rather than trusting what the page said when it was first loaded.

A run walks the likes list newest first, resolving each page's new notes into one row per file in the
`rednote` table — keyed `(note_id, media_index)`, written with `downloaded = 0` — and then closes
the browser and downloads every row still pending. The browser window is kept short on purpose: a
persistent Chromium context costs several hundred megabytes and the download phase can run for hours,
and being OOM-killed there would take the whole worker with it. Two pieces of state make runs
incremental: a note that already has rows costs one place in a list page instead of a page load of
its own, and a `rednote_state` row marks whether the first full walk ever finished. Until it has,
every run walks to the end of the list so the history fills in; afterwards runs stop once
`abort_after` pages that **added nothing** come up in a row. A walk that ended early — on the stop
rule, on `max_pages_per_run`, or because scrolling stopped producing — does not set that mark, so a
failed backfill is retried rather than assumed complete.

The stop rule counts what a page contributed rather than whether every note on it was already held,
because the list churns underneath it: notes get deleted, and get unliked while you are looking at
them. A deleted note never gains rows and so is never "already held" — under the older rule one of
those near the top of the list reset the counter on every run, and the early stop was never reached.
Those notes are tracked in `rednote_missing`, and retried until **three separate runs** have each
found them gone, after which they stop costing a page load. Only the site's own verdict counts, which
is its redirect to `/404`; a timeout, a navigation error or a note that simply carries no files is
this run's problem rather than the note's, and reading a note successfully clears whatever had been
counted against it.

Every file comes straight from the CDN, with the browser closed and no cookies attached — images,
the clips inside live photos, and whole videos alike. A video row points at `originVideoKey`, the
untranscoded upload: the signed-in page states the key outright and the CDN serves it unsigned, so
the original is reachable without presenting the account for it. Only if a note is rendered without
that key does the row fall back to the transcoded stream the web player is given, which on a
phone-shot note is a quarter of the pixels. Files land in
`<root>/<nickname>/[<nickname>]<date> [<note_id>_<n>].<ext>`, where `<root>` is `path`, or
`video_path` for mp4s when that is set. Downloads stream to a neighbouring `.part` file and are moved
into place at the end, because the next run skips whatever is already on disk. A CDN URL that stops
serving is recorded rather than judged:
the browser is closed by then, and a 404 says as much about a rotated URL as a deleted note, so the
next run looks at the note again and either refreshes the URL or retires the row.

Caveats worth knowing: automating a signed-in session is against RedNote's terms and the ban risk
is not zero — everything here lowers the probability, none of it eliminates it, and the account is
not replaceable. The QR is a live credential for the few minutes it lasts, so send notifications to a
private chat rather than a shared group. And unliking a note does not delete what was already
downloaded.

Every section is constructible from defaults, so an empty database boots with all sources disabled.
Required fields are enforced by `validate_runnable()` only when a source is enabled: the API reports
them as `missing_fields`, and the scheduler keeps an enabled-but-incomplete source parked rather than
crashing.

Secrets (`cookiecloud.configs[].password`, `web.telegram.accounts[].api_hash`,
`notifications.telegram.bot_token`) are stored in
plain text but are masked on read (`aa78••••`). Sending a masked value back — or omitting the field —
keeps the stored secret. Telegram secrets are matched by account name, so reordering accounts in the
UI cannot shuffle credentials between them; the same holds for the shared CookieCloud passwords,
matched by config name. (`web.rednote.proxy` and the other proxy URLs are deliberately shown in
plain text.)

The worker polls `app_settings` every 15 seconds and reschedules APScheduler jobs in place, so
`enabled` and `cron` changes apply without a restart. `notify` is read when a notification is
queued, so it applies to the next message either way.

**Known limitation:** the Telegram realtime listener is created at process start. Toggling
`web.telegram.enabled` at runtime only affects its cron reconciliation job; the listener still needs
a worker restart.

## Web UI

`web/` is a React + TypeScript + Vite front end served by FastAPI from `web/dist` when that directory
exists (API-only otherwise). Pages: overview, archive records, jobs, settings. Cron fields show a
live natural-language description of the expression.

The jobs page owns `cron`, `enabled` and `notify` for every source, plus manual 立即运行. Sources with an
incomplete configuration cannot be enabled: the toggle is disabled and `PUT /api/v2/settings/{section}`
answers `422 incomplete_settings` with the missing field names. 通知 has no such restriction — it is
saved on an incomplete source like any other field.

The settings page owns everything else and renders a typed form per section — checkboxes for toggles
and media-type routing, repeatable rows for Bilibili accounts/favourites, Telegram accounts/channels,
Kemono creators and shared CookieCloud configs (the `CookieCloud` block at the bottom of the page,
with a live connection test; Bilibili accounts, X and pixiv reference its entries by name from a
dropdown) — validated locally before submitting. Sources whose only settings are cron/enabled (StellaSora, BD2, Azur Lane)
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
- `POST /api/v2/hanime1/authors`
- `GET /api/v2/hanime1/authors`
- `DELETE /api/v2/hanime1/authors/{author_id}`
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

Azur Lane support is backend-only in this repository. The crawler archives Live2D, Spine and painting resources, writes deterministic manifests, and the API returns metadata plus static asset URLs. Browser rendering, model display, and any Azur Lane client UI belong in a separate application that consumes the API.

A painting webp is a tight-packed sprite sheet, not finished artwork, so the crawler also archives the layer index (`painting/<key>.json`), the sibling layer sheets and the per-layer meshes needed to reassemble it. See [docs/azurlane-api.md](docs/azurlane-api.md#reassembling-a-painting).

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

`enabled` here is the delivery switch for the whole outbox; `web.<source>.notify` on the jobs page
is the per-source one. A muted source is dropped when the notification is queued rather than when it
is delivered — nothing accumulates for it, and turning it back on does not replay what was silenced.
The one thing that ignores the toggle is RedNote's login QR, which goes straight to Telegram
(`send_photo_now`) because it is a prompt the source cannot run without, not a report on a run.


### Azur Lane

Azur Lane crawler settings live in the `web.azurlane` section (`enabled`, `path`, `cron`). Set
`enabled` to true to schedule archive updates. The API reads Azur Lane manifests from the configured
path even when the scheduled job is disabled.

The source advertises models whose files its CDN does not host, and no amount of crawling fixes
that. A model whose required assets fail is put on an escalating retry cooldown — one day, then
three, then a week — during which the run skips it entirely and does not count it as a failure.
It is reported again the first run after the wait lapses, or immediately if the source moves the
model's URL, so a permanent source-side gap is announced weekly rather than every six hours.

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

## Hanime1 Archive Layout

Every downloaded video lands under `{web.hanime1.path}/{genre}/{group}/{title} [id].mp4`. The genre is read from the video's own
watch page (the link next to the artist name) and converted to Simplified Chinese, e.g. `里番`, `泡面番`, `2D动画`, `AI生成`, `MMD`.
Videos whose watch page exposes no genre fall back to `未分类` with a warning log.

- Series subscriptions archive as `{path}/里番/{series}/{title NN} [id].mp4` — point Emby libraries at the `里番` / `泡面番`
  directories for TMDB scraping.
- Author subscriptions archive as `{path}/2D动画/{author}/{title} [id].mp4` — point Stash libraries at the fan-work genre
  directories (`2D动画`, `AI生成`, `MMD`, ...).

Files downloaded before the genre layer existed sit directly under `{path}/{series}/` and are not migrated automatically; move them
into the matching genre directory manually when repointing media libraries. Dedup is database-backed (the `hanime1` table), so
files uploaded and removed from disk by clouddrive2 are never re-downloaded.

## Hanime1 Author Subscriptions

Author (circle) uploads can be subscribed by user id from the Settings page or the API. The worker scans each author's
`/user/{id}/uploaded` listing on every run, downloads new videos through the regular pipeline, and archives them under the author's
display name. Adding a subscription accepts either the numeric id (`202534`) or the profile URL
(`https://hanime1.me/user/202534/uploaded`); the display name is resolved from the profile page at add time and refused if
unavailable. Per-author scan state (`video_count`, `last_scanned_at`, `last_scan_error`) is kept on the `hanime1_author` table, and
a failing author never blocks the series route or the other authors.

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

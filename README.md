# fav

Automation toolkit for collecting content from multiple sources and deduplicating with PostgreSQL.

fav is a single Rust binary (a Cargo workspace at the repository root, crates under `crates/`) plus a
React front end in `web/`. It was ported from Python in November 2026; the last Python commit is tag
`python-final`, and `docs/rewrite/` has the history. `AGENTS.md` describes the layout and conventions.

```bash
cargo build --release -p fav        # target/release/fav
fav serve                           # API, web UI and worker in one process (the default command)
fav api | fav worker                # either half alone
fav trigger <job_key|all>           # run one job now and exit
```

Tests: `FAV_TEST_DATABASE_URL=postgresql://... cargo test --workspace` (database tests create their own
temporary databases). The image is built from `Dockerfile` by CI and published as
`ghcr.io/cyrahs/fav:latest` from `main`.

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
| `CREDENTIAL_CLIENTS` | no | empty | Services allowed to read a stored login from `GET /api/v2/credentials/{site}` (see [Sharing a login](#sharing-a-login-with-other-services)) |
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
`web.hanime1`, `web.jandan`, `web.kemono`, `web.twitter`, `web.pixiv`, `web.rednote`, `web.wechat`,
`notifications.telegram`. The settings page groups them into two partitions — sources (`web.*`) and
notifications (`notifications.*`) — and shows only display names, never these internal keys. Site
logins are not settings: they live on the 登录态 page (see [Site logins](#site-logins)). The
`cookiecloud` section of the retired CookieCloud integration is deleted from `app_settings` on startup,
since it held vault passwords.

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
      "name": "main",                                  // ASCII slug; names its 登录态 login (bilibili-main)
      "favorites": [
        { "fav_id": 123456789, "path": "collection/bilibili/main/fav" },
        { "fav_id": 987654321, "path": "collection/bilibili/main/music" }
      ],
      "toview_enabled": true,                          // watch-later is opt-in per account
      "toview_path": "collection/bilibili/main/later"   // its own directory, not a subdir of the favourites
    }
  ]
}
```

Each favourite list names its own directory, and watch-later has a separate one, so the two no longer
share a parent. An account needs either a favourite list or `toview_enabled` before it is runnable.
Each account signs in on the web UI's 登录态 page, which lists one 「Bilibili（<name>）」 login per account (just 「Bilibili」 when there is only one)
(see [Site logins](#site-logins)); that login *is* the account identity, since the watch-later list is
always the logged-in user's. The session (`SESSDATA`, `bili_jct`, `buvid3`, `DedeUserID`) is stored in
`site_sessions` under `bilibili-<name>`, and each run first asks `/x/web-interface/nav` whether it is
still a login: a session Bilibili rejects marks the login 已失效 and fails the run with a notification to
sign in again, while Bilibili not answering fails the run without touching the login. Before that
check the run keeps the session alive the way a browser would (`crates/fav-sources/src/bilibili/session.rs`): it
fetches a new `bili_ticket` once the stored one has less than two days left, and when
`/cookie/info` asks for it, trades the refresh token (`ac_time_value`, read from the login window's
localStorage when the login is captured and kept in `site_sessions.extra`) for a new `SESSDATA` and
`bili_jct`, storing them before confirming the trade. A failed renewal only logs a warning. A login
captured before this existed has no refresh token; signing in again once adds it. Renaming an
account means signing it in again. (A `cookiecloud` key left in an older account row from the retired
CookieCloud integration is ignored.) Watch-later is still cleared after a successful pass, so only enable it
for accounts whose list you want consumed.

Deduplication is by `bvid` across the whole `bilibili` table, so a video favourited by two accounts
downloads once, into whichever directory is reached first.

#### X liked tweets

`web.twitter` archives the images and videos from your own liked tweets. X sells no affordable read
access to a likes timeline, so the crawl shells out to [gallery-dl](https://github.com/mikf/gallery-dl)
(installed in the image at the version `docker/downloaders/uv.lock` pins) with the browser session the account is signed in
with. gallery-dl owns the parts that break when X changes — GraphQL endpoints, the transaction-id
header, 429 back-off — so a break is usually fixed by a newer gallery-dl rather than by editing this
repository.

That upgrade is unattended: `.github/dependabot.yml` watches gallery-dl daily, and
`.github/workflows/dependabot-auto-merge.yml` enables auto-merge on the pull request so it lands once
the `CI passed` check that main's branch protection requires has passed, then dispatches an image rebuild.
Nothing bypasses a check — a failing check just leaves the pull request open. To upgrade by hand:

```bash
cd docker/downloaders && uv lock --upgrade-package gallery-dl
```

```jsonc
{
  "cron": "0 */6 * * *",
  "enabled": true,
  "username": "yourhandle",                 // your own screen name, without the @
  "path": "collection/twitter",
  "video_path": null,                       // null keeps videos with the images
  "sleep_request_seconds": 2.0,             // spacing between X requests
  "abort_after": 20,                        // consecutive known files that end an incremental run
  "include_retweets": true,                 // liked retweets, filed under the original author
  "include_videos": true,                   // photos only when false
  "proxy": ""                               // also the egress of the 登录态 login
}
```

The session is the `auth_token` and `ct0` cookies of a login made once on the web UI's 登录态 page
(see [Site logins](#site-logins)). The login window leaves through `proxy`, the same egress as the
crawl, so X sees the sign-in and the crawling come from one place; point it at a residential proxy
rather than letting a fresh login come from a datacenter address. When the window's `auth_token`
appears, the login is checked against X's account settings, whose screen name has to be `username`
(the likes timeline is readable only by its owner). Each run writes the stored cookies to a
temporary cookies.txt for gallery-dl. A run that gallery-dl cannot authenticate marks the login
已失效 on the page and fails with one deduplicated `twitter:auth` notification asking for a new
login; with no login captured at all it fails the same way. (A `cookiecloud` key left in an older
`web.twitter` row is ignored.)

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
  "sleep_request_seconds": 1.0,             // spacing between ajax requests; CDN downloads are not paced
  "proxy": ""                               // empty: direct (or the global HTTP_PROXY)
}
```

The bookmarks listing refuses anonymous requests, so the session is a `PHPSESSID` cookie. Sign in once on
the web UI's 登录态 page (see [Site logins](#site-logins)); the cookie is stored in the database and read
at the start of every run. A run that finds pixiv rejecting it marks the login 已失效 there and fails with
one deduplicated notification asking for a new login; with no login captured at all it fails the same
way. (A `cookiecloud` key left in an older `web.pixiv` row is ignored.) The logged-in user's id is embedded
in the cookie's value (`{user_id}_{hash}`), so `user_id` normally stays empty. Image originals on
`i.pximg.net` need only the `Referer` header, never the cookie.

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
identity; on ephemeral storage every restart means signing in again. The documented `docker run` in
`AGENTS.md` mounts `data/`, and a Chromium profile wants real block storage — its LevelDB and SQLite
files do not survive NFS locking.

Signing in happens on the web UI's 登录态 page (see [Site logins](#site-logins)), not during a run.
Its window opens **this source's own profile** (`profile_path`), through the same proxy, presented the
same way the crawl presents it (the Chrome-on-macOS user agent and client hints), and holds the
profile's lock while it is open, so a run that starts meanwhile skips rather than racing it. The login
is the site's own: scan the account QR shown in the window with the app (or use a phone number and
SMS code), then the account-security QR the site usually puts up next when it sees a new device or
address. Once the page reports a signed-in account the window closes and the next run uses the
session straight from the profile; the cookies are never handed to another client.

A run only checks the login. A profile that turns out signed out fails the run with one deduplicated
`rednote:login` notification asking for a new sign-in on the 登录态 page, and marks the login 已失效
there; a run that finds it signed in records that too (and writes the page's row the first time, for
a profile signed in before the page existed).

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
not replaceable. The login window is the account for as long as it is open, so the 登录态 page is
only as private as the web UI's token. And unliking a note does not delete what was already
downloaded.

#### WeChat forwarded images, stickers, articles and videos

`web.wechat` collects the images, stickers (表情), 公众号 articles and 视频号 videos forwarded to a WeChat account that runs in redroid
on another host. The WAuxiliary plugin in `script/wechat_favinbox/` exposes WeChat over HTTP (the
bridge) and also saves the original of every image a private chat receives (the inbox);
`favinbox_server.py` beside it serves both behind one bearer token (`FAVINBOX_TOKEN` on that host),
the bridge under `/wx/`. frp tunnels it into the cluster, where fav reaches it at `api_url`
(`http://favinbox-oracle.proxy.svc` by default).

Every run (`*/15 * * * *` by default) reads the bridge's message events after its cursor
(`wechat_state`) and queues the images, stickers and article shares received in private chats in
`wechat_events`. An image is fetched through `/wx/image` (or from the inbox when that fails) into
`<path>/<YYYY-MM>/<YYYY-MM-DD HHMMSS> [<msg_id>].<ext>` and recorded in `wechat_inbox`. A sticker is
stored and recorded the same way, downloaded from the `cdnurl` its message names (no login needed) and
checked against its `md5`; when that fails, the `encrypturl` copy is decrypted with the message's
`aeskey` (AES-128-CBC, the key doubling as the IV). A large sticker, which WeChat only loads when
it is tapped, arrives as an app message of type 8 instead, with the same addresses and key in the
base64 protobuf of its `<emojiinfo>`. Its Telegram notification plays a GIF as an
animation. Every shared
link is first opened in the account's WeChat through the service's `/link/open`, which waits for the
page to load and closes it again, so the account reads what it is sent before fav saves it. WeChat
opens nothing on command: the FavInbox plugin taps through to the chat and taps the message, as a
person would (`script/wechat_favinbox/README.md`, "Opening messages by tapping"). An article is
fetched from mp.weixin.qq.com with every body image at full size into
`<path>/<YYYY-MM>/<YYYY-MM-DD HHMMSS> <title> [<msg_id>]/`, beside the page and an `article.json`, and
recorded in `wechat_article`. Image posts (贴图), which mp.weixin.qq.com hides behind a captcha, are
read through the service's `/post/page`, which has the account's WeChat open them. A shared Baidu Tieba
post is kept the same way: its first floor is read through the Tieba client API (replies are left out), and its
pictures saved at original size. So is a shared NGA thread, whose first floor is read from `read.php`'s JSON
with the NGA login (`ngaPassportUid`, `ngaPassportCid`) captured on the 登录态 page (see
[Site logins](#site-logins)): NGA shows most boards to members only. NGA issues both cookies for a year and
never needs them refreshed, so one sign-in lasts until NGA ends the session or the year runs out. Until
there is a login, the thread waits without using up its tries; a thread NGA refuses as 未登录 marks the
login 已失效 on the page, and the thread waits the same way. A 视频号 (Channels) video is not opened in WeChat: TikHub's
`fetch_video_detail` (key in `services.tikhub.token`, about $0.01 per lookup) answers with its CDN
address and a `decode_key`. The best encoding that address offers is downloaded (`X-snsvideoflag=xWT111`,
720p for a 1080p upload; the CDN's default is 576p, and the original upload is not offered), and its first 128 KB are decrypted with the ISAAC-64
keystream that key seeds. Without a key the video waits without using up its tries. It is saved as
`<path>/<YYYY-MM>/<YYYY-MM-DD HHMMSS> <caption> [<msg_id>].mp4` with its cover beside it, recorded in
`wechat_channel_video`, and a second forward of the same video (by object id) is skipped. All three
tables are keyed `(msg_id, create_time)`. A failed item is
retried on the next runs, five times at most, and for about a day when what failed is TikHub and not
the item (see [When TikHub is failing](#when-tikhub-is-failing)). When the source learns a new kind of message, it reads
the bridge's event log again from the start once, so earlier messages of that kind are picked up. The run then drains the inbox as a fallback: it downloads what
is not stored yet, and acknowledges each item, which deletes the host's copy. Chat videos and files
are not collected yet.

Every section is constructible from defaults, so an empty database boots with all sources disabled.
Required fields are enforced by `validate_runnable()` only when a source is enabled: the API reports
them as `missing_fields`, and the scheduler keeps an enabled-but-incomplete source parked rather than
crashing.

Secrets (`web.telegram.accounts[].api_hash`, `notifications.telegram.bot_token`,
`web.wechat.token`, `services.tikhub.token`) are stored in
plain text but are masked on read (`aa78••••`). Sending a masked value back — or omitting the field —
keeps the stored secret. Telegram secrets are matched by account name, so reordering accounts in the
UI cannot shuffle credentials between them. (`web.rednote.proxy` and the other proxy URLs are deliberately shown in
plain text.)

The worker polls `app_settings` every 15 seconds and reschedules APScheduler jobs in place, so
`enabled` and `cron` changes apply without a restart. `notify` is read when a notification is
queued, so it applies to the next message either way.

**Known limitation:** the Telegram realtime listener is created at process start. Toggling
`web.telegram.enabled` at runtime only affects its cron reconciliation job; the listener still needs
a worker restart.

## Web UI

`web/` is a React + TypeScript + Vite front end served by fav from `web/dist` when that directory
exists (API-only otherwise). Pages: overview, archive records, jobs, site logins (登录态), settings.
Cron fields show a live natural-language description of the expression.

The jobs page owns `cron`, `enabled` and `notify` for every source, plus manual 立即运行. Sources with an
incomplete configuration cannot be enabled: the toggle is disabled and `PUT /api/v2/settings/{section}`
answers `422 incomplete_settings` with the missing field names. 通知 has no such restriction — it is
saved on an incomplete source like any other field.

The settings page owns everything else and renders a typed form per section — checkboxes for toggles
and media-type routing, repeatable rows for Bilibili accounts/favourites, Telegram accounts/channels,
and Kemono creators — validated locally before submitting. Sources whose only settings are cron/enabled (StellaSora, BD2, Azur Lane)
do not appear there at all; their `path` keeps its default and can be changed through the API if a
deployment needs to. Every listed section also has a `JSON 编辑` toggle for raw editing, which is what
a section gets if this build predates it. Secret fields (`api_hash`, Telegram bot token, the WeChat
tokens) read back masked; leaving the mask in place keeps the stored value. Note that `PUT` replaces a
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

### Site logins

The 登录态 page lists the sources that need a signed-in session (pixiv, X, each Bilibili account, NGA and RedNote) with the state of the
stored one: account, when it was captured, when its cookie expires, and whether the last crawl found it
valid. 登录 starts a browser just for that login (`crates/fav-browser`, sites in `crates/fav-api/src/site_logins.rs`): a TigerVNC
`Xvnc` virtual display on loopback with a headed Chromium on it, opened at the site's login page and
shown in the page through noVNC. That Chromium is a plain process, not one Playwright drives: nothing
attaches to its pages or runs scripts in them, so the site sees an ordinary browser (X refuses
sign-ins from a Playwright-controlled window as "temporarily restricted"). fav only reads its cookie
jar, with `Storage.getCookies` on the browser-level DevTools target on loopback (port 5998), which
never touches a page. Because nothing can answer a proxy's credential challenge, a login proxy must
not need credentials. The VNC WebSocket (`/api/v2/site-sessions/vnc`) is opened with a
ticket that belongs to that one login, since a browser cannot send the bearer token on a WebSocket.

While the window is open the API polls its cookies. When the session cookie appears it asks the site
whether it is a real login (for pixiv, by reading the private bookmarks, which only the owner can;
for X, by reading the account's settings; for Bilibili, from `nav`; for NGA, by reading the
notification list, which turns guests away),
stores the cookies in `site_sessions`, and shuts the browser and the display down. A login left
unfinished closes itself after 15 minutes. Nothing runs between logins; a login takes roughly
400-600 MB while it is open. Each site keeps its Chromium profile under `data/login-profiles/<site>`
so successive logins come from the same browser. A login is refused while the RedNote crawl has its
own browser open, so the two never share the pod's memory limit.

RedNote is the exception to all of this. Its crawl itself drives a Chromium over CDP, so the window is
one too, and it opens the
crawl's own profile (`web.rednote.profile_path`) under the crawl's profile lock, presented the way the
crawl presents it, and the login is confirmed by reading the window's own page rather than by
replaying its cookies anywhere else. `site_sessions` records only which of its cookies exist and when
they expire, for the page's status, never their values; the crawl reads the profile. The window keeps
to one tab: the UA override is per tab, so any second tab is closed as it opens.

#### Sharing a login with other services

Another service on the cluster that needs one of these logins (RSSHub's Bilibili proxy) reads it from
`GET /api/v2/credentials/{site}` rather than keeping a login of its own. The answer is the stored
cookies with their values (`name`, `value`, `domain`, `path`, `expires`), the account and the last
check's status, marked `Cache-Control: no-store`; extras such as the Bilibili refresh token are never
served, and a session whose values live in a browser profile (RedNote) answers 404. The endpoint is read
only: fav stays the one writer, so a client can never refresh a session and retire the cookies fav
holds.

It does not take `API_TOKEN`. Each client has its own bearer token and may read only its sites, given
in `CREDENTIAL_CLIENTS` as whitespace- or `;`-separated `name:sha256:site[,site...]` entries, where
`sha256` is the hex SHA-256 of the client's token (`printf %s "$TOKEN" | sha256sum`). Only the hash is
configured on fav's side, so the value is not a secret. A malformed value stops the API from starting.

## API Backend

This repository exposes an internal HTTP API (axum, `crates/fav-api`) for control, archived catalog reads, and Hanime1 scan-target creation.

- `fav worker` is the worker and scheduler; `fav api` serves the API with its OpenAPI document.
- The image's default command, `fav serve`, runs both in one process.
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
- `GET /api/v2/site-sessions`
- `POST /api/v2/site-sessions/{site}/login`
- `DELETE /api/v2/site-sessions/login`
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

All `/api/v2/*` endpoints except `/api/v2/credentials/{site}` (see
[Sharing a login](#sharing-a-login-with-other-services)) require:

- `Authorization: Bearer <API_TOKEN>`

The API contract is OpenAPI-first. Use `/openapi.json` or `/docs` for the exact request and response schema.

Nikke, BD2, and Azur Lane asset URLs in API responses point to `/static/{source}/{directory_name}/assets/...` and are intended to be served by the deployment's static file service, not by fav.
For Azur Lane specifically, map `/static/azurlane/` to the configured `web.azurlane.path` directory so returned URLs resolve to archived files under each character directory.

Azur Lane support is backend-only in this repository. The crawler archives Live2D, Spine and painting resources, writes deterministic manifests, and the API returns metadata plus static asset URLs. Browser rendering, model display, and any Azur Lane client UI belong in a separate application that consumes the API.

A painting webp is a tight-packed sprite sheet, not finished artwork, so the crawler also archives the layer index (`painting/<key>.json`), the sibling layer sheets and the per-layer meshes needed to reassemble it. See [docs/azurlane-api.md](docs/azurlane-api.md#reassembling-a-painting).

### MCP endpoint

`/mcp` serves an MCP server (streamable HTTP, stateless, JSON responses) for developing and operating
fav without a shell in the pod. It takes no token of its own, like the cluster's other MCP servers: the
LiteLLM MCP gateway reaches it in-cluster, and the public hostname sits behind Cloudflare Access.
Tools (`crates/fav-api/src/mcp/tools.rs`, queries in `crates/fav-api/src/mcp/ops.rs`):

- Read: `get_runtime` (deployed git commit, build time, process start times), `list_jobs`, `list_runs`,
  `get_run`, `read_logs` (the log files under `log/`, which survive pod restarts), `get_settings`,
  `wechat_status`, `list_wechat_events`, `site_sessions` (stored logins, never cookie values),
  `list_notifications`, `list_archive_sources`, `search_archive`.
- Write: `run_job`, `update_settings` (partial, validated like a UI save), `retry_wechat_events`,
  `set_wechat_state`, `retry_notifications`, `send_test_notification`.

There is no tool for arbitrary SQL. Secret values are masked in settings, and redacted from logs and
error text. `get_runtime` reads `FAV_GIT_COMMIT` and `FAV_BUILD_TIME`, which CI passes as build args.

## BD2 L2D Viewer comparison

BD2 is collected from GameKee. The Python app had a script that compared the archived GameKee Live2D
resource stems with the resources listed by [BD2-L2D-Viewer](https://jelosus2.github.io/BD2-L2D-Viewer/)
(`script/bd2_compare_l2d_viewer.py`, at tag `python-final`); it was not ported.

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
delivered message. A failure that repeats re-pins its latest message and unpins the previous one, and
the recovery message unpins the failure, so one problem never holds more than one pin. Only the first
occurrence of a failure rings the phone; repeats arrive silently with an occurrence count. The API origin
is fixed to `https://api.telegram.org` so a retained masked token cannot be redirected to another host.

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
fav api
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

## Post Links

WeChat (a shared link card) and Telegram (a message's text or link preview) save links to the same kinds of post, through
one module, `crates/fav-sources/src/post_links.rs`: 公众号 articles, Baidu Tieba posts, NGA threads, and Zhihu answers
(`zhihu.com/question/<q>/answer/<id>` or `zhihu.com/answer/<id>`), column articles (`zhuanlan.zhihu.com/p/<id>`) and pins
(`zhihu.com/pin/<id>`). It recognises the link, reads the post's pictures and videos, and downloads them; each source
decides where the files go and how the save is recorded.

An NGA thread needs the NGA login captured on the 登录态 page. Zhihu answers datacenter addresses with HTTP 403 and a
login wall, so a Zhihu post is read in a signed-in Chromium profile: sign in to 知乎 on the 登录态 page once, and every
Zhihu link opens its page in that profile (`<RedNote profile's folder>/zhihu-profile`, through `web.rednote.proxy`, one
browser per post under the profile's advisory lock) and takes the post from the page's own `js-initialData`, or from
`/api/v4` asked inside the page. Until Zhihu is signed in there, a post is looked up through TikHub instead
(`fetch_answer_detail`, `fetch_column_article_detail`, `fetch_pin_detail`, about $0.001 a lookup). Either way its
pictures come at full size from `*.zhimg.com` and its videos at the widest quality `lens.zhihu.com` lists. A profile that
lost its login, or that Zhihu puts a security check in front of, marks the 知乎 session invalid and its links wait for a
new sign-in; a browser that fails to read the page counts as an outage ([When TikHub is failing](#when-tikhub-is-failing)).
Without the login or the TikHub key, a link waits without using up its tries. A 公众号
image post sent through Telegram cannot be read (mp.weixin.qq.com puts it behind a captcha); share it to WeChat instead.

### Messages With Nothing To Save

Every message sent to the WeChat account, or to a Telegram channel with `report_unsupported` on (设置 → Telegram, 报告未下载:
a channel someone sends things to on purpose, not one only listened to), is expected to hold something to save. One that does not
(a link to a site no source reads, a text, a file, a voice message) is reported with a `download_failed` notification
naming what it held, instead of being skipped. WeChat leaves out system notices, recalled messages and 拍一拍; Telegram
leaves out service messages, and a picture or video of a type the channel has no route for. Only messages that arrive
after this was first deployed are reported: WeChat records the moment in `wechat_state.unsupported_since`, and Telegram
keeps them out of its look back over a channel's history.

The report is an `unsupported_message` notification, pinned like a failing job. The message is checked again on every
run (WeChat keeps its row as `reported`, Telegram re-reads the channel's reported messages on each reconciliation): once
a later version can save it, it is queued as real work, and saving it resolves the report, which unpins it. Deleting a
reported Telegram message stops the checks but leaves the pin to be removed by hand, as does any report that is to be
ignored.

### TikHub

TikHub is configured once, in its own `services.tikhub` section (`token`, `api_url`; 设置 → 第三方服务), and used by the
WeChat 视频号 lookup and by Zhihu links from either source. `POST /api/v2/services/tikhub/test` checks the saved key with
TikHub's free account lookup and answers with the key's name, balance and free credit; the settings page shows it
when the section opens, with a button to refresh it. A database that still holds the
key under the old `web.wechat.tikhub_token` / `tikhub_api_url` fields has them moved to `services.tikhub` at startup.

#### When TikHub is failing

A lookup can fail because TikHub itself is failing: no answer, a rate limit or a server error, or its "Request failed.
Please retry" answer, which it gives for every lookup while one of its APIs is down. The post or video may be fine then,
so it is not given up with the five tries an unreadable one gets. Telegram and WeChat follow one rule for this
(`link_retry`): its first four failed tries are retried like any other item's; a fifth that fails this way no longer
gives it up: it stays queued and is tried again for about a day before it is given up and reported. Each source keeps its own pace. A Telegram link is retried with the
queue's backoff, 30 minutes apart from the seventh try on, 53 tries in all. A WeChat item is tried once per run, 48 tries
in all, its first four failures fail the run as any failed item does, and from the fifth on the run no longer fails over
it; a link is not opened in WeChat again for those later tries.

The outage has one `download_failed` notification for everything it stops, whichever source runs into it: it is sent by
the first item that reaches its fifth try while none is open. It is resolved when the last item of a source that waited
on the outage is saved or given up, and not by a save in between: while TikHub fails on and off, or for one post only,
the notification stands instead of being closed and sent again with every save. TikHub's Zhihu API and its 视频号 API
fail apart from each other, so each has a notification of its own (`tikhub:outage:zhihu`, `tikhub:outage:channels`).
TikHub gives the same "Request failed" answer for some posts it cannot read at all; such an item also waits the day out,
and closes the notification when it is given up. An item that waited is given up by the first failure of its own (a
post that is gone, a download that fails) once TikHub answers again.

### In Telegram

A message that carries no media of its own and links one of these posts is queued as a `link` job in every configured
channel. The pictures go to the channel's `image` route and the videos to its `video` route; a channel with only one
route takes both. Files are named `[author]title-N [message id].ext`, where the title is the post's (for Zhihu, the
question's, the article's or the pin's opening text). A link whose post cannot be read is retried with the queue's backoff
and given up after five tries with a `download_failed` notification; one that waits on a login or a key is retried every
30 minutes with a notification saying what it needs. One whose lookup fails because TikHub itself is failing waits the
outage out for about a day instead (see [When TikHub is failing](#when-tikhub-is-failing)). Links are scanned as a type of their own, so the first run after an
upgrade backfills the latest `scan_limit` messages of each channel for them, as for a newly configured media type.

## Telegram Downloader Safety

Telegram accounts keep a long-running MTProto connection (grammers, reading Telethon-format session files). New single-media messages and albums are persisted to
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

The image's default command, `fav serve`, runs the scheduler worker and the API server in one process.

## Hanime1 userscript

`script/hanime1_downloaded_marker.user.js` consumes the Hanime1 downloaded-list endpoint.

- Endpoint: `GET /api/v2/hanime1/videos`
- Auth: `Authorization: Bearer <API_TOKEN>`
- Response shape: `{"items":[{"video_id":"1001","title":"...","downloaded":true,"uploader":"...","release_date":"2024-01-01","plot":"...","watch_url":"https://hanime1.me/watch?v=1001"}],"total":1}`

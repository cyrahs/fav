# FavInbox (WAuxiliary plugin)

A [WAuxiliary](https://github.com/HdShare/WAuxiliary_Plugin) plugin for the agent-owned
WeChat account running in redroid on the `oracle` host. It does two things:

- **Inbox**: it saves the **original** of every incoming private-chat image into
  an inbox directory the host can read, so the fav `wechat` source can pick the
  files up.
- **Bridge** (since 0.3.0): it runs an HTTP API inside the WeChat process that
  exposes what WAuxiliary can do (message events, history, original images,
  contacts, groups, sending, Moments), so fav decides what to do with WeChat
  instead of the plugin. See [Bridge API](#bridge-api).

It is not imported by the fav app; it runs inside WeChat via WAuxiliary/LSPosed.

- `info.prop` — plugin manifest (name/author/version).
- `main.java` — the plugin (BeanShell-style Java, interpreted by WAuxiliary).
- `favinbox_server.py` — the host-side HTTP service fav talks to.

## What it does

- `onHandleMsg`: for a received private-chat image, downloads the original
  (`downloadImg`, falling back to the local `imgPath`), strips the 24-byte
  trailer WeChat appends, sniffs the real extension, and writes
  `<msgId>.<ext>` plus a `<msgId>.json` (written last, marking the item done).
  Failures write `<msgId>.err` so a later backfill retries them. Group chats,
  official accounts and outgoing messages are skipped. Videos and files are
  recorded as JSON only (no download API yet).
- Long-pressing a conversation adds a **"FavInbox 补抓图片"** menu item that
  pages back through the conversation's history and saves any images not saved
  yet.
- `/ui/open` and `/ui/close` (bridge) open a message the way a person does,
  with taps; see "Opening messages by tapping" below.

Inbox directory (inside the container):
`/sdcard/Android/data/com.tencent.mm/files/FavInbox`, which on the host is
`/home/ubuntu/redroid-data/media/0/Android/data/com.tencent.mm/files/FavInbox`.

## Install / update

Copy both files into the plugin folder, matching the owner and modes of the
existing plugins, then reload in WAuxiliary.

```sh
DIR=/data/media/0/Android/media/com.tencent.mm/WAuxiliary/Plugin/FavInbox
su 0 sh -c "mkdir -p $DIR && cp info.prop main.java $DIR/ \
  && chown u0_a68:media_rw $DIR/* && chmod 660 $DIR/*"
```

## HTTP API (`favinbox_server.py`)

A dependency-free (stdlib only) HTTP service that lets the fav crawler read and
drain the inbox over the frpc tunnel. It runs on the oracle host, binds
`127.0.0.1`, and requires a bearer token.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/health` | liveness, no auth |
| GET | `/items[?include_acked=1]` | list items from the JSON sidecars |
| GET | `/file/<msg_id>` | the image bytes |
| POST | `/ack/<msg_id>` | mark acked, delete the media file, keep the JSON |
| POST | `/link/open` | open a link in WeChat, wait for it to load, close it |
| POST | `/post/page` | a 公众号 post's page, as WeChat itself loads it |
| POST | `/finder/video` | a 视频号 video, as WeChat's player cached it |

`ack` keeps `<msg_id>.json` (with `status: acked`) as a permanent dedupe
tombstone so the plugin's backfill never re-saves an item fav already has, and
deletes only the media to reclaim disk. Dedupe is on `msg_id`; the `md5` in the
JSON is WeChat's own value and does not match the saved bytes.

Env: `FAVINBOX_DIR` (required), `FAVINBOX_TOKEN` (required), `FAVINBOX_HOST`
(default `127.0.0.1`), `FAVINBOX_PORT` (default `9970`), `FAVINBOX_XWEB_CACHE`
(default `/home/ubuntu/redroid-data/data/com.tencent.mm/cache/xweb_cache`, the
host side of WeChat's internal `/data/data/com.tencent.mm/cache/xweb_cache`), `FAVINBOX_WECHAT_DATA` (default
`/home/ubuntu/redroid-data/data/com.tencent.mm`, the host side of `/data/data/com.tencent.mm`).

`/link/open`, `/post/page` and `/finder/video` get WeChat to open a message by
having the plugin tap it (bridge `POST /ui/open`, then `POST /ui/close`), so
each names the message: `"message": {"talker": "<wxid>", "msg_id": <id>,
"create_time": <ms>, "texts": ["<the card's title>", ...]}`. Nothing is started
over adb, and nothing is fetched without being opened first, however old the
message; reaching an old one can take minutes of scrolling.

`/link/open` is how fav reads a shared link (公众号 article, Tieba post, NGA
thread) the way the account's owner would: before fetching it, fav has WeChat
open it. It takes `{"url": "<share link>", "message": {...}, "wait": 20}` (at
most 60), taps the message open, polls the web view's HTTP cache until it holds
a complete response for the link's host and path written after the open, leaves
the page up a few seconds more, then backs out to the chat list. It answers
`{"loaded": true, "seconds": 7.2, "steps": [...]}`; a page that never reaches
the cache (uncacheable, or redirected to another host) is closed after `wait`
with `loaded: false`. Errors: 400 for anything but an http(s) URL of printable
ASCII or a missing message, 502 when the message could not be opened (the
plugin's steps are in the error).

`/post/page` is for image posts (贴图), which mp.weixin.qq.com hides behind a
slider captcha from every client but WeChat. It takes
`{"url": "<share link>", "message": {...}, "wait": 30}` and answers with the
post's HTML. If WeChat's web view (XWeb) has the post in its HTTP cache, that
copy is used; otherwise the message is tapped open, the cache polled until the
page arrives, and WeChat backed out to its chat list. The cache is Chromium's Simple Cache: `<hash>_0` files under
`xweb_cache/<profile>/HTTP Cache/Cache_Data/`, each holding the URL, the body
as received (gzip or brotli) and the response headers. Brotli needs
`python3-brotli` on the host. Errors: 400 for a link without
`__biz`/`mid`/`idx`/`sn`, 404 when the page never reached the cache, 502 when
the message could not be opened.

`/finder/video` is for 视频号 videos. The share message names the feed
(`objectId`, `objectNonceId`) but its video address carries no token or key, so
only WeChat can fetch it. The endpoint takes
`{"object_id": "<objectId>", "message": {...}, "wait": 120}` (at most 300) and answers with the MP4. WeChat's player caches what it downloads as
plain MP4 files in `cache/<account>/finder/video/`, indexed per feed in the
account's `MicroMsg/<account>/FinderMM*.db`, table `FinderMediaCacheInfoV4`
(`feedId` = the object id as a signed 64-bit number, `mediaId` = the file name,
`totalSize`). The server reads that index from a copy of the database (with its
WAL), and returns the file once its MP4 boxes add up to its size. If it is not
there yet, the message is tapped open and the index polled until the file is
whole, then WeChat is backed out to its chat list. Opening a video
also preloads a few recommended ones into the same directory, which is why the
file is found through the index. The account is the one whose `FinderMM*.db`
changed last, so a new login is picked up on its own. 视频号 only works for an
account registered in mainland China: an overseas one gets empty feeds and
「轻触重试」. Errors: 400 for a bad id, 404 when the video was not whole within
`wait`, 502 when the message could not be opened, 503 when no account has a
视频号 database.

### Deploy on oracle

1. Copy the server and create the unit + token file (as root):
   ```sh
   install -D -m 0644 favinbox_server.py /opt/favinbox/favinbox_server.py
   install -m 0644 deploy/favinbox.service /etc/systemd/system/favinbox.service
   umask 077; printf 'FAVINBOX_TOKEN=%s\n' "$(openssl rand -hex 32)" > /etc/favinbox.env
   systemctl daemon-reload && systemctl enable --now favinbox
   curl -s localhost:9970/health        # {"ok": true}
   ```
2. Add the proxy in `deploy/frpc-proxy.toml` to `/etc/frp/frpc.toml`, then
   `systemctl restart frpc`.
3. Cluster side (the `charys117/nas-gitops` repo): the `favinbox-oracle` Service
   and the `9970-9979` frps `allowPorts` range are added there, not here —
   `deploy/nas-gitops-service.yaml` is the manifest, kept as a reference copy.

fav then reaches the API at `http://favinbox-oracle.proxy.svc:80` with the same
token in `Authorization: Bearer <token>`.

## Bridge API

The plugin listens on port `9971` inside the redroid container (all
interfaces; the container sits on Docker's default bridge, `172.17.0.2` on
oracle). Every route except `/health` needs `Authorization: Bearer <token>`,
where the token is a random value the plugin writes on first load to
`FavBridge/token` in WeChat's external files directory (host:
`/home/ubuntu/redroid-data/media/0/Android/data/com.tencent.mm/files/FavBridge/token`).

fav does not use that token or port directly: `favinbox_server.py` forwards
`/wx/<route>` to the bridge, after checking fav's usual FavInbox token, and adds
the bridge token itself. So from the cluster it is
`http://favinbox-oracle.proxy.svc/wx/<route>` with the existing token. Set
`FAVINBOX_BRIDGE_URL` on the host if the container's address changes (it can,
when the container is recreated).

Plugin config (`config.prop` in the plugin folder, WAuxiliary's own
`getBoolean`/`getInt` store): `bridge_enabled` (default true), `bridge_port`
(default 9971), `inbox_enabled` (default true; turn off once fav reads images
through the bridge).

Errors come back as `{"ok": false, "error": "..."}` with 400 (bad input),
401, 404 (unknown route or message) or 500. A WAuxiliary function the installed
version lacks answers 500 with `Command not found: <name>()`.

### Reading

| Method | Route | Returns |
| --- | --- | --- |
| GET | `/health` | `{ok, version}`, no auth |
| GET | `/status` | plugin version, login wxid/alias, WeChat version, `last_seq` |
| GET | `/events?after=N&limit=200&wait=0` | events with `seq > N`, oldest first; `wait` (≤60 s) long-polls for the first new one |
| GET | `/history?talker=&start=0&order=asc&count=100` | `queryHistoryMsg`, as message objects |
| GET | `/message?talker=&msg_id=[&create_time=]` | one message |
| GET | `/image?talker=&msg_id=[&create_time=]` | the original image bytes (trailer stripped) |
| GET | `/image?md5=&cdn_url=&aes_key=` | the same, straight from CDN fields |
| GET | `/friends`, `/officials`, `/groups`, `/labels` | contact lists (every getter of WAuxiliary's bean, snake_case) |
| GET | `/group/members?room=` | `{count, members}` |
| GET | `/contact?wxid=[&room=]` | nick, remark, name, group display name, avatar URLs |
| GET | `/label/contacts?id=` or `?name=` | wxids carrying a label |

Events live in `FavBridge/events.jsonl` (rolled to `events.1.jsonl` at
16 MB, so at most ~32 MB is kept; a gap in `seq` means the reader fell behind
the retention). Each is `{seq, kind, at, data}`:

- `message` — every message `onHandleMsg` sees, sent or received, in any chat.
  `data` has `msg_id`, `type`, `kind` (text, image, video, voice, emoji, link,
  file, app, quote, pat, location, share_card, system, recalled, ...), `chat`
  (private, group, official, im_private, im_group), `create_time` (ms),
  `talker`, `sender`, `is_send`, `is_at_me`, `at_users`, `content` (for app and
  link messages the raw `<msg><appmsg>` XML, which carries the URL of a shared
  article), `msg_source`, and `image` / `file` / `quote` / `pat` when present.
  A shared 公众号 article arrives as `kind: app` (type 49), not `link`: tell it
  apart by `<type>5</type>` inside `<appmsg>`, and fetch the `<url>` exactly as
  given, since the bare `__biz/mid/idx/sn` form is answered with a captcha.
  Image posts (`<itemshowtype>8</itemshowtype>`) are always answered with
  Tencent's slider captcha; their share names only the first picture
  (`coverpicimageurl`, full size) and the count (`piccount`). fav reads them
  through `/post/page` instead (see the HTTP API above).
- `new_friend` — `wxid`, `ticket`, `scene`, `info` (pass these to
  `/friend/verify`).
- `member_change` — `change` (join/left), `room`, `wxid`, `name`.
- `pay` — the received payment bean.

### Acting

All take a JSON body. Media is uploaded first with `POST /upload?name=<file>`
(raw bytes as the body, up to 200 MB, kept for a day); the returned `path` is
what the media routes accept, and nothing outside that upload directory can be
sent.

| Route | Body |
| --- | --- |
| `/send/text` | `talker, content` → `svr_id` (WeChat's server id, null if it did not confirm in 15 s) |
| `/send/quote` | `talker, content, msg_id` |
| `/send/image`, `/send/emoji` | `talker, path[, quote_msg_id]` |
| `/send/video` | `talker, path` |
| `/send/voice` | `talker, path[, duration]` (Silk; convert MP3 with `/audio/mp3-to-silk`) |
| `/send/file` | `talker, path[, title, app_id]` |
| `/send/link` | `talker, title, url[, description, app_id]` |
| `/send/share-text`, `/send/share-video`, `/send/music`, `/send/mini-program`, `/send/app-brand` | WAuxiliary's share helpers, fields named after its parameters |
| `/send/pat` | `talker, user` |
| `/send/card` | `talker, wxid` |
| `/send/location` | `talker, poi_name, x, y[, label, scale]` |
| `/send/note`, `/send/cipher` | `talker, content` (cipher also `title`) |
| `/msg/revoke` | `msg_id` |
| `/msg/system` | `talker, content[, create_time]` → `msg_id` |
| `/friend/verify` | `wxid, ticket, scene[, privacy]` |
| `/group/members/add`, `/invite`, `/remove` | `room, members[, reason]` |
| `/contact/labels` | `wxid, labels` |
| `/sns/post` | `content[, images]` (Moments) |
| `/audio/mp3-to-silk` | `path` → `{code, path}` |
| `/toast` | `text` |
| `/device-step` | `step` |
| `/ui/open` | `talker, msg_id[, create_time, texts]` → `{opened, found_by, steps}` |
| `/ui/close` | `[home]` (default true) → `{steps}` |

`GET /ui/dump` lists what the plugin sees in WeChat's top activity: each shown
view's class, screen bounds, text (cut to 16 characters), whether it is
clickable, and the message or conversation its tag carries.

`GET /ui/selftest` checks how WAuxiliary's BeanShell handles a few forms the
bridge relies on: try/finally, break, continue, a local captured by a scripted
`Callable`, and a script error inside one (`thrown` when it surfaces as an
exception, `nothing` when the interpreter turns it into a null). Up to v0.4.2
every POST route came back empty under WAuxiliary although each passed under
BeanShell 2.0b6, so the bridge now reads a request's body before handing the
request to its route, catches the route's failure inside the `Callable` that
runs it, and answers a route that still returns nothing with a 500 naming it.

### Opening messages by tapping

`/ui/open` opens a message the way a person does. It brings WeChat to the front
if it is not (as its launcher icon would), taps Back until WeChat shows its chat
list or the right chat, taps the conversation, finds the message's bubble and
taps it. When the bubble is not on screen it scrolls the chat until it is,
however far back the message is: up by default, and down when the send times of
the bubbles on screen (their tags' `field_createTime`) say the message's
`create_time` is below them. WeChat 8.0.78 leaves `field_createTime` at 0 on the
bubbles, so it then compares message ids instead, which WeChat hands out in
arrival order. It stops only when a few swipes move nothing (the top or bottom
of the chat), when the bubbles on screen span the message's send time (or id)
but it is not among them (hidden or deleted), or after 400 swipes. It
answers once the chat has lost the focus to what opened; when a tap changes nothing it taps once
more, then gives up with `opened: false` and the steps it took. `/ui/close`
taps Back until the chat list shows again; it never presses Back on the chat
list itself, which would send WeChat to the background.

Views are recognised by what they carry, not by WeChat's obfuscated class
names: a bubble's tag holds its message (an object with a public
`field_msgId` and `field_talker`), and a chat list row may hold its
conversation (`field_username`). When tags do not tell, visible text does: the
contact's name in the chat list (below the title bar), the card's title
(`texts`) in the chat. Of several views carrying the message, the largest
clickable one that is not avatar-sized is tapped.

Touches are real `MotionEvent`s (finger, touchscreen, a few pixels of drift, a
press of 70-150 ms) injected with `Instrumentation`, which Android accepts for
windows of the injecting app; if the system refuses, or an injected tap changes
nothing, later ones are dispatched straight into the activity's window. Back is
an injected `KEYCODE_BACK`; without injection it is the activity's own
`onBackPressed()`, or, when the window on top belongs to another WeChat process
(the web view), WeChat's launcher intent with `FLAG_ACTIVITY_CLEAR_TOP`.

Not exposed: downloading videos, voice and files (WAuxiliary has no API for
them; their CDN fields are in the event), and WAuxiliary's `eval`, hook,
reflection and DexKit helpers, which would amount to running arbitrary code in
WeChat.

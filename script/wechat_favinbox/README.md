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
| POST | `/post/page` | a 公众号 post's page, as WeChat itself loads it |

`ack` keeps `<msg_id>.json` (with `status: acked`) as a permanent dedupe
tombstone so the plugin's backfill never re-saves an item fav already has, and
deletes only the media to reclaim disk. Dedupe is on `msg_id`; the `md5` in the
JSON is WeChat's own value and does not match the saved bytes.

Env: `FAVINBOX_DIR` (required), `FAVINBOX_TOKEN` (required), `FAVINBOX_HOST`
(default `127.0.0.1`), `FAVINBOX_PORT` (default `9970`), `FAVINBOX_XWEB_CACHE`
(default `cache/xweb_cache` in the WeChat app directory), `FAVINBOX_ADB`
(default `adb`), `FAVINBOX_ADB_SERIAL` (default `127.0.0.1:5555`).

`/post/page` is for image posts (贴图), which mp.weixin.qq.com hides behind a
slider captcha from every client but WeChat. It takes
`{"url": "<share link>", "wait": 30}` and answers with the post's HTML. If
WeChat's web view (XWeb) has the post in its HTTP cache, that copy is used;
otherwise the server runs, over adb,
`am start -n com.tencent.mm/.plugin.webview.ui.tools.WebViewUI --es rawUrl <post>`
(as root inside redroid), polls the cache until the page arrives, then presses
Back. The cache is Chromium's Simple Cache: `<hash>_0` files under
`xweb_cache/<profile>/HTTP Cache/Cache_Data/`, each holding the URL, the body
as received (gzip or brotli) and the response headers. Brotli needs
`python3-brotli` on the host. Errors: 400 for a link without
`__biz`/`mid`/`idx`/`sn`, 404 when the page never reached the cache, 502 when
adb failed.

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

Not exposed: downloading videos, voice and files (WAuxiliary has no API for
them; their CDN fields are in the event), and WAuxiliary's `eval`, hook,
reflection and DexKit helpers, which would amount to running arbitrary code in
WeChat.

# FavInbox (WAuxiliary plugin)

A [WAuxiliary](https://github.com/HDSHARE/WAuxiliary) plugin for the agent-owned
WeChat account running in redroid on the `oracle` host. It saves the **original**
of every incoming private-chat image into an inbox directory the host can read,
so a downstream job can pick the files up. It is not imported by the fav app; it
runs inside WeChat via WAuxiliary/LSPosed.

- `info.prop` — plugin manifest (name/author/version).
- `main.java` — the plugin (BeanShell-style Java, interpreted by WAuxiliary).

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

`ack` keeps `<msg_id>.json` (with `status: acked`) as a permanent dedupe
tombstone so the plugin's backfill never re-saves an item fav already has, and
deletes only the media to reclaim disk. Dedupe is on `msg_id`; the `md5` in the
JSON is WeChat's own value and does not match the saved bytes.

Env: `FAVINBOX_DIR` (required), `FAVINBOX_TOKEN` (required), `FAVINBOX_HOST`
(default `127.0.0.1`), `FAVINBOX_PORT` (default `9970`).

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

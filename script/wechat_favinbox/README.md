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

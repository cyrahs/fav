# redroid web console (ws-scrcpy)

A browser view of the redroid container on the `oracle` host (redroid-1) that
can be tapped, swiped and typed into, so the agent-owned WeChat account can be
used by hand without an SSH tunnel and a local scrcpy.

It is [ws-scrcpy](https://github.com/NetrisTV/ws-scrcpy) built from a pinned
commit, running on the host and reaching redroid through the existing adb
server (`127.0.0.1:5555`). Not part of the fav app.

```
browser ──https/wss──> Cloudflare Access ──> Traefik (nas-us)
  ──> redroid.s117.me ──> Service proxy/redroid-oracle (80 -> frps 9971)
  ──frp──> oracle frpc ──> 127.0.0.1:8000 ws-scrcpy ──adb──> redroid
```

## Files

- `install.sh` — clones ws-scrcpy at the pinned commit, applies the build
  config and the loopback patch, builds, installs to `/opt/ws-scrcpy` and
  (re)starts the `ws-scrcpy` service. Run as `ubuntu`.
- `build.config.override.json` — turns off the remote shell, file browser and
  devtools, and keeps the on-device scrcpy server on the device's loopback.
- `config.yaml` — HTTP on port 8000.
- `ws-scrcpy.service` — systemd unit, runs as `ubuntu`.
- `frpc-proxy.toml` — the frpc proxy (local 8000 -> frps 9971).

## Security

ws-scrcpy has no authentication of its own. The service binds 127.0.0.1 only
(upstream listens on every interface; `install.sh` patches that), and the only
public way in is `redroid.s117.me`, behind the Cloudflare Access catch-all on
`*.s117.me`. With the shell and file browser compiled out, the page can only
show the screen and send input.

## Use

Open `https://redroid.s117.me/`, pick the device (`127.0.0.1:5555`) and a
player (WebCodecs in Chromium, Broadway elsewhere). The host has 2 vCPUs and
encodes in software, so keep the stream small in the video settings: max size
720–1024, bit rate 2–4 Mbps, max fps 30 or lower.

Upstream notes the on-device scrcpy server keeps running after the last viewer
leaves.

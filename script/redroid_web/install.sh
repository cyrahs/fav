#!/usr/bin/env bash
# Build ws-scrcpy from a pinned upstream commit and install it as the
# ws-scrcpy systemd service on the oracle host. Run as `ubuntu` (uses sudo for
# /opt and systemd) from the directory holding the files next to this script.
set -euo pipefail

REPO=https://github.com/NetrisTV/ws-scrcpy.git
REF=cd6cea6ebde17af6685c2b325183fc24bef9d52b  # master, 2026-09-30
HERE=$(cd "$(dirname "$0")" && pwd)
BUILD=${BUILD_DIR:-$HOME/ws-scrcpy-build}
export PATH="$HOME/.local/bin:$PATH"

rm -rf "$BUILD"
git clone --quiet "$REPO" "$BUILD"
git -C "$BUILD" checkout --quiet --detach "$REF"

# Drop the remote shell, file browser and devtools, and keep scrcpy on the
# device's loopback (the browser reaches it through this server's proxy).
cp "$HERE/build.config.override.json" "$BUILD/build.config.override.json"

# Upstream listens on every interface and has no host option; bind loopback.
http_server="$BUILD/src/server/services/HttpServer.ts"
sed -i "s/server.listen(port, () => {/server.listen(port, process.env.WS_SCRCPY_HOST || '127.0.0.1', () => {/" "$http_server"
grep -q "process.env.WS_SCRCPY_HOST" "$http_server" || { echo "listen patch did not apply" >&2; exit 1; }

# The adb server's emulator scan also lists redroid's 127.0.0.1:5555 as
# emulator-5554, so the same device would show twice and both entries would
# race to start scrcpy on its port 8886. Skip serials matching
# WS_SCRCPY_IGNORE_UDID (set in ws-scrcpy.service).
control_center="$BUILD/src/server/goog-device/services/ControlCenter.ts"
sed -i "s/    private handleConnected(udid: string, state: string): void {/&\n        const ignore = process.env.WS_SCRCPY_IGNORE_UDID;\n        if (ignore \&\& new RegExp(ignore).test(udid)) {\n            return;\n        }/" "$control_center"
grep -q "WS_SCRCPY_IGNORE_UDID" "$control_center" || { echo "ignore patch did not apply" >&2; exit 1; }

# --ignore-scripts skips the Appium postinstall and the node-pty native build;
# the server bundle uses neither once the remote shell is off.
(cd "$BUILD" && npm ci --ignore-scripts --no-audit --no-fund && npm run dist)
(cd "$BUILD/dist" && npm install --omit=dev --ignore-scripts --no-audit --no-fund)

sudo rm -rf /opt/ws-scrcpy.new
sudo cp -a "$BUILD/dist" /opt/ws-scrcpy.new
sudo install -m 644 "$HERE/config.yaml" /opt/ws-scrcpy.new/config.yaml
sudo chown -R root:root /opt/ws-scrcpy.new
sudo rm -rf /opt/ws-scrcpy.old
[ -d /opt/ws-scrcpy ] && sudo mv /opt/ws-scrcpy /opt/ws-scrcpy.old
sudo mv /opt/ws-scrcpy.new /opt/ws-scrcpy

sudo install -m 644 "$HERE/ws-scrcpy.service" /etc/systemd/system/ws-scrcpy.service
sudo systemctl daemon-reload
sudo systemctl enable ws-scrcpy
sudo systemctl restart ws-scrcpy

for _ in $(seq 1 15); do
  curl -fsS -o /dev/null http://127.0.0.1:8000/ && break
  sleep 1
done
systemctl --no-pager --lines=15 status ws-scrcpy
curl -fsS http://127.0.0.1:8000/ | head -c 120; echo

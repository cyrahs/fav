#!/usr/bin/env bash
# Install or update oracle-mcp on the oracle host (redroid-1). Run as root
# from a copy of this directory, e.g. from a nas-gitops checkout:
#
#   scp -r tools/oracle-mcp <oracle>:/tmp/oracle-mcp
#   ssh -t <oracle> sudo bash /tmp/oracle-mcp/install.sh
#
# Idempotent. It
#   1. copies server.py to /opt/oracle-mcp and installs the pinned, hashed
#      dependencies (requirements.txt) into /opt/oracle-mcp/.venv, built with
#      the system /usr/bin/python3 (not Hermes' bundled Python);
#   2. creates /etc/oracle-mcp.env with two random tokens if it is missing;
#   3. installs and (re)starts the oracle-mcp unit and waits for /healthz;
#   4. adds the oracle-mcp proxy to /etc/frp/frpc.toml if it is not there yet,
#      checks the file with `frpc verify` and restarts frpc (its other tunnels
#      drop for a few seconds); on a failed check the old file is restored.
set -euo pipefail

SRC=$(cd "$(dirname "$0")" && pwd)
DEST=/opt/oracle-mcp
ENV_FILE=/etc/oracle-mcp.env
UNIT=/etc/systemd/system/oracle-mcp.service
FRPC_CONFIG=/etc/frp/frpc.toml
PY=/usr/bin/python3

[ "$(id -u)" = 0 ] || { echo "run as root (sudo bash $0)" >&2; exit 1; }

echo "== dependencies"
if ! "$PY" -c 'import ensurepip' 2>/dev/null; then
  apt-get install -y python3-venv
fi
install -d -m 755 "$DEST"
install -m 644 "$SRC/server.py" "$SRC/requirements.txt" "$SRC/pyproject.toml" "$SRC/uv.lock" "$DEST/"
[ -x "$DEST/.venv/bin/python" ] || "$PY" -m venv "$DEST/.venv"
"$DEST/.venv/bin/pip" install --quiet --disable-pip-version-check --require-hashes --only-binary=:all: \
  -r "$DEST/requirements.txt"
"$DEST/.venv/bin/python" -c 'import mcp, PIL, uvicorn'

echo "== tokens"
new_tokens=0
if [ ! -f "$ENV_FILE" ]; then
  (
    umask 077
    printf 'ORACLE_MCP_TOKEN_READ=%s\nORACLE_MCP_TOKEN_OPS=%s\n' "$(openssl rand -hex 32)" "$(openssl rand -hex 32)" > "$ENV_FILE"
  )
  new_tokens=1
fi
chmod 600 "$ENV_FILE"
chown root:root "$ENV_FILE"

echo "== service"
install -d -m 700 /var/backups/oracle-mcp
install -m 644 "$SRC/oracle-mcp.service" "$UNIT"
systemctl daemon-reload
systemctl enable --quiet oracle-mcp
systemctl restart oracle-mcp
for _ in $(seq 30); do
  curl -fsS http://127.0.0.1:9972/healthz >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS http://127.0.0.1:9972/healthz && echo

echo "== frpc"
if grep -q '^name = "oracle-mcp"' "$FRPC_CONFIG"; then
  echo "proxy oracle-mcp already in $FRPC_CONFIG"
else
  frpc_bin=$(systemctl show -p ExecStart --value frpc | sed -n 's/.*path=\([^ ;]*\).*/\1/p')
  [ -n "$frpc_bin" ] || frpc_bin=$(command -v frpc)
  backup="$FRPC_CONFIG.bak-before-oracle-mcp"
  cp -a "$FRPC_CONFIG" "$backup"
  { echo; cat "$SRC/frpc-proxy.toml"; } >> "$FRPC_CONFIG"
  if ! "$frpc_bin" verify -c "$FRPC_CONFIG"; then
    cp -a "$backup" "$FRPC_CONFIG"
    echo "frpc verify failed; restored $FRPC_CONFIG from $backup" >&2
    exit 1
  fi
  systemctl restart frpc
  sleep 3
  systemctl is-active frpc
fi

if [ "$new_tokens" = 1 ]; then
  cat <<NOTE

New tokens were written to $ENV_FILE. Register the MCP servers oracle
(/mcp/read, ORACLE_MCP_TOKEN_READ) and oracle_ops (/mcp/ops,
ORACLE_MCP_TOKEN_OPS) in the LiteLLM UI with these as bearer tokens (see
README.md). Show them with:  sudo cat $ENV_FILE
NOTE
fi
echo "done"

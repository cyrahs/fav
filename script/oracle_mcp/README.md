# oracle-mcp

MCP server for operating the oracle host (Oracle Cloud ARM VM, hostname
`redroid-1`) through the LiteLLM MCP gateway, the way `k8s` / `k8s_ops`
(`apps/mcp/k8s/README.md`) do for the cluster. It replaces the hermes-oracle
A2A agent. oracle is not a cluster node, so this runs on the host as a systemd
service; Flux does not apply anything in this directory.

```
Claude ──https──> llm.s117.me/mcp/oracle | /mcp/oracle_ops   (CF Access + LiteLLM key)
  ──> LiteLLM ──Bearer <tier token>──> Service proxy/oracle-mcp (80 -> frps 9972)
  ──frp──> oracle frpc ──> 127.0.0.1:9972 oracle-mcp (root)
                             ├─ /mcp/read  -> read-only tools
                             └─ /mcp/ops   -> read-only tools + ops tools
```

| LiteLLM server | Endpoint | Token | Can do |
|---|---|---|---|
| `oracle` | `/mcp/read` | `ORACLE_MCP_TOKEN_READ` | Host and service status, journals, frpc tunnels, redroid state, logcat, screenshots, favinbox GETs (`/wx/status`, `/wx/ui/dump`, events, history, ...), file reads under fixed directories and four unit files, directory listings |
| `oracle_ops` | `/mcp/ops` | `ORACLE_MCP_TOKEN_OPS` | All of the above, plus service start/stop/restart, favinbox POSTs (`/wx/ui/open`, `/wx/ui/close`, `/link/open`, `/post/page`, `/finder/video`, `/ack/*`), adb input, WeChat restart, FavInbox deploys from cyrahs/fav at a commit, file writes, and a root shell |

## Security

- Any pod in the cluster can reach frps ports (the `proxy` namespace has no
  NetworkPolicy), so unlike the k8s servers this one checks a bearer token on
  each endpoint itself. LiteLLM sends it (`auth_type: bearer_token` in
  `apps/litellm/configmap.yaml`); callers authenticate at LiteLLM with their
  key, which needs the server granted under MCP Servers in the LiteLLM UI.
  Grant `oracle_ops` only to trusted keys: it is root on oracle and controls the
  WeChat account.
- The server runs as root, so the read-only tier is enforced in `server.py`
  (fixed commands, path and route allowlists, no shell), not by the OS.
- Outputs are redacted: both tokens, favinbox's token, the plugin bridge token,
  frpc's `auth.token`, and `Bearer ...` / `token = ...` / `aes_key` patterns
  become `***`. `read_file` refuses `*.env` and token files.
- favinbox routes that act towards other people (send, Moments, revoke, friend
  requests) are not exposed at all.
- Every call is logged: `journalctl -u oracle-mcp` (tier, tool, arguments,
  outcome, duration; `write_file` contents only as length and hash).
- Files replaced by `write_file` and `deploy_from_github` are copied to
  `/var/backups/oracle-mcp/<UTC time>/<original path>` first.
- Off switch: `systemctl stop oracle-mcp` on the host, or remove the key's grant
  in LiteLLM.

## Install (once, by hand)

From a nas-gitops checkout:

```sh
scp -r tools/oracle-mcp <oracle>:/tmp/oracle-mcp
ssh -t <oracle> sudo bash /tmp/oracle-mcp/install.sh
```

`install.sh` builds `/opt/oracle-mcp/.venv` with the system Python from the
hashed `requirements.txt`, writes `/etc/oracle-mcp.env` with two new tokens if
it is missing, installs and starts the unit, then adds the `oracle-mcp` proxy
to `/etc/frp/frpc.toml` and restarts frpc. Then:

1. Add `ORACLE_MCP_TOKEN_READ` and `ORACLE_MCP_TOKEN_OPS` (the values from
   `sudo cat /etc/oracle-mcp.env` on oracle) to LiteLLM's SOPS secret in
   `apps/litellm/secrets`. LiteLLM reads them at startup, so this has to be in
   place before the `apps/litellm/configmap.yaml` entries reach the cluster.
2. In the LiteLLM UI, grant the key `oracle` and `oracle_ops` (Keys > key > MCP
   Servers).

## Update

Copy the directory and run `install.sh` again, or from a session with
`oracle_ops`: `write_file` the new `/opt/oracle-mcp/server.py` and
`service_action("oracle-mcp", "restart")`. New dependencies need `install.sh`.

## Develop

```sh
uv sync
uv run pytest
uv export --format requirements-txt --no-dev --no-emit-project --no-header -o requirements.txt
```

Regenerate `requirements.txt` after changing `pyproject.toml` / `uv.lock`. The
host is aarch64 with Python 3.12; every pinned package has a wheel for it.

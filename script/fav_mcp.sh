#!/usr/bin/env bash
# Call one tool of fav's MCP server through the LiteLLM gateway, for sessions
# where the server from .mcp.json is not loaded as native tools. The gateway's
# credentials must be supplied by the environment (the cloud session's proxy
# injects them); this script sends none.
#
#   script/fav_mcp.sh list                         # tool names
#   script/fav_mcp.sh get_runtime
#   script/fav_mcp.sh list_runs '{"job": "wechat", "limit": 5}'
set -euo pipefail

url="${FAV_MCP_URL:-https://llm.s117.me/mcp/fav}"
headers=(-H 'content-type: application/json' -H 'accept: application/json, text/event-stream')
header_file="$(mktemp)"
trap 'rm -f "$header_file"' EXIT

curl -sS --fail --max-time 30 -D "$header_file" -o /dev/null -X POST "${headers[@]}" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fav_mcp.sh","version":"1"}}}' \
  "$url"
session="$(grep -i '^mcp-session-id:' "$header_file" | awk '{print $2}' | tr -d '\r' || true)"
[[ -n "$session" ]] && headers+=(-H "mcp-session-id: $session")

if [[ "${1:-list}" == list ]]; then
  body='{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
else
  # The gateway prefixes tool names with the server name.
  body="{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"fav-$1\",\"arguments\":${2:-{\}}}}"
fi
# Answers arrive as server-sent events; print the JSON payloads.
curl -sS --fail --max-time 180 -X POST "${headers[@]}" -d "$body" "$url" | sed -n 's/^data: //p'

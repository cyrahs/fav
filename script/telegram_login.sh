#!/usr/bin/env sh
set -eu

usage() {
    cat <<'EOF'
Usage:
  script/telegram_login.sh

Examples:
  script/telegram_login.sh
  TG_API_ID=123 TG_SESSION_PATH=./data/telethon-session-second script/telegram_login.sh
  TG_SESSION_OVERWRITE=yes script/telegram_login.sh

This starts Telethon's interactive login flow from manually entered API
credentials and session path. It does not read stored settings.

Prompts:
  api_id       Telegram application API ID from my.telegram.org
  api_hash     Telegram application API hash from my.telegram.org
  session_path Local Telethon session path, without requiring .session suffix

If the session file already exists, the script exits unless you confirm with
"yes" or set TG_SESSION_OVERWRITE=yes.
EOF
}

case "${1:-}" in
    -h | --help)
        usage
        exit 0
        ;;
esac

SCRIPT_DIR="$(CDPATH= cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(CDPATH= cd "$SCRIPT_DIR/.." && pwd)"

cd "$REPO_ROOT"

if command -v uv >/dev/null 2>&1; then
    UV_BIN="$(command -v uv)"
elif [ -x "$HOME/.local/bin/uv" ]; then
    UV_BIN="$HOME/.local/bin/uv"
else
    echo "uv not found. Install uv or set PATH so uv is available." >&2
    exit 1
fi

TG_API_ID="${TG_API_ID:-}" TG_SESSION_PATH="${TG_SESSION_PATH:-}" TG_SESSION_OVERWRITE="${TG_SESSION_OVERWRITE:-}" "$UV_BIN" run python -c '
import asyncio
import getpass
import os
from pathlib import Path

from telethon import TelegramClient


def session_file_hint(session_path: Path) -> Path:
    if session_path.suffix == ".session":
        return session_path
    return Path(f"{session_path}.session")


def prompt(label: str, *, default: str = "") -> str:
    suffix = f" [{default}]" if default else ""
    value = input(f"{label}{suffix}: ").strip()
    return value or default


def confirm_existing_session(session_file: Path) -> None:
    if not session_file.exists() or os.environ.get("TG_SESSION_OVERWRITE") == "yes":
        return

    print(f"Session file already exists: {session_file}")
    print("Reusing this path may overwrite or modify an existing Telegram login session.")
    confirm = input("Type \"yes\" to continue: ").strip()
    if confirm != "yes":
        raise SystemExit("Cancelled.")


async def main() -> None:
    api_id_raw = os.environ.get("TG_API_ID", "").strip() or prompt("api_id")
    session_path_raw = os.environ.get("TG_SESSION_PATH", "").strip() or prompt("session_path", default="./data/telethon-session")
    session_path = Path(session_path_raw)

    if not api_id_raw:
        raise SystemExit("api_id cannot be empty")
    if not str(session_path):
        raise SystemExit("session_path cannot be empty")

    try:
        api_id = int(api_id_raw)
    except ValueError as exc:
        raise SystemExit("api_id must be an integer") from exc

    confirm_existing_session(session_file_hint(session_path))
    api_hash = getpass.getpass("api_hash: ").strip()
    if not api_hash:
        raise SystemExit("api_hash cannot be empty")

    session_path.parent.mkdir(parents=True, exist_ok=True)
    print(f"Session file: {session_file_hint(session_path)}")
    print("Starting Telegram login. Follow the prompts below.")

    client = TelegramClient(session_path, api_id, api_hash)
    try:
        await client.start()
        me = await client.get_me()
        username = getattr(me, "username", None) or "<none>"
        print(f"Logged in: telegram_id={me.id}, username={username}")
    finally:
        await client.disconnect()


asyncio.run(main())
'

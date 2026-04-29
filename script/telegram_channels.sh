#!/usr/bin/env sh
set -eu

usage() {
    cat <<'EOF'
Usage:
  script/telegram_channels.sh [account_name]
  script/telegram_channels.sh [account_name] --resolve TARGET
  script/telegram_channels.sh --all
  script/telegram_channels.sh --accounts

Examples:
  script/telegram_channels.sh
  script/telegram_channels.sh cyrah
  script/telegram_channels.sh cyrah --resolve https://t.me/example_channel
  script/telegram_channels.sh --all

This reads Telegram accounts from config.toml and uses their configured
session_path files. It does not start an interactive login flow.
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

"$UV_BIN" run python - "$@" <<'PY'
import argparse
import asyncio
from pathlib import Path
from typing import Any

from telethon import TelegramClient
from telethon.tl.types import Channel, PeerChannel


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description='List or resolve Telegram channel IDs from configured accounts.')
    parser.add_argument('account_name', nargs='?', help='Configured Telegram account name. Defaults to "default", or the first account.')
    parser.add_argument('--all', action='store_true', help='List channels for all configured accounts.')
    parser.add_argument('--accounts', action='store_true', help='List configured account names and exit.')
    parser.add_argument('--resolve', metavar='TARGET', help='Resolve a public username, t.me URL, or channel ID with the selected account.')
    return parser.parse_args()


def load_accounts() -> list[Any]:
    try:
        from src.core.config import config

        return config.web.telegram.resolved_accounts()
    except Exception as exc:
        msg = f'Failed to load config.toml: {exc.__class__.__name__}: {exc}'
        raise SystemExit(msg) from exc


def session_file_hint(session_path: Path) -> Path:
    if session_path.suffix == '.session':
        return session_path
    return Path(f'{session_path}.session')


def clean_target(raw: str) -> str | PeerChannel:
    target = raw.strip()
    for prefix in ('https://t.me/', 'http://t.me/', 't.me/'):
        if target.startswith(prefix):
            target = target.removeprefix(prefix)
            break
    target = target.strip().strip('/')

    if target.startswith('-100') and target[4:].isdigit():
        return PeerChannel(int(target[4:]))
    if target.isdigit():
        return PeerChannel(int(target))
    return target.removeprefix('@')


def entity_type(entity: object) -> str:
    if getattr(entity, 'broadcast', False):
        return 'channel'
    if getattr(entity, 'megagroup', False):
        return 'megagroup'
    return entity.__class__.__name__


def title_for(entity: object, fallback: str = '') -> str:
    return str(getattr(entity, 'title', None) or getattr(entity, 'first_name', None) or fallback)


def username_for(entity: object) -> str:
    username = getattr(entity, 'username', None)
    return f'@{username}' if username else ''


async def connect_authorized(account: Any) -> TelegramClient:
    client = TelegramClient(account.session_path, account.api_id, account.api_hash)
    await client.connect()
    if await client.is_user_authorized():
        return client

    await client.disconnect()
    session_file = session_file_hint(account.session_path)
    msg = (
        f'Session for account {account.name!r} is not authorized: {session_file}. '
        'Run script/telegram_login.sh first.'
    )
    raise SystemExit(msg)


async def list_channels(account: Any) -> None:
    client = await connect_authorized(account)
    try:
        print(f'# account={account.name}')
        print('id\ttype\ttitle\tusername')
        async for dialog in client.iter_dialogs():
            entity = dialog.entity
            if not isinstance(entity, Channel):
                continue
            if not (getattr(entity, 'broadcast', False) or getattr(entity, 'megagroup', False)):
                continue
            print(f'{entity.id}\t{entity_type(entity)}\t{dialog.name}\t{username_for(entity)}')
    finally:
        await client.disconnect()


async def resolve_target(account: Any, target: str) -> None:
    client = await connect_authorized(account)
    try:
        entity = await client.get_entity(clean_target(target))
        print('account\tid\ttype\ttitle\tusername')
        print(f'{account.name}\t{entity.id}\t{entity_type(entity)}\t{title_for(entity, target)}\t{username_for(entity)}')
    finally:
        await client.disconnect()


async def main() -> None:
    args = parse_args()
    accounts = load_accounts()
    account_by_name = {account.name: account for account in accounts}

    if args.accounts:
        for account in accounts:
            print(account.name)
        return

    if args.resolve and args.all:
        raise SystemExit('--resolve cannot be combined with --all')

    if args.all:
        selected_accounts = accounts
    else:
        account_name = args.account_name or ('default' if 'default' in account_by_name else accounts[0].name)
        account = account_by_name.get(account_name)
        if account is None:
            valid = ', '.join(sorted(account_by_name))
            raise SystemExit(f'Unknown telegram account: {account_name}. Valid accounts: {valid}')
        selected_accounts = [account]

    if args.resolve:
        await resolve_target(selected_accounts[0], args.resolve)
        return

    for index, account in enumerate(selected_accounts):
        if index:
            print()
        await list_channels(account)


asyncio.run(main())
PY

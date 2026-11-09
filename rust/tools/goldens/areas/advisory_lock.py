"""`src/tool/database.py::advisory_lock_id`: lock ids shared with a running Python worker (WP1)."""

from __future__ import annotations

from typing import Any

from src.tool.database import advisory_lock_id

NAMES: list[str] = [
    'bd2',
    'nikke',
    'azurlane',
    'telegram-session:/app/data/telegram/main.session',
    'rednote-profile:/app/data/rednote/profile',
    'arbitrary name',
    '',
    'é中😀',
    'x' * 300,
]


def _ids() -> list[dict[str, Any]]:
    return [{'name': name, 'id': advisory_lock_id(name)} for name in NAMES]


GOLDENS = {'db/advisory_lock_id.json': _ids}

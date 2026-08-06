"""Helpers for detecting newly archived characters and their cosmetic items."""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from pathlib import Path


@dataclass(frozen=True, slots=True)
class CharacterSnapshot:
    """Previously archived character data, including invalid snapshot state."""

    exists: bool
    character: dict[str, Any] | None


def has_character_snapshot(base_dir: Path) -> bool:
    """Return whether an archive already contains at least one character snapshot."""
    return next(base_dir.glob('*/character.json'), None) is not None


def load_character_snapshot(*, base_dir: Path, current_root: Path, content_id: int) -> CharacterSnapshot:
    """Load the prior snapshot, including one stored under an earlier title directory."""
    current_path = current_root / 'character.json'
    candidates = [current_path, *sorted(base_dir.glob(f'{content_id} - */character.json'))]
    visited: set[Path] = set()
    for path in candidates:
        if path in visited or not path.is_file():
            continue
        visited.add(path)
        try:
            parsed = json.loads(path.read_text(encoding='utf-8'))
        except (OSError, json.JSONDecodeError, UnicodeDecodeError):
            return CharacterSnapshot(exists=True, character=None)
        return CharacterSnapshot(exists=True, character=parsed if isinstance(parsed, dict) else None)
    return CharacterSnapshot(exists=False, character=None)


def collection_item_names(
    items: Any,
    *,
    identity_fields: tuple[str, ...],
    display_fields: tuple[str, ...],
    fallback_label: str,
) -> dict[str, str]:
    """Map stable cosmetic identities to display names while preserving source order."""
    if not isinstance(items, list):
        return {}

    named_items: dict[str, str] = {}
    for index, item in enumerate(items):
        if not isinstance(item, dict):
            continue
        identity = next((_normalized_text(item.get(field)) for field in identity_fields if _normalized_text(item.get(field))), '')
        display_name = next((_display_text(item.get(field)) for field in display_fields if _display_text(item.get(field))), '')
        key = identity or f'index:{index}'
        named_items.setdefault(key, display_name or f'{fallback_label} {index + 1}')
    return named_items


def new_collection_item_names(previous: dict[str, str], current: dict[str, str]) -> list[str]:
    """Return display names whose stable identities were absent from the prior snapshot."""
    return [display_name for identity, display_name in current.items() if identity not in previous]


def _normalized_text(value: Any) -> str:
    return value.strip().casefold() if isinstance(value, str) else ''


def _display_text(value: Any) -> str:
    return value.strip() if isinstance(value, str) else ''

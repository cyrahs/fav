from __future__ import annotations

import hashlib
import json
import re
import threading
from typing import Any, Protocol

import psycopg
from psycopg.rows import dict_row
from psycopg.types.json import Jsonb

_MODEL_ID_RE = re.compile(r'^[A-Za-z0-9_.:-]{1,160}$')
_PROFILE_RE = re.compile(r'^[A-Za-z0-9_-]{1,64}$')
_SOURCE_VALUES = {'bd2', 'nikke'}

_CREATE_TABLE_SQL = """
CREATE TABLE IF NOT EXISTS live2d_view_overrides (
    source TEXT NOT NULL,
    content_id INTEGER NOT NULL,
    model_id TEXT NOT NULL,
    profile TEXT NOT NULL,
    position JSONB NOT NULL,
    scale DOUBLE PRECISION NOT NULL,
    background_position JSONB,
    background_scale DOUBLE PRECISION,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (source, content_id, model_id, profile),
    CONSTRAINT live2d_view_overrides_source_check CHECK (source IN ('bd2', 'nikke')),
    CONSTRAINT live2d_view_overrides_profile_check CHECK (profile ~ '^[A-Za-z0-9_-]{1,64}$'),
    CONSTRAINT live2d_view_overrides_position_check CHECK (jsonb_typeof(position) = 'object'),
    CONSTRAINT live2d_view_overrides_background_position_check CHECK (
        background_position IS NULL OR jsonb_typeof(background_position) = 'object'
    ),
    CONSTRAINT live2d_view_overrides_scale_check CHECK (scale > 0),
    CONSTRAINT live2d_view_overrides_background_scale_check CHECK (background_scale IS NULL OR background_scale > 0)
);
"""


class Live2DViewOverrideStore(Protocol):
    def list_for_character(self, *, source: str, content_id: int) -> list[dict[str, Any]]:
        pass

    def get(self, *, source: str, content_id: int, model_id: str, profile: str) -> dict[str, Any] | None:
        pass

    def upsert(  # noqa: PLR0913
        self,
        *,
        source: str,
        content_id: int,
        model_id: str,
        profile: str,
        position: dict[str, float],
        scale: float,
        background_position: dict[str, float] | None,
        background_scale: float | None,
    ) -> dict[str, Any]:
        pass

    def delete(self, *, source: str, content_id: int, model_id: str, profile: str) -> bool:
        pass


class PostgresLive2DViewOverrideStore:
    def __init__(self, dsn: str) -> None:
        self._dsn = dsn
        self._schema_ready = False
        self._schema_lock = threading.Lock()

    def list_for_character(self, *, source: str, content_id: int) -> list[dict[str, Any]]:
        self._ensure_schema()
        with psycopg.connect(self._dsn, row_factory=dict_row) as conn, conn.cursor() as cursor:
            cursor.execute(
                """
                SELECT source, content_id, model_id, profile, position, scale, background_position, background_scale, created_at, updated_at
                FROM live2d_view_overrides
                WHERE source = %s AND content_id = %s
                ORDER BY model_id, profile;
                """,
                (source, content_id),
            )
            return [dict(row) for row in cursor.fetchall()]

    def get(self, *, source: str, content_id: int, model_id: str, profile: str) -> dict[str, Any] | None:
        self._ensure_schema()
        with psycopg.connect(self._dsn, row_factory=dict_row) as conn, conn.cursor() as cursor:
            cursor.execute(
                """
                SELECT source, content_id, model_id, profile, position, scale, background_position, background_scale, created_at, updated_at
                FROM live2d_view_overrides
                WHERE source = %s AND content_id = %s AND model_id = %s AND profile = %s;
                """,
                (source, content_id, model_id, profile),
            )
            row = cursor.fetchone()
            return dict(row) if row is not None else None

    def upsert(  # noqa: PLR0913
        self,
        *,
        source: str,
        content_id: int,
        model_id: str,
        profile: str,
        position: dict[str, float],
        scale: float,
        background_position: dict[str, float] | None,
        background_scale: float | None,
    ) -> dict[str, Any]:
        self._ensure_schema()
        with psycopg.connect(self._dsn, row_factory=dict_row) as conn, conn.cursor() as cursor:
            cursor.execute(
                """
                INSERT INTO live2d_view_overrides (
                    source, content_id, model_id, profile, position, scale, background_position, background_scale
                )
                VALUES (%s, %s, %s, %s, %s, %s, %s, %s)
                ON CONFLICT (source, content_id, model_id, profile) DO UPDATE SET
                    position = EXCLUDED.position,
                    scale = EXCLUDED.scale,
                    background_position = EXCLUDED.background_position,
                    background_scale = EXCLUDED.background_scale,
                    updated_at = NOW()
                RETURNING
                    source, content_id, model_id, profile, position, scale, background_position, background_scale, created_at, updated_at;
                """,
                (
                    source,
                    content_id,
                    model_id,
                    profile,
                    Jsonb(position),
                    scale,
                    Jsonb(background_position) if background_position is not None else None,
                    background_scale,
                ),
            )
            row = cursor.fetchone()
            conn.commit()
            if row is None:
                msg = 'Live2D view override upsert did not return a row'
                raise RuntimeError(msg)
            return dict(row)

    def delete(self, *, source: str, content_id: int, model_id: str, profile: str) -> bool:
        self._ensure_schema()
        with psycopg.connect(self._dsn) as conn, conn.cursor() as cursor:
            cursor.execute(
                """
                DELETE FROM live2d_view_overrides
                WHERE source = %s AND content_id = %s AND model_id = %s AND profile = %s;
                """,
                (source, content_id, model_id, profile),
            )
            deleted = cursor.rowcount > 0
            conn.commit()
            return deleted

    def _ensure_schema(self) -> None:
        if self._schema_ready:
            return
        with self._schema_lock:
            if self._schema_ready:
                return
            with psycopg.connect(self._dsn) as conn, conn.cursor() as cursor:
                cursor.execute(_CREATE_TABLE_SQL)
                conn.commit()
            self._schema_ready = True


def validate_live2d_source(source: str) -> str:
    normalized = source.strip().lower()
    if normalized not in _SOURCE_VALUES:
        msg = f'Unsupported Live2D source: {source}'
        raise ValueError(msg)
    return normalized


def validate_live2d_profile(profile: str) -> str:
    normalized = profile.strip()
    if not _PROFILE_RE.fullmatch(normalized):
        msg = 'Live2D override profile must contain only ASCII letters, digits, underscores, or hyphens'
        raise ValueError(msg)
    return normalized


def live2d_model_id(model: dict[str, Any]) -> str:
    for key in ('stable_id', 'live2d_key'):
        value = _clean_text(model.get(key))
        if not value:
            continue
        if _MODEL_ID_RE.fullmatch(value):
            return value
        return f'{key}_{_short_hash(value)}'

    return f'generated_{_short_hash(_live2d_model_fingerprint(model))}'


def iter_live2d_models(character: dict[str, Any]) -> list[dict[str, Any]]:
    models: list[dict[str, Any]] = []
    models.extend(_dict_items(character.get('live2d_models')))
    for costume in _dict_items(character.get('costumes')):
        models.extend(_dict_items(costume.get('live2d_models')))
    for skin in _dict_items(character.get('skins')):
        models.extend(_dict_items(skin.get('live2d_models')))
    return models


def assign_live2d_model_ids(character: dict[str, Any]) -> None:
    model_id_by_fingerprint: dict[str, str] = {}
    used_model_ids: set[str] = set()
    for model in iter_live2d_models(character):
        fingerprint = _live2d_model_fingerprint(model)
        model_id = model_id_by_fingerprint.get(fingerprint)
        if model_id is None:
            base_model_id = live2d_model_id(model)
            model_id = base_model_id if base_model_id not in used_model_ids else _deduped_model_id(base_model_id, fingerprint)
            model_id_by_fingerprint[fingerprint] = model_id
            used_model_ids.add(model_id)
        model['model_id'] = model_id


def apply_live2d_view_overrides(character: dict[str, Any], overrides: list[dict[str, Any]]) -> None:
    assign_live2d_model_ids(character)
    grouped: dict[str, dict[str, dict[str, Any]]] = {}
    for override in overrides:
        model_id = _clean_text(override.get('model_id'))
        profile = _clean_text(override.get('profile'))
        if not model_id or not profile:
            continue
        grouped.setdefault(model_id, {})[profile] = live2d_view_override_value(override)

    for model in iter_live2d_models(character):
        model['view_overrides'] = grouped.get(_clean_text(model.get('model_id')), {})


def live2d_view_override_value(override: dict[str, Any]) -> dict[str, Any]:
    return {
        'position': override.get('position') if isinstance(override.get('position'), dict) else {},
        'scale': override.get('scale'),
        'background_position': override.get('background_position') if isinstance(override.get('background_position'), dict) else None,
        'background_scale': override.get('background_scale'),
        'created_at': override.get('created_at'),
        'updated_at': override.get('updated_at'),
    }


def _dict_items(value: Any) -> list[dict[str, Any]]:
    if not isinstance(value, list):
        return []
    return [item for item in value if isinstance(item, dict)]


def _short_hash(value: str) -> str:
    return hashlib.sha256(value.encode('utf-8')).hexdigest()[:16]


def _deduped_model_id(base_model_id: str, fingerprint: str) -> str:
    suffix = _short_hash(fingerprint)[:8]
    prefix = base_model_id[:151]
    return f'{prefix}_{suffix}'


def _live2d_model_fingerprint(model: dict[str, Any]) -> str:
    basis = {
        'stable_id': _clean_text(model.get('stable_id')),
        'live2d_key': _clean_text(model.get('live2d_key')),
        'key': _clean_text(model.get('key')),
        'field': _clean_text(model.get('field')),
        'label': _clean_text(model.get('label')),
        'source': _clean_text(model.get('source')),
        'variant': _clean_text(model.get('variant')),
        'row_index': model.get('row_index'),
        'column_index': model.get('column_index'),
        'style_index': model.get('style_index'),
        'skin_index': model.get('skin_index'),
        'source_urls': model.get('source_urls') if isinstance(model.get('source_urls'), dict) else {},
    }
    return json.dumps(basis, ensure_ascii=False, sort_keys=True, separators=(',', ':'))


def _clean_text(value: Any) -> str:
    if value is None:
        return ''
    return str(value).strip()

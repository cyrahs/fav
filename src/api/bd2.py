from __future__ import annotations

import json
import threading
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path, PurePosixPath
from typing import Any
from urllib.parse import quote, unquote

from src.core import logger

from .summary_cache import ManifestEntry, ManifestSignature, SummaryCacheData, manifest_signature, read_summary_cache, write_summary_cache

log = logger.get('fav-api.bd2')

_LONG_CACHE_CONTROL = 'public, max-age=31536000, immutable'
_SHORT_CACHE_CONTROL = 'public, max-age=3600'
_BD2_STATIC_PREFIX = '/static/bd2'


class BD2LibraryError(RuntimeError):
    pass


class BD2CharacterNotFoundError(BD2LibraryError):
    pass


class BD2AssetNotFoundError(BD2LibraryError):
    pass


@dataclass(frozen=True, slots=True)
class BD2AssetFile:
    path: Path
    content_type: str
    headers: dict[str, str]


@dataclass(frozen=True, slots=True)
class _BD2Record:
    content_id: int
    root: Path
    manifest_path: Path
    directory_name: str
    manifest: dict[str, Any]


@dataclass(frozen=True, slots=True)
class _BD2RecordEntry:
    content_id: int
    root: Path
    manifest_path: Path
    directory_name: str


def _read_json_object(path: Path) -> dict[str, Any]:
    data = json.loads(path.read_text(encoding='utf-8'))
    if not isinstance(data, dict):
        msg = f'JSON document is not an object: {path}'
        raise TypeError(msg)
    return data


def _to_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return None


def _clean_text(value: Any) -> str:
    if value is None:
        return ''
    return str(value).replace('\r\n', '\n').strip()


def _context_value(context: dict[str, Any], key: str) -> str:
    return _clean_text(context.get(key))


def _iter_assets(manifest: dict[str, Any]) -> list[dict[str, Any]]:
    assets = manifest.get('assets')
    if not isinstance(assets, list):
        return []
    return [asset for asset in assets if isinstance(asset, dict)]


def _iter_contexts(asset: dict[str, Any]) -> list[dict[str, Any]]:
    contexts = asset.get('contexts')
    if not isinstance(contexts, list):
        return []
    return [context for context in contexts if isinstance(context, dict)]


def _manifest_content_id(manifest_path: Path, manifest: dict[str, Any]) -> int | None:
    content_id = _to_int(manifest.get('content_id'))
    if content_id is not None:
        return content_id

    prefix = manifest_path.parent.name.split(' - ', 1)[0].strip()
    return _to_int(prefix)


def _safe_manifest_title(record: _BD2Record) -> str:
    title = _clean_text(record.manifest.get('title'))
    if title:
        return title
    return record.directory_name.split(' - ', 1)[-1].strip() or str(record.content_id)


def _first_text_value(values: Any) -> str:
    if not isinstance(values, list):
        return ''
    for cell in values:
        if not isinstance(cell, dict) or cell.get('type') != 'text':
            continue
        text = _clean_text(cell.get('value'))
        if text:
            return text
    return ''


def _base_info_text(base_info: dict[str, Any], label: str) -> str:
    rows = base_info.get(label)
    if not isinstance(rows, list):
        return ''
    for row in rows:
        if not isinstance(row, dict):
            continue
        text = _first_text_value(row.get('values'))
        if text:
            return text
    return ''


def _profile_key(label: str) -> str:
    key = ''.join(char.lower() if char.isalnum() else '_' for char in label.strip())
    key = '_'.join(part for part in key.split('_') if part)
    return key or 'field'


def _sort_timestamp(value: Any) -> float:
    result = 0.0
    if isinstance(value, bool) or value is None:
        return result
    if isinstance(value, (int, float)):
        result = float(value)
    elif isinstance(value, str):
        text = value.strip()
        if text.isdigit():
            result = float(text)
        elif text:
            try:
                normalized = f'{text.removesuffix("Z")}+00:00' if text.endswith('Z') else text
                parsed = datetime.fromisoformat(normalized)
            except ValueError:
                result = 0.0
            else:
                result = parsed.replace(tzinfo=UTC).timestamp() if parsed.tzinfo is None else parsed.timestamp()
    return result


def _record_freshness_key(manifest: dict[str, Any], *, mtime_ns: int) -> tuple[float, float, int]:
    return (
        _sort_timestamp(manifest.get('fetched_at')),
        _sort_timestamp(manifest.get('updated_at')),
        mtime_ns,
    )


def _costume_search_terms(costumes: list[Any]) -> list[str]:
    terms: list[str] = []
    for costume in costumes:
        if not isinstance(costume, dict):
            continue
        for key in ('style_name', 'title', 'category'):
            value = _clean_text(costume.get(key))
            if value:
                terms.append(value)
        rows = costume.get('rows')
        if isinstance(rows, list):
            for row in rows:
                if not isinstance(row, dict):
                    continue
                label = _clean_text(row.get('label'))
                if label:
                    terms.append(label)
    return sorted(set(terms), key=str.casefold)


class BD2Library:
    def __init__(self, root: Path) -> None:
        self._root = Path(root)
        self._signature: ManifestSignature | None = None
        self._record_entries: dict[int, _BD2RecordEntry] = {}
        self._summaries: list[dict[str, Any]] = []
        self._cache_lock = threading.Lock()

    def list_characters(self) -> list[dict[str, Any]]:
        self._ensure_summary_cache()
        return list(self._summaries)

    def get_character(self, content_id: int) -> dict[str, Any]:
        record = self._get_record(content_id)
        character = self._read_character_json(record)
        costumes = character.get('costumes')
        if not isinstance(costumes, list):
            content_summary = record.manifest.get('content_summary')
            costumes = content_summary.get('costumes', []) if isinstance(content_summary, dict) else record.manifest.get('costumes', [])
        if not isinstance(costumes, list):
            costumes = []

        live2d_models = character.get('live2d_models')
        if not isinstance(live2d_models, list):
            live2d_models = record.manifest.get('live2d_models', [])
        if not isinstance(live2d_models, list):
            live2d_models = []

        payload = self._summary_payload(record)
        payload.update(
            {
                'tree_row': character.get('tree_row') if isinstance(character.get('tree_row'), dict) else record.manifest.get('tree_row'),
                'base_info': self._base_info(record, character),
                'costumes': [self._costume_payload(record, costume, live2d_models) for costume in costumes if isinstance(costume, dict)],
                'live2d_models': [self._live2d_model_payload(record, model) for model in live2d_models if isinstance(model, dict)],
                'assets': [self._asset_ref(record, asset) for asset in _iter_assets(record.manifest)],
            },
        )
        return payload

    def get_asset_file(self, content_id: int, asset_path: str) -> BD2AssetFile:
        record = self._get_record(content_id)
        normalized_path = self._normalize_asset_path(asset_path)
        asset = next((item for item in _iter_assets(record.manifest) if item.get('local_path') == normalized_path), None)
        if asset is None:
            msg = f'BD2 asset not found: {content_id}/{normalized_path}'
            raise BD2AssetNotFoundError(msg)

        target = (record.root / normalized_path).resolve()
        record_root = record.root.resolve()
        assets_root = (record.root / 'assets').resolve()
        if not assets_root.is_relative_to(record_root) or not target.is_relative_to(assets_root) or not target.is_file():
            msg = f'BD2 asset file not found: {content_id}/{normalized_path}'
            raise BD2AssetNotFoundError(msg)

        sha256 = _clean_text(asset.get('sha256'))
        headers = {
            'Cache-Control': _LONG_CACHE_CONTROL if sha256 else _SHORT_CACHE_CONTROL,
            'X-Content-Type-Options': 'nosniff',
        }
        if sha256:
            headers['ETag'] = f'"{sha256}"'
        return BD2AssetFile(path=target, content_type=_clean_text(asset.get('content_type')), headers=headers)

    def _manifest_entries(self) -> list[ManifestEntry]:
        if not self._root.is_dir():
            return []

        entries: list[tuple[Path, int, int]] = []
        root_resolved = self._root.resolve()
        try:
            children = list(self._root.iterdir())
        except OSError:
            log.exception('Failed to list BD2 collection root: %s', self._root)
            return []

        for child in children:
            if child.name.startswith('_') or not child.is_dir():
                continue
            child_resolved = child.resolve()
            if not child_resolved.is_relative_to(root_resolved):
                log.warning('Skipping BD2 character directory outside collection root: %s', child)
                continue
            manifest_path = child / 'manifest.json'
            if not manifest_path.is_file():
                continue
            try:
                stat = manifest_path.stat()
            except OSError:
                log.warning('Failed to stat BD2 manifest: %s', manifest_path)
                continue
            entries.append((manifest_path, stat.st_mtime_ns, stat.st_size))
        return sorted(entries, key=lambda entry: entry[0].parent.name.casefold())

    def _ensure_summary_cache(self) -> None:
        entries = self._manifest_entries()
        signature = manifest_signature(entries)
        if signature == self._signature:
            return

        with self._cache_lock:
            entries = self._manifest_entries()
            signature = manifest_signature(entries)
            if signature == self._signature:
                return
            if self._load_summary_cache(signature):
                return

            record_entries, summaries = self._build_summary_cache(entries)
            self._replace_summary_cache(signature=signature, record_entries=record_entries, summaries=summaries)
            self._write_summary_cache()

    def _build_summary_cache(self, entries: list[ManifestEntry]) -> tuple[dict[int, _BD2RecordEntry], list[dict[str, Any]]]:
        record_entries: dict[int, _BD2RecordEntry] = {}
        summaries_by_content_id: dict[int, dict[str, Any]] = {}
        freshness_by_content_id: dict[int, tuple[float, float, int]] = {}
        for manifest_path, mtime_ns, _size in entries:
            try:
                manifest = _read_json_object(manifest_path)
            except (OSError, TypeError, ValueError) as exc:
                log.warning('Skipping unreadable BD2 manifest %s: %s', manifest_path, exc)
                continue

            content_id = _manifest_content_id(manifest_path, manifest)
            if content_id is None:
                log.warning('Skipping BD2 manifest without content id: %s', manifest_path)
                continue
            freshness = _record_freshness_key(manifest, mtime_ns=mtime_ns)
            existing = record_entries.get(content_id)
            if existing is not None:
                if freshness <= freshness_by_content_id[content_id]:
                    log.warning('Skipping stale duplicate BD2 manifest for content_id=%d: %s', content_id, manifest_path)
                    continue
                log.warning(
                    'Replacing stale duplicate BD2 manifest for content_id=%d: %s -> %s',
                    content_id,
                    existing.manifest_path,
                    manifest_path,
                )

            record = _BD2Record(
                content_id=content_id,
                root=manifest_path.parent,
                manifest_path=manifest_path,
                directory_name=manifest_path.parent.name,
                manifest=manifest,
            )
            record_entries[content_id] = _BD2RecordEntry(
                content_id=content_id,
                root=manifest_path.parent,
                manifest_path=manifest_path,
                directory_name=manifest_path.parent.name,
            )
            summaries_by_content_id[content_id] = self._summary_payload(record)
            freshness_by_content_id[content_id] = freshness

        summaries = sorted(
            summaries_by_content_id.values(),
            key=lambda item: (str(item.get('title') or '').casefold(), int(item.get('content_id') or 0)),
        )
        return record_entries, summaries

    def _load_summary_cache(self, signature: ManifestSignature) -> bool:
        cached = read_summary_cache(
            self._summary_cache_path,
            expected_signature=signature,
            log=log,
            label='BD2',
        )
        if cached is None:
            return False

        record_entries = self._record_entries_from_cache(cached.records, signature=signature)
        if record_entries is None:
            return False
        self._replace_summary_cache(signature=signature, record_entries=record_entries, summaries=cached.summaries)
        return True

    def _record_entries_from_cache(
        self,
        records: list[dict[str, Any]],
        *,
        signature: ManifestSignature,
    ) -> dict[int, _BD2RecordEntry] | None:
        current_paths = {path for path, _mtime_ns, _size in signature}
        record_entries: dict[int, _BD2RecordEntry] = {}
        for item in records:
            content_id = _to_int(item.get('content_id'))
            manifest_path_value = item.get('manifest_path')
            if content_id is None or not isinstance(manifest_path_value, str) or manifest_path_value not in current_paths:
                return None
            manifest_path = Path(manifest_path_value)
            record_entries[content_id] = _BD2RecordEntry(
                content_id=content_id,
                root=manifest_path.parent,
                manifest_path=manifest_path,
                directory_name=manifest_path.parent.name,
            )
        return record_entries

    def _replace_summary_cache(
        self,
        *,
        signature: ManifestSignature,
        record_entries: dict[int, _BD2RecordEntry],
        summaries: list[dict[str, Any]],
    ) -> None:
        self._signature = signature
        self._record_entries = record_entries
        self._summaries = summaries

    @property
    def _summary_cache_path(self) -> Path:
        return self._root / '_api' / 'summary-cache.json'

    def _write_summary_cache(self) -> None:
        if not self._root.is_dir():
            return
        records = [
            {'content_id': entry.content_id, 'manifest_path': entry.manifest_path.as_posix()}
            for entry in sorted(self._record_entries.values(), key=lambda item: item.content_id)
        ]
        write_summary_cache(
            self._summary_cache_path,
            data=SummaryCacheData(signature=self._signature or (), records=records, summaries=self._summaries),
            log=log,
            label='BD2',
        )

    def _get_record(self, content_id: int) -> _BD2Record:
        self._ensure_summary_cache()
        entry = self._record_entries.get(content_id)
        if entry is None:
            msg = f'BD2 character not found: {content_id}'
            raise BD2CharacterNotFoundError(msg)
        try:
            manifest = _read_json_object(entry.manifest_path)
        except (OSError, TypeError, ValueError) as exc:
            log.warning('Skipping unreadable BD2 manifest %s: %s', entry.manifest_path, exc)
            msg = f'BD2 character not found: {content_id}'
            raise BD2CharacterNotFoundError(msg) from exc
        return _BD2Record(
            content_id=entry.content_id,
            root=entry.root,
            manifest_path=entry.manifest_path,
            directory_name=entry.directory_name,
            manifest=manifest,
        )

    def _read_character_json(self, record: _BD2Record) -> dict[str, Any]:
        path = record.root / 'character.json'
        try:
            return _read_json_object(path)
        except (OSError, TypeError, ValueError) as exc:
            log.warning('Falling back to manifest data for %s: %s', record.directory_name, exc)
            return {}

    def _base_info(self, record: _BD2Record, character: dict[str, Any] | None = None) -> dict[str, Any]:
        source = character if character is not None else self._read_character_json(record)
        base_info = source.get('base_info') if isinstance(source, dict) else None
        if isinstance(base_info, dict):
            return base_info
        fallback = record.manifest.get('base_info')
        return fallback if isinstance(fallback, dict) else {}

    def _summary_payload(self, record: _BD2Record) -> dict[str, Any]:
        profile = self._profile_payload(record, {})
        tree_row = record.manifest.get('tree_row')
        tree_row = tree_row if isinstance(tree_row, dict) else {}
        tags = {
            item['key']: item['value']
            for item in profile
            if item['value'] and item['key'] in {'rarity', 'attribute', 'role', 'weapon', 'class'}
        }
        rarity_group = _clean_text(tree_row.get('_bd2_group_name'))
        if rarity_group:
            tags['rarity_group'] = rarity_group

        live2d_models = record.manifest.get('live2d_models', [])
        costumes = record.manifest.get('costumes')
        if not isinstance(costumes, list):
            content_summary = record.manifest.get('content_summary')
            costumes = content_summary.get('costumes', []) if isinstance(content_summary, dict) else []

        first_costume = next((costume for costume in costumes if isinstance(costume, dict)), {})
        return {
            'content_id': record.content_id,
            'title': _safe_manifest_title(record),
            'directory_name': record.directory_name,
            'source_url': _clean_text(record.manifest.get('source_url')),
            'updated_at': record.manifest.get('updated_at'),
            'fetched_at': _clean_text(record.manifest.get('fetched_at')) or None,
            'asset_counts': record.manifest.get('asset_counts') if isinstance(record.manifest.get('asset_counts'), dict) else {},
            'tags': tags,
            'profile': profile,
            'icon': self._pick_asset(record, field='icon') or self._pick_asset(record, field='icon_small') or self._pick_asset(record),
            'portrait': self._pick_costume_asset(record, first_costume, field='costume_portrait')
            or self._pick_costume_asset(record, first_costume, field='costume_full_portrait')
            or self._pick_costume_asset(record, first_costume, field='costume_sprite'),
            'costume_count': len(costumes) if isinstance(costumes, list) else 0,
            'live2d_model_count': len(live2d_models) if isinstance(live2d_models, list) else 0,
            'search_terms': _costume_search_terms(costumes if isinstance(costumes, list) else []),
        }

    def _profile_payload(self, record: _BD2Record, character: dict[str, Any]) -> list[dict[str, Any]]:
        base_info = self._base_info(record, character)
        profile: list[dict[str, Any]] = []
        for label in sorted((key for key in base_info if isinstance(key, str)), key=str.casefold):
            value = _base_info_text(base_info, label)
            asset = self._pick_asset(record, label=label)
            if not value and asset is None:
                continue
            profile.append({'key': _profile_key(label), 'label': label, 'value': value, 'asset': asset})
        return profile

    def _costume_payload(self, record: _BD2Record, costume: dict[str, Any], live2d_models: list[Any]) -> dict[str, Any]:
        style_index = _to_int(costume.get('style_index'))
        models = [
            self._live2d_model_payload(record, model)
            for model in live2d_models
            if isinstance(model, dict) and _to_int(model.get('style_index')) == style_index
        ]
        rows = costume.get('rows')
        rows = rows if isinstance(rows, list) else []
        return {
            'style_index': style_index,
            'style_name': _clean_text(costume.get('style_name')),
            'title': _clean_text(costume.get('title')),
            'category': _clean_text(costume.get('category')),
            'sprite': self._pick_costume_asset(record, costume, field='costume_sprite'),
            'portrait': self._pick_costume_asset(record, costume, field='costume_portrait'),
            'full_portrait': self._pick_costume_asset(record, costume, field='costume_full_portrait'),
            'gallery': self._assets_for_context(record, section='style', style_index=style_index, kind='image'),
            'videos': self._assets_for_context(record, section='style', style_index=style_index, kind='video'),
            'audio': self._assets_for_context(record, section='style', style_index=style_index, kind='audio'),
            'live2d_models': models,
            'rows': rows,
        }

    def _pick_costume_asset(self, record: _BD2Record, costume: dict[str, Any], *, field: str) -> dict[str, Any] | None:
        style_index = _to_int(costume.get('style_index'))
        title = _clean_text(costume.get('title'))
        if title:
            asset = self._pick_asset(record, field=field, style_index=style_index, column_name=title)
            if asset is not None:
                return asset
        category = _clean_text(costume.get('category'))
        if category:
            asset = self._pick_asset(record, field=field, style_index=style_index, column_category=category)
            if asset is not None:
                return asset
        return self._pick_asset(record, field=field, style_index=style_index)

    def _live2d_model_payload(self, record: _BD2Record, model: dict[str, Any]) -> dict[str, Any]:
        live2d_key = _clean_text(model.get('live2d_key'))
        textures = self._assets_for_context(record, live2d_key=live2d_key, kind='live2d_texture')
        return {
            'label': _clean_text(model.get('label')),
            'section': _clean_text(model.get('section')),
            'style_index': _to_int(model.get('style_index')),
            'style_name': _clean_text(model.get('style_name')),
            'costume_title': _clean_text(model.get('costume_title')),
            'costume_category': _clean_text(model.get('costume_category')),
            'row_index': _to_int(model.get('row_index')),
            'column_index': _to_int(model.get('column_index')),
            'field': _clean_text(model.get('field')),
            'is_art_row': bool(model.get('is_art_row')),
            'column_name': _clean_text(model.get('column_name')),
            'column_category': _clean_text(model.get('column_category')),
            'column_role': _clean_text(model.get('column_role')),
            'column_header': _clean_text(model.get('column_header')),
            'key': _clean_text(model.get('key')),
            'stable_id': _clean_text(model.get('stable_id')),
            'live2d_key': live2d_key,
            'animation': _clean_text(model.get('animation')),
            'skin': _clean_text(model.get('skin')),
            'limit_age': bool(model.get('limit_age')),
            'source': _clean_text(model.get('source')),
            'variant': _clean_text(model.get('variant')),
            'supplement_reason': _clean_text(model.get('supplement_reason')),
            'viewer_entry_id': _clean_text(model.get('viewer_entry_id')),
            'viewer_stem': _clean_text(model.get('viewer_stem')),
            'source_page_url': _clean_text(model.get('source_page_url')),
            'position': model.get('position') if isinstance(model.get('position'), dict) else {},
            'bg_position': model.get('bg_position') if isinstance(model.get('bg_position'), dict) else {},
            'source_urls': model.get('urls') if isinstance(model.get('urls'), dict) else {},
            'assets': {
                'atlas': self._pick_asset(record, kind='live2d_atlas', live2d_key=live2d_key, live2d_field='atlas'),
                'skel': self._pick_asset(record, kind='live2d_skel', live2d_key=live2d_key, live2d_field='skel'),
                'json': self._pick_asset(record, kind='live2d_json', live2d_key=live2d_key, live2d_field='json'),
                'textures': textures,
                'background': self._pick_asset(record, kind='live2d_background', live2d_key=live2d_key, live2d_field='bg'),
            },
        }

    def _pick_asset(  # noqa: PLR0913
        self,
        record: _BD2Record,
        *,
        kind: str | None = 'image',
        label: str | None = None,
        field: str | None = None,
        section: str | None = None,
        style_index: int | None = None,
        column_name: str | None = None,
        column_category: str | None = None,
        live2d_key: str | None = None,
        live2d_field: str | None = None,
    ) -> dict[str, Any] | None:
        assets = self._assets_for_context(
            record,
            kind=kind,
            label=label,
            field=field,
            section=section,
            style_index=style_index,
            column_name=column_name,
            column_category=column_category,
            live2d_key=live2d_key,
            live2d_field=live2d_field,
        )
        return assets[0] if assets else None

    def _assets_for_context(  # noqa: PLR0913
        self,
        record: _BD2Record,
        *,
        kind: str | None = None,
        label: str | None = None,
        field: str | None = None,
        section: str | None = None,
        style_index: int | None = None,
        column_name: str | None = None,
        column_category: str | None = None,
        live2d_key: str | None = None,
        live2d_field: str | None = None,
    ) -> list[dict[str, Any]]:
        found: list[tuple[tuple[int, int, str, str], dict[str, Any]]] = []
        seen_paths: set[str] = set()
        for asset in _iter_assets(record.manifest):
            if kind is not None and asset.get('kind') != kind:
                continue
            for context in _iter_contexts(asset):
                if not self._context_matches(
                    context,
                    label=label,
                    field=field,
                    section=section,
                    style_index=style_index,
                    column_name=column_name,
                    column_category=column_category,
                    live2d_key=live2d_key,
                    live2d_field=live2d_field,
                ):
                    continue
                local_path = _clean_text(asset.get('local_path'))
                if local_path in seen_paths:
                    continue
                seen_paths.add(local_path)
                sort_key = (
                    _to_int(context.get('style_index')) or 0,
                    _to_int(context.get('row_index')) or 0,
                    _context_value(context, 'label'),
                    local_path,
                )
                found.append((sort_key, self._asset_ref(record, asset, context)))
        return [item for _sort_key, item in sorted(found, key=lambda entry: entry[0])]

    @staticmethod
    def _context_matches(  # noqa: PLR0913
        context: dict[str, Any],
        *,
        label: str | None,
        field: str | None,
        section: str | None,
        style_index: int | None,
        column_name: str | None,
        column_category: str | None,
        live2d_key: str | None,
        live2d_field: str | None,
    ) -> bool:
        return all(
            (
                label is None or _context_value(context, 'label') == label,
                field is None or _context_value(context, 'field') == field,
                section is None or _context_value(context, 'section') == section,
                style_index is None or _to_int(context.get('style_index')) == style_index,
                column_name is None or _context_value(context, 'column_name') == column_name,
                column_category is None or _context_value(context, 'column_category') == column_category,
                live2d_key is None or _context_value(context, 'live2d_key') == live2d_key,
                live2d_field is None or _context_value(context, 'live2d_field') == live2d_field,
            ),
        )

    def _asset_ref(self, record: _BD2Record, asset: dict[str, Any], context: dict[str, Any] | None = None) -> dict[str, Any]:
        local_path = _clean_text(asset.get('local_path'))
        selected_context = context or {}
        available = False
        if local_path:
            try:
                normalized = self._normalize_asset_path(local_path)
            except BD2AssetNotFoundError:
                normalized = ''
            if normalized:
                available = (record.root / normalized).is_file()
        sha256 = _clean_text(asset.get('sha256'))
        return {
            'kind': _clean_text(asset.get('kind')),
            'path': local_path,
            'url': self._asset_static_url(record.directory_name, local_path, sha256=sha256) if local_path else '',
            'content_type': _clean_text(asset.get('content_type')),
            'size': _to_int(asset.get('size')) or 0,
            'sha256': sha256,
            'status': _clean_text(asset.get('status')),
            'label': _context_value(selected_context, 'label'),
            'field': _context_value(selected_context, 'field') or _context_value(selected_context, 'live2d_field'),
            'style_index': _to_int(selected_context.get('style_index')),
            'style_name': _context_value(selected_context, 'style_name'),
            'costume_title': _context_value(selected_context, 'costume_title'),
            'costume_category': _context_value(selected_context, 'costume_category'),
            'column_role': _context_value(selected_context, 'column_role'),
            'live2d_key': _context_value(selected_context, 'live2d_key'),
            'available': available,
            'contexts': _iter_contexts(asset),
        }

    @staticmethod
    def _asset_static_url(directory_name: str, asset_path: str, *, sha256: str = '') -> str:
        url = f'{_BD2_STATIC_PREFIX}/{quote(directory_name, safe="")}/{quote(asset_path, safe="/")}'
        return f'{url}?v={quote(sha256)}' if sha256 else url

    @staticmethod
    def _normalize_asset_path(asset_path: str) -> str:
        raw_path = unquote(asset_path).replace('\\', '/').strip()
        path = PurePosixPath(raw_path)
        parts = path.parts
        if not raw_path or raw_path.startswith('/') or path.is_absolute() or not parts:
            msg = f'Invalid BD2 asset path: {asset_path}'
            raise BD2AssetNotFoundError(msg)
        if parts[0] != 'assets' or any(part in {'', '.', '..'} for part in parts):
            msg = f'Invalid BD2 asset path: {asset_path}'
            raise BD2AssetNotFoundError(msg)
        return path.as_posix()

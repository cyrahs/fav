# ruff: noqa: RUF001

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any
from urllib.parse import quote, unquote

from src.core import logger

from .constants import API_V2_PREFIX

log = logger.get('fav-api.nikke')

_PROFILE_FIELDS: tuple[tuple[str, str, str], ...] = (
    ('rarity', '稀有度', 'level'),
    ('company', '企业', 'qy'),
    ('burst', '阶段', 'jd'),
    ('attribute', '属性', 'attr'),
    ('role', '职业', 'zy'),
    ('weapon', '武器', 'wq'),
    ('squad', '部队名称', 'bd'),
    ('cv', 'CV', ''),
    ('implemented_at', '实装日期', ''),
)
_SUMMARY_TAG_KEYS = {'rarity', 'company', 'burst', 'attribute', 'role', 'weapon'}
_LONG_CACHE_CONTROL = 'public, max-age=31536000, immutable'
_SHORT_CACHE_CONTROL = 'public, max-age=3600'


class NikkeLibraryError(RuntimeError):
    pass


class NikkeCharacterNotFoundError(NikkeLibraryError):
    pass


class NikkeAssetNotFoundError(NikkeLibraryError):
    pass


@dataclass(frozen=True, slots=True)
class NikkeAssetFile:
    path: Path
    content_type: str
    headers: dict[str, str]


@dataclass(frozen=True, slots=True)
class _NikkeRecord:
    content_id: int
    root: Path
    manifest_path: Path
    directory_name: str
    manifest: dict[str, Any]


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


def _extract_text_value(base_info: dict[str, Any], label: str) -> str:
    rows = base_info.get(label)
    if not isinstance(rows, list):
        return ''
    for row in rows:
        if not isinstance(row, dict):
            continue
        values = row.get('values')
        if not isinstance(values, list):
            continue
        for cell in values:
            if not isinstance(cell, dict) or cell.get('type') != 'text':
                continue
            text = _clean_text(cell.get('value'))
            if text:
                return text
    return ''


def _manifest_content_id(manifest_path: Path, manifest: dict[str, Any]) -> int | None:
    content_id = _to_int(manifest.get('content_id'))
    if content_id is not None:
        return content_id

    prefix = manifest_path.parent.name.split(' - ', 1)[0].strip()
    return _to_int(prefix)


def _safe_manifest_title(record: _NikkeRecord) -> str:
    title = _clean_text(record.manifest.get('title'))
    if title:
        return title
    return record.directory_name.split(' - ', 1)[-1].strip() or str(record.content_id)


class NikkeLibrary:
    def __init__(self, root: Path) -> None:
        self._root = Path(root)
        self._signature: tuple[tuple[str, int, int], ...] | None = None
        self._records: dict[int, _NikkeRecord] = {}
        self._summaries: list[dict[str, Any]] = []

    def list_characters(self) -> list[dict[str, Any]]:
        self._load_records()
        return list(self._summaries)

    def get_character(self, content_id: int) -> dict[str, Any]:
        record = self._get_record(content_id)
        character = self._read_character_json(record)
        skins = character.get('skins')
        if not isinstance(skins, list):
            content_summary = record.manifest.get('content_summary')
            skins = content_summary.get('skins', []) if isinstance(content_summary, dict) else []
        live2d_models = character.get('live2d_models')
        if not isinstance(live2d_models, list):
            live2d_models = record.manifest.get('live2d_models', [])
        if not isinstance(live2d_models, list):
            live2d_models = []

        payload = self._summary_payload(record)
        payload.update(
            {
                'tj_list': character.get('tj_list') if isinstance(character.get('tj_list'), dict) else record.manifest.get('tj_list'),
                'base_info': self._base_info(record, character),
                'skins': [self._skin_payload(record, skin, live2d_models) for skin in skins if isinstance(skin, dict)],
                'live2d_models': [self._live2d_model_payload(record, model) for model in live2d_models if isinstance(model, dict)],
                'assets': [self._asset_ref(record, asset) for asset in _iter_assets(record.manifest)],
            },
        )
        return payload

    def get_asset_file(self, content_id: int, asset_path: str) -> NikkeAssetFile:
        record = self._get_record(content_id)
        normalized_path = self._normalize_asset_path(asset_path)
        asset = next((item for item in _iter_assets(record.manifest) if item.get('local_path') == normalized_path), None)
        if asset is None:
            msg = f'Nikke asset not found: {content_id}/{normalized_path}'
            raise NikkeAssetNotFoundError(msg)

        target = (record.root / normalized_path).resolve()
        record_root = record.root.resolve()
        assets_root = (record.root / 'assets').resolve()
        if not assets_root.is_relative_to(record_root) or not target.is_relative_to(assets_root) or not target.is_file():
            msg = f'Nikke asset file not found: {content_id}/{normalized_path}'
            raise NikkeAssetNotFoundError(msg)

        sha256 = _clean_text(asset.get('sha256'))
        headers = {
            'Cache-Control': _LONG_CACHE_CONTROL if sha256 else _SHORT_CACHE_CONTROL,
            'X-Content-Type-Options': 'nosniff',
        }
        if sha256:
            headers['ETag'] = f'"{sha256}"'
        return NikkeAssetFile(path=target, content_type=_clean_text(asset.get('content_type')), headers=headers)

    def _manifest_entries(self) -> list[tuple[Path, int, int]]:
        if not self._root.is_dir():
            return []

        entries: list[tuple[Path, int, int]] = []
        try:
            children = list(self._root.iterdir())
        except OSError:
            log.exception('Failed to list Nikke collection root: %s', self._root)
            return []

        for child in children:
            if child.name.startswith('_') or not child.is_dir():
                continue
            manifest_path = child / 'manifest.json'
            if not manifest_path.is_file():
                continue
            try:
                stat = manifest_path.stat()
            except OSError:
                log.warning('Failed to stat Nikke manifest: %s', manifest_path)
                continue
            entries.append((manifest_path, stat.st_mtime_ns, stat.st_size))
        return sorted(entries, key=lambda entry: entry[0].parent.name.casefold())

    def _load_records(self) -> dict[int, _NikkeRecord]:
        entries = self._manifest_entries()
        signature = tuple((path.as_posix(), mtime_ns, size) for path, mtime_ns, size in entries)
        if signature == self._signature:
            return self._records

        records: dict[int, _NikkeRecord] = {}
        for manifest_path, _mtime_ns, _size in entries:
            try:
                manifest = _read_json_object(manifest_path)
            except (OSError, TypeError, ValueError) as exc:
                log.warning('Skipping unreadable Nikke manifest %s: %s', manifest_path, exc)
                continue

            content_id = _manifest_content_id(manifest_path, manifest)
            if content_id is None:
                log.warning('Skipping Nikke manifest without content id: %s', manifest_path)
                continue
            records[content_id] = _NikkeRecord(
                content_id=content_id,
                root=manifest_path.parent,
                manifest_path=manifest_path,
                directory_name=manifest_path.parent.name,
                manifest=manifest,
            )

        self._records = records
        self._summaries = sorted(
            (self._summary_payload(record) for record in records.values()),
            key=lambda item: (str(item.get('title') or '').casefold(), int(item.get('content_id') or 0)),
        )
        self._signature = signature
        return records

    def _get_record(self, content_id: int) -> _NikkeRecord:
        record = self._load_records().get(content_id)
        if record is None:
            msg = f'Nikke character not found: {content_id}'
            raise NikkeCharacterNotFoundError(msg)
        return record

    def _read_character_json(self, record: _NikkeRecord) -> dict[str, Any]:
        path = record.root / 'character.json'
        try:
            return _read_json_object(path)
        except (OSError, TypeError, ValueError) as exc:
            log.warning('Falling back to manifest data for %s: %s', record.directory_name, exc)
            return {}

    def _base_info(self, record: _NikkeRecord, character: dict[str, Any] | None = None) -> dict[str, Any]:
        source = character if character is not None else self._read_character_json(record)
        base_info = source.get('base_info') if isinstance(source, dict) else None
        if isinstance(base_info, dict):
            return base_info
        fallback = record.manifest.get('base_info')
        return fallback if isinstance(fallback, dict) else {}

    def _summary_payload(self, record: _NikkeRecord) -> dict[str, Any]:
        profile = self._profile_payload(record, {})
        tags = {item['key']: item['value'] for item in profile if item['key'] in _SUMMARY_TAG_KEYS and item['value']}
        live2d_models = record.manifest.get('live2d_models', [])
        content_summary = record.manifest.get('content_summary')
        manifest_skins = content_summary.get('skins', []) if isinstance(content_summary, dict) else []

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
            'icon': self._pick_asset(record, field='icon')
            or self._pick_asset(record, label='icon')
            or self._pick_asset(record, label='部队成员1'),
            'portrait': self._pick_asset(record, label='时装立绘', skin_index=0)
            or self._pick_asset(record, label='时装图（切换）', skin_index=0)
            or self._pick_asset(record, field='icon1'),
            'skin_count': len(manifest_skins) if isinstance(manifest_skins, list) else 0,
            'live2d_model_count': len(live2d_models) if isinstance(live2d_models, list) else 0,
        }

    def _profile_payload(self, record: _NikkeRecord, character: dict[str, Any]) -> list[dict[str, Any]]:
        base_info = self._base_info(record, character)
        tj_list = character.get('tj_list') if isinstance(character.get('tj_list'), dict) else record.manifest.get('tj_list')
        tj_list = tj_list if isinstance(tj_list, dict) else {}
        profile: list[dict[str, Any]] = []
        for key, label, tj_key in _PROFILE_FIELDS:
            value = _extract_text_value(base_info, label)
            if not value and tj_key:
                value = _clean_text(tj_list.get(tj_key))
            asset = self._pick_asset(record, label=label)
            if not value and asset is None:
                continue
            profile.append({'key': key, 'label': label, 'value': value, 'asset': asset})
        return profile

    def _skin_payload(self, record: _NikkeRecord, skin: dict[str, Any], live2d_models: list[Any]) -> dict[str, Any]:
        skin_index = _to_int(skin.get('skin_index'))
        models = [
            self._live2d_model_payload(record, model)
            for model in live2d_models
            if isinstance(model, dict) and _to_int(model.get('skin_index')) == skin_index
        ]
        rows = skin.get('rows')
        rows = rows if isinstance(rows, list) else []
        return {
            'skin_index': skin_index,
            'name': _clean_text(skin.get('name')),
            'title': _clean_text(skin.get('title')),
            'series': _clean_text(skin.get('series')),
            'obtain': _clean_text(skin.get('obtain')),
            'is_collection_skin': bool(skin.get('is_collection_skin')),
            'thumbnail': self._pick_asset(record, label='时装图（切换）', skin_index=skin_index),
            'portrait': self._pick_asset(record, label='时装立绘', skin_index=skin_index),
            'sd_model': self._pick_asset(record, label='SD模型', skin_index=skin_index),
            'burst_animation': self._pick_asset(record, label='爆裂动画', skin_index=skin_index),
            'gallery': self._assets_for_context(record, section='style', skin_index=skin_index, kind='image'),
            'live2d_models': models,
            'voice_lines': self._voice_lines_from_rows(rows),
            'rows': rows,
        }

    def _live2d_model_payload(self, record: _NikkeRecord, model: dict[str, Any]) -> dict[str, Any]:
        live2d_key = _clean_text(model.get('live2d_key'))
        textures = self._assets_for_context(record, live2d_key=live2d_key, kind='live2d_texture')
        return {
            'label': _clean_text(model.get('label')),
            'section': _clean_text(model.get('section')),
            'row_index': _to_int(model.get('row_index')),
            'skin_index': _to_int(model.get('skin_index')),
            'skin_name': _clean_text(model.get('skin_name')),
            'skin_title': _clean_text(model.get('skin_title')),
            'skin_series': _clean_text(model.get('skin_series')),
            'skin_obtain': _clean_text(model.get('skin_obtain')),
            'is_collection_skin': bool(model.get('is_collection_skin')),
            'key': _clean_text(model.get('key')),
            'stable_id': _clean_text(model.get('stable_id')),
            'live2d_key': live2d_key,
            'animation': _clean_text(model.get('animation')),
            'skin': _clean_text(model.get('skin')),
            'limit_age': bool(model.get('limit_age')),
            'position': model.get('position') if isinstance(model.get('position'), dict) else {},
            'bg_position': model.get('bg_position') if isinstance(model.get('bg_position'), dict) else {},
            'source_urls': model.get('urls') if isinstance(model.get('urls'), dict) else {},
            'assets': {
                'atlas': self._pick_asset(record, kind='live2d_atlas', live2d_key=live2d_key, live2d_field='atlas'),
                'skel': self._pick_asset(record, kind='live2d_skel', live2d_key=live2d_key, live2d_field='skel'),
                'json': self._pick_asset(record, kind='live2d_json', live2d_key=live2d_key, live2d_field='json'),
                'textures': textures,
            },
        }

    def _pick_asset(  # noqa: PLR0913
        self,
        record: _NikkeRecord,
        *,
        kind: str | None = 'image',
        label: str | None = None,
        field: str | None = None,
        section: str | None = None,
        skin_index: int | None = None,
        live2d_key: str | None = None,
        live2d_field: str | None = None,
    ) -> dict[str, Any] | None:
        assets = self._assets_for_context(
            record,
            kind=kind,
            label=label,
            field=field,
            section=section,
            skin_index=skin_index,
            live2d_key=live2d_key,
            live2d_field=live2d_field,
        )
        return assets[0] if assets else None

    def _assets_for_context(  # noqa: PLR0913
        self,
        record: _NikkeRecord,
        *,
        kind: str | None = None,
        label: str | None = None,
        field: str | None = None,
        section: str | None = None,
        skin_index: int | None = None,
        live2d_key: str | None = None,
        live2d_field: str | None = None,
    ) -> list[dict[str, Any]]:
        found: list[tuple[tuple[int, str, str], dict[str, Any]]] = []
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
                    skin_index=skin_index,
                    live2d_key=live2d_key,
                    live2d_field=live2d_field,
                ):
                    continue
                local_path = _clean_text(asset.get('local_path'))
                if local_path in seen_paths:
                    continue
                seen_paths.add(local_path)
                sort_key = (_to_int(context.get('row_index')) or 0, _context_value(context, 'label'), local_path)
                found.append((sort_key, self._asset_ref(record, asset, context)))
        return [item for _sort_key, item in sorted(found, key=lambda entry: entry[0])]

    @staticmethod
    def _context_matches(  # noqa: PLR0913
        context: dict[str, Any],
        *,
        label: str | None,
        field: str | None,
        section: str | None,
        skin_index: int | None,
        live2d_key: str | None,
        live2d_field: str | None,
    ) -> bool:
        return all(
            (
                label is None or _context_value(context, 'label') == label,
                field is None or _context_value(context, 'field') == field,
                section is None or _context_value(context, 'section') == section,
                skin_index is None or _to_int(context.get('skin_index')) == skin_index,
                live2d_key is None or _context_value(context, 'live2d_key') == live2d_key,
                live2d_field is None or _context_value(context, 'live2d_field') == live2d_field,
            ),
        )

    def _asset_ref(self, record: _NikkeRecord, asset: dict[str, Any], context: dict[str, Any] | None = None) -> dict[str, Any]:
        local_path = _clean_text(asset.get('local_path'))
        selected_context = context or {}
        available = False
        if local_path:
            try:
                normalized = self._normalize_asset_path(local_path)
            except NikkeAssetNotFoundError:
                normalized = ''
            if normalized:
                available = (record.root / normalized).is_file()
        sha256 = _clean_text(asset.get('sha256'))
        return {
            'kind': _clean_text(asset.get('kind')),
            'path': local_path,
            'url': self._asset_api_url(record.content_id, local_path, sha256=sha256) if local_path else '',
            'content_type': _clean_text(asset.get('content_type')),
            'size': _to_int(asset.get('size')) or 0,
            'sha256': sha256,
            'status': _clean_text(asset.get('status')),
            'label': _context_value(selected_context, 'label'),
            'field': _context_value(selected_context, 'field') or _context_value(selected_context, 'live2d_field'),
            'skin_index': _to_int(selected_context.get('skin_index')),
            'live2d_key': _context_value(selected_context, 'live2d_key'),
            'available': available,
            'contexts': _iter_contexts(asset),
        }

    @staticmethod
    def _asset_api_url(content_id: int, asset_path: str, *, sha256: str = '') -> str:
        url = f'{API_V2_PREFIX}/nikke/assets/{content_id}/{quote(asset_path, safe="/")}'
        return f'{url}?v={quote(sha256)}' if sha256 else url

    @staticmethod
    def _normalize_external_url(url: str) -> str:
        value = url.strip()
        if value.startswith('//'):
            return f'https:{value}'
        return value

    @classmethod
    def _voice_lines_from_rows(cls, rows: list[Any]) -> list[dict[str, str]]:
        voice_lines: list[dict[str, str]] = []
        for row in rows:
            if not isinstance(row, dict):
                continue
            cells = row.get('cells')
            if not isinstance(cells, list):
                continue
            audio_urls = [
                cls._normalize_external_url(_clean_text(cell.get('value')))
                for cell in cells
                if isinstance(cell, dict) and cell.get('type') == 'audio' and _clean_text(cell.get('value'))
            ]
            if not audio_urls:
                continue
            text_values = [
                _clean_text(cell.get('value'))
                for cell in cells
                if isinstance(cell, dict) and cell.get('type') == 'text' and _clean_text(cell.get('value'))
            ]
            label = _clean_text(row.get('label')) or (text_values[0] if text_values else '')
            text = text_values[1] if len(text_values) > 1 else ''
            voice_lines.extend({'label': label, 'text': text, 'source_url': source_url} for source_url in audio_urls)
        return voice_lines

    @staticmethod
    def _normalize_asset_path(asset_path: str) -> str:
        raw_path = unquote(asset_path).replace('\\', '/').strip()
        path = PurePosixPath(raw_path)
        parts = path.parts
        if not raw_path or raw_path.startswith('/') or path.is_absolute() or not parts:
            msg = f'Invalid Nikke asset path: {asset_path}'
            raise NikkeAssetNotFoundError(msg)
        if parts[0] != 'assets' or any(part in {'', '.', '..'} for part in parts):
            msg = f'Invalid Nikke asset path: {asset_path}'
            raise NikkeAssetNotFoundError(msg)
        return path.as_posix()

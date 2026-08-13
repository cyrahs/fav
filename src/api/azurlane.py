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

log = logger.get('fav-api.azurlane')

_LONG_CACHE_CONTROL = 'public, max-age=31536000, immutable'
_SHORT_CACHE_CONTROL = 'public, max-age=3600'
_AZURLANE_STATIC_PREFIX = '/static/azurlane'
_REPRESENTATIVE_ASSET_KINDS = {'live2d.texture', 'spine.texture', 'painting.image'}
_ASSET_KIND_ORDER = {
    kind: index
    for index, kind in enumerate(
        (
            'live2d.model3',
            'live2d.moc3',
            'live2d.texture',
            'live2d.physics',
            'live2d.pose',
            'live2d.display-info',
            'live2d.expression',
            'live2d.motion',
            'live2d.audio',
            'spine.parts',
            'spine.skel',
            'spine.atlas',
            'spine.texture',
            'painting.image',
            'painting.face',
            'icon.square',
            'icon.shipyard',
            'voice.audio',
        ),
    )
}


class AzurLaneLibraryError(RuntimeError):
    pass


class AzurLaneCharacterNotFoundError(AzurLaneLibraryError):
    pass


class AzurLaneAssetNotFoundError(AzurLaneLibraryError):
    pass


@dataclass(frozen=True, slots=True)
class AzurLaneAssetFile:
    path: Path
    content_type: str
    headers: dict[str, str]


@dataclass(frozen=True, slots=True)
class _AzurLaneRecord:
    character_key: str
    root: Path
    manifest_path: Path
    directory_name: str
    manifest: dict[str, Any]


@dataclass(frozen=True, slots=True)
class _AzurLaneRecordEntry:
    character_key: str
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


def _json_object(value: Any) -> dict[str, Any]:
    if isinstance(value, dict):
        return value
    return {}


def _json_list(value: Any) -> list[Any]:
    return value if isinstance(value, list) else []


def _iter_models(manifest: dict[str, Any]) -> list[dict[str, Any]]:
    return [model for model in _json_list(manifest.get('models')) if isinstance(model, dict)]


def _iter_assets(model: dict[str, Any]) -> list[dict[str, Any]]:
    return [asset for asset in _json_list(model.get('assets')) if isinstance(asset, dict)]


def _iter_all_assets(manifest: dict[str, Any]) -> list[dict[str, Any]]:
    assets: list[dict[str, Any]] = []
    for model in _iter_models(manifest):
        assets.extend(_iter_assets(model))
    return assets


def _iter_contexts(asset: dict[str, Any]) -> list[dict[str, Any]]:
    return [context for context in _json_list(asset.get('contexts')) if isinstance(context, dict)]


def _string_list(value: Any) -> list[str]:
    return [_clean_text(item) for item in _json_list(value) if _clean_text(item)]


def _context_value(context: dict[str, Any], key: str) -> str:
    return _clean_text(context.get(key))


def _manifest_character_key(manifest_path: Path, manifest: dict[str, Any]) -> str:
    character_key = _clean_text(manifest.get('character_key'))
    if character_key:
        return character_key
    return manifest_path.parent.name.split(' - ', 1)[0].strip()


def _safe_display_name(record: _AzurLaneRecord) -> str:
    name_en = _clean_text(record.manifest.get('name_en'))
    if name_en:
        return name_en
    name_zh = _clean_text(record.manifest.get('name_zh'))
    if name_zh:
        return name_zh
    suffix = record.directory_name.split(' - ', 1)[-1].strip()
    return suffix or record.character_key


def _sort_timestamp(value: Any) -> float:  # noqa: PLR0911
    result = 0.0
    if isinstance(value, bool) or value is None:
        return result
    if isinstance(value, (int, float)):
        return float(value)
    if not isinstance(value, str):
        return result

    text = value.strip()
    if not text:
        return result
    if text.isdigit():
        return float(text)
    normalized = f'{text.removesuffix("Z")}+00:00' if text.endswith('Z') else text
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return result
    return parsed.replace(tzinfo=UTC).timestamp() if parsed.tzinfo is None else parsed.timestamp()


def _record_freshness_key(manifest: dict[str, Any], *, mtime_ns: int) -> tuple[float, float, int]:
    return (
        _sort_timestamp(manifest.get('fetched_at')),
        _sort_timestamp(manifest.get('completed_at')),
        mtime_ns,
    )


def _status_counts(items: list[str]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for item in items:
        if not item:
            continue
        counts[item] = counts.get(item, 0) + 1
    return {key: counts[key] for key in sorted(counts)}


def _model_counts(models: list[dict[str, Any]]) -> dict[str, int]:
    counts = {'live2d': 0, 'spine': 0, 'painting': 0, 'total': 0}
    for model in models:
        model_type = _clean_text(model.get('type'))
        if model_type in counts:
            counts[model_type] += 1
        counts['total'] += 1
    return counts


def _asset_counts(models: list[dict[str, Any]]) -> dict[str, int]:
    return _status_counts([_clean_text(asset.get('kind')) for model in models for asset in _iter_assets(model)])


def _asset_sort_key(asset: dict[str, Any]) -> tuple[int, str, str]:
    return (
        _ASSET_KIND_ORDER.get(_clean_text(asset.get('kind')), len(_ASSET_KIND_ORDER)),
        _clean_text(asset.get('local_path')),
        _clean_text(asset.get('url')),
    )


def _model_sort_key(model: dict[str, Any]) -> tuple[str, str, str]:
    costume = _json_object(model.get('costume'))
    return (_clean_text(model.get('type')), _clean_text(costume.get('key')), _clean_text(model.get('model_id')))


class AzurLaneLibrary:
    def __init__(self, root: Path) -> None:
        self._root = Path(root)
        self._signature: ManifestSignature | None = None
        self._record_entries: dict[str, _AzurLaneRecordEntry] = {}
        self._summaries: list[dict[str, Any]] = []
        self._cache_lock = threading.Lock()

    def list_characters(self) -> list[dict[str, Any]]:
        self._ensure_summary_cache()
        return list(self._summaries)

    def get_character(self, character_key: str) -> dict[str, Any]:
        record = self._get_record(character_key)
        models = [self._model_payload(record, model) for model in sorted(_iter_models(record.manifest), key=_model_sort_key)]
        payload = self._summary_payload(record)
        payload.update(
            {
                'active': bool(record.manifest.get('active', True)),
                'models': models,
                'live2d_models': [model for model in models if model['type'] == 'live2d'],
                'spine_models': [model for model in models if model['type'] == 'spine'],
                'painting_models': [model for model in models if model['type'] == 'painting'],
                'assets': [asset for model in models for asset in model['assets']],
            },
        )
        return payload

    def get_asset_file(self, character_key: str, asset_path: str) -> AzurLaneAssetFile:
        record = self._get_record(character_key)
        normalized_path = self._normalize_asset_path(asset_path)
        asset = self._asset_by_normalized_path(record, normalized_path)
        if asset is None:
            msg = f'Azur Lane asset not found: {character_key}/{normalized_path}'
            raise AzurLaneAssetNotFoundError(msg)

        target = self._resolved_asset_path(record, normalized_path)
        if target is None or not target.is_file():
            msg = f'Azur Lane asset file not found: {character_key}/{normalized_path}'
            raise AzurLaneAssetNotFoundError(msg)

        sha256 = _clean_text(asset.get('sha256'))
        headers = {
            'Cache-Control': _LONG_CACHE_CONTROL if sha256 else _SHORT_CACHE_CONTROL,
            'X-Content-Type-Options': 'nosniff',
        }
        if sha256:
            headers['ETag'] = f'"{sha256}"'
        return AzurLaneAssetFile(path=target, content_type=_clean_text(asset.get('content_type')), headers=headers)

    def get_ship_detail(self, character_key: str) -> dict[str, Any]:
        record = self._get_record(character_key)
        detail_path = record.manifest_path.parent / 'detail.json'
        if not detail_path.is_file():
            msg = f'Azur Lane ship detail not found: {record.character_key}'
            raise AzurLaneAssetNotFoundError(msg)
        try:
            return _read_json_object(detail_path)
        except (OSError, TypeError, ValueError) as exc:
            msg = f'Azur Lane ship detail not readable: {record.character_key}'
            raise AzurLaneAssetNotFoundError(msg) from exc

    def _manifest_entries(self) -> list[ManifestEntry]:
        if not self._root.is_dir():
            return []

        entries: list[tuple[Path, int, int]] = []
        try:
            root_resolved = self._root.resolve()
            children = list(self._root.iterdir())
        except OSError:
            log.exception('Failed to list Azur Lane collection root: %s', self._root)
            return []

        for child in children:
            if child.name.startswith('_') or not child.is_dir():
                continue
            try:
                child_resolved = child.resolve()
            except OSError:
                log.warning('Failed to resolve Azur Lane character directory: %s', child)
                continue
            if not child_resolved.is_relative_to(root_resolved):
                log.warning('Skipping Azur Lane character directory outside collection root: %s', child)
                continue

            manifest_path = child / 'manifest.json'
            if not manifest_path.is_file():
                continue
            try:
                manifest_resolved = manifest_path.resolve()
                stat = manifest_path.stat()
            except OSError:
                log.warning('Failed to stat Azur Lane manifest: %s', manifest_path)
                continue
            if not manifest_resolved.is_relative_to(child_resolved) or not manifest_resolved.is_relative_to(root_resolved):
                log.warning('Skipping Azur Lane manifest outside collection root: %s', manifest_path)
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

    def _build_summary_cache(self, entries: list[ManifestEntry]) -> tuple[dict[str, _AzurLaneRecordEntry], list[dict[str, Any]]]:
        record_entries: dict[str, _AzurLaneRecordEntry] = {}
        summaries_by_character_key: dict[str, dict[str, Any]] = {}
        freshness_by_character_key: dict[str, tuple[float, float, int]] = {}
        for manifest_path, mtime_ns, _size in entries:
            try:
                manifest = _read_json_object(manifest_path)
            except (OSError, TypeError, ValueError) as exc:
                log.warning('Skipping unreadable Azur Lane manifest %s: %s', manifest_path, exc)
                continue

            character_key = _manifest_character_key(manifest_path, manifest)
            if not character_key:
                log.warning('Skipping Azur Lane manifest without character key: %s', manifest_path)
                continue
            freshness = _record_freshness_key(manifest, mtime_ns=mtime_ns)
            existing = record_entries.get(character_key)
            if existing is not None:
                if freshness <= freshness_by_character_key[character_key]:
                    log.warning('Skipping stale duplicate Azur Lane manifest for character_key=%s: %s', character_key, manifest_path)
                    continue
                log.warning(
                    'Replacing stale duplicate Azur Lane manifest for character_key=%s: %s -> %s',
                    character_key,
                    existing.manifest_path,
                    manifest_path,
                )

            record = _AzurLaneRecord(
                character_key=character_key,
                root=manifest_path.parent,
                manifest_path=manifest_path,
                directory_name=manifest_path.parent.name,
                manifest=manifest,
            )
            record_entries[character_key] = _AzurLaneRecordEntry(
                character_key=character_key,
                root=manifest_path.parent,
                manifest_path=manifest_path,
                directory_name=manifest_path.parent.name,
            )
            summaries_by_character_key[character_key] = self._summary_payload(record)
            freshness_by_character_key[character_key] = freshness

        summaries = sorted(
            summaries_by_character_key.values(),
            key=lambda item: (str(item.get('display_name') or '').casefold(), str(item.get('character_key') or '')),
        )
        return record_entries, summaries

    def _load_summary_cache(self, signature: ManifestSignature) -> bool:
        cached = read_summary_cache(
            self._summary_cache_path,
            expected_signature=signature,
            log=log,
            label='Azur Lane',
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
    ) -> dict[str, _AzurLaneRecordEntry] | None:
        current_paths = {path for path, _mtime_ns, _size in signature}
        record_entries: dict[str, _AzurLaneRecordEntry] = {}
        for item in records:
            character_key = _clean_text(item.get('character_key'))
            manifest_path_value = item.get('manifest_path')
            if not character_key or not isinstance(manifest_path_value, str) or manifest_path_value not in current_paths:
                return None
            manifest_path = Path(manifest_path_value)
            record_entries[character_key] = _AzurLaneRecordEntry(
                character_key=character_key,
                root=manifest_path.parent,
                manifest_path=manifest_path,
                directory_name=manifest_path.parent.name,
            )
        return record_entries

    def _replace_summary_cache(
        self,
        *,
        signature: ManifestSignature,
        record_entries: dict[str, _AzurLaneRecordEntry],
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
            {'character_key': entry.character_key, 'manifest_path': entry.manifest_path.as_posix()}
            for entry in sorted(self._record_entries.values(), key=lambda item: item.character_key)
        ]
        write_summary_cache(
            self._summary_cache_path,
            data=SummaryCacheData(signature=self._signature or (), records=records, summaries=self._summaries),
            log=log,
            label='Azur Lane',
        )

    def _get_record(self, character_key: str) -> _AzurLaneRecord:
        self._ensure_summary_cache()
        normalized_key = _clean_text(character_key)
        entry = self._record_entries.get(normalized_key)
        if entry is None:
            msg = f'Azur Lane character not found: {normalized_key}'
            raise AzurLaneCharacterNotFoundError(msg)
        try:
            manifest = _read_json_object(entry.manifest_path)
        except (OSError, TypeError, ValueError) as exc:
            log.warning('Skipping unreadable Azur Lane manifest %s: %s', entry.manifest_path, exc)
            msg = f'Azur Lane character not found: {normalized_key}'
            raise AzurLaneCharacterNotFoundError(msg) from exc
        return _AzurLaneRecord(
            character_key=entry.character_key,
            root=entry.root,
            manifest_path=entry.manifest_path,
            directory_name=entry.directory_name,
            manifest=manifest,
        )

    def _summary_payload(self, record: _AzurLaneRecord) -> dict[str, Any]:
        models = _iter_models(record.manifest)
        source_counts = _status_counts([_clean_text(model.get('source')) for model in models])
        model_types = sorted({_clean_text(model.get('type')) for model in models if _clean_text(model.get('type'))})
        source_names = sorted(source_counts)
        model_counts = _json_object(record.manifest.get('model_counts')) or _model_counts(models)
        asset_counts = _json_object(record.manifest.get('asset_counts')) or _asset_counts(models)
        return {
            'schema_version': _to_int(record.manifest.get('schema_version')) or 0,
            'source': _clean_text(record.manifest.get('source')) or 'azurlane',
            'character_key': record.character_key,
            'source_id': _to_int(record.manifest.get('source_id')),
            'title': _safe_display_name(record),
            'display_name': _safe_display_name(record),
            'name_zh': _clean_text(record.manifest.get('name_zh')),
            'name_en': _clean_text(record.manifest.get('name_en')),
            'directory_name': record.directory_name,
            'source_metadata': _json_object(record.manifest.get('source_metadata')),
            'fetched_at': _clean_text(record.manifest.get('fetched_at')) or None,
            'completed_at': _clean_text(record.manifest.get('completed_at')) or None,
            'model_counts': model_counts,
            'asset_counts': asset_counts,
            'source_counts': source_counts,
            'tags': {
                'model_types': ', '.join(model_types),
                'sources': ', '.join(source_names),
            },
            'representative_asset': self._pick_representative_asset(record),
            'icon': self._pick_icon_asset(record, models),
            'model_count': _to_int(model_counts.get('total')) or len(models),
        }

    def _pick_icon_asset(self, record: _AzurLaneRecord, models: list[dict[str, Any]]) -> dict[str, Any] | None:
        """Square icon of the default skin, for list and sidebar avatars."""
        default_skin_id = _to_int(_json_object(record.manifest.get('source_metadata')).get('default_skin_id'))
        icons: list[tuple[bool, dict[str, Any]]] = []
        for model in models:
            if _clean_text(model.get('type')) != 'painting':
                continue
            costume_id = _to_int(_json_object(model.get('costume')).get('id'))
            is_default = costume_id is not None and costume_id == default_skin_id
            icons.extend((is_default, asset) for asset in _iter_assets(model) if _clean_text(asset.get('kind')) == 'icon.square')

        if not icons:
            return None
        # Prefer the default skin's icon, then any downloaded icon, then whatever exists.
        refs = [(is_default, self._asset_ref(record, asset)) for is_default, asset in icons]
        for wanted_default in (True, False):
            for is_default, ref in refs:
                if is_default is wanted_default and ref['available']:
                    return ref
        return refs[0][1]

    def _model_payload(self, record: _AzurLaneRecord, model: dict[str, Any]) -> dict[str, Any]:
        assets = [self._asset_ref(record, asset) for asset in sorted(_iter_assets(model), key=_asset_sort_key)]
        model_type = _clean_text(model.get('type'))
        return {
            'model_id': _clean_text(model.get('model_id')),
            'type': model_type,
            'source': _clean_text(model.get('source')),
            'character_key': _clean_text(model.get('character_key')) or record.character_key,
            'costume': self._costume_payload(model),
            'source_urls': _json_object(model.get('source_urls')),
            'availability': _json_object(model.get('availability')),
            'source_metadata': _json_object(model.get('source_metadata')),
            'fetched_at': _clean_text(model.get('fetched_at')) or None,
            'completed_at': _clean_text(model.get('completed_at')) or None,
            'asset_counts': _json_object(model.get('asset_counts')) or _status_counts([_clean_text(asset.get('kind')) for asset in assets]),
            'files': self._model_files(model_type, assets),
            'assets': assets,
        }

    @staticmethod
    def _costume_payload(model: dict[str, Any]) -> dict[str, Any]:
        costume = _json_object(model.get('costume'))
        return {
            'key': _clean_text(costume.get('key')),
            'id': _to_int(costume.get('id')),
            'name_zh': _clean_text(costume.get('name_zh')),
            'name_en': _clean_text(costume.get('name_en')),
        }

    def _model_files(self, model_type: str, assets: list[dict[str, Any]]) -> dict[str, Any]:
        if model_type == 'live2d':
            return {
                'model3': self._first_asset(assets, 'live2d.model3'),
                'moc3': self._first_asset(assets, 'live2d.moc3'),
                'textures': self._assets_by_kind(assets, 'live2d.texture'),
                'physics': self._first_asset(assets, 'live2d.physics'),
                'pose': self._first_asset(assets, 'live2d.pose'),
                'display_info': self._first_asset(assets, 'live2d.display-info'),
                'expressions': self._assets_by_kind(assets, 'live2d.expression'),
                'motions': self._assets_by_kind(assets, 'live2d.motion'),
                'audio': self._assets_by_kind(assets, 'live2d.audio'),
            }
        if model_type == 'spine':
            return {
                'parts': self._first_asset(assets, 'spine.parts'),
                'skel': self._first_asset(assets, 'spine.skel'),
                'skeletons': self._assets_by_kind(assets, 'spine.skel'),
                'atlas': self._first_asset(assets, 'spine.atlas'),
                'atlases': self._assets_by_kind(assets, 'spine.atlas'),
                'textures': self._assets_by_kind(assets, 'spine.texture'),
            }
        if model_type == 'painting':
            return {
                'image': self._first_asset(assets, 'painting.image'),
                'faces': self._assets_by_kind(assets, 'painting.face'),
                'square_icon': self._first_asset(assets, 'icon.square'),
                'shipyard_icon': self._first_asset(assets, 'icon.shipyard'),
                'voices': self._assets_by_kind(assets, 'voice.audio'),
            }
        return {}

    @staticmethod
    def _first_asset(assets: list[dict[str, Any]], kind: str) -> dict[str, Any] | None:
        return next((asset for asset in assets if asset['kind'] == kind), None)

    @staticmethod
    def _assets_by_kind(assets: list[dict[str, Any]], kind: str) -> list[dict[str, Any]]:
        return [asset for asset in assets if asset['kind'] == kind]

    def _pick_representative_asset(self, record: _AzurLaneRecord) -> dict[str, Any] | None:
        candidates = [self._asset_ref(record, asset) for asset in _iter_all_assets(record.manifest)]
        available = [asset for asset in candidates if asset['available']]
        representative = [asset for asset in available if asset['kind'] in _REPRESENTATIVE_ASSET_KINDS]
        if representative:
            return sorted(representative, key=lambda item: (_ASSET_KIND_ORDER.get(item['kind'], len(_ASSET_KIND_ORDER)), item['path']))[0]
        if available:
            return sorted(available, key=lambda item: (_ASSET_KIND_ORDER.get(item['kind'], len(_ASSET_KIND_ORDER)), item['path']))[0]
        return candidates[0] if candidates else None

    def _asset_ref(self, record: _AzurLaneRecord, asset: dict[str, Any]) -> dict[str, Any]:
        local_path = _clean_text(asset.get('local_path'))
        normalized_path = ''
        available = False
        if local_path:
            try:
                normalized_path = self._normalize_asset_path(local_path)
            except AzurLaneAssetNotFoundError:
                normalized_path = ''
            if normalized_path:
                target = self._resolved_asset_path(record, normalized_path)
                available = target is not None and target.is_file()

        sha256 = _clean_text(asset.get('sha256'))
        contexts = _iter_contexts(asset)
        primary_context = contexts[0] if contexts else {}
        source_urls = _json_object(asset.get('source_urls'))
        if not source_urls:
            source_urls = {
                'primary': _clean_text(asset.get('url')),
                'fallback': _clean_text(asset.get('fallback_url')),
                'downloaded': _clean_text(asset.get('downloaded_url')),
            }
        return {
            'kind': _clean_text(asset.get('kind')),
            'path': normalized_path or local_path,
            'url': self._asset_static_url(record.directory_name, normalized_path, sha256=sha256) if normalized_path else '',
            'source_url': _clean_text(asset.get('url')),
            'normalized_url': _clean_text(asset.get('normalized_url')),
            'downloaded_url': _clean_text(asset.get('downloaded_url')),
            'fallback_url': _clean_text(asset.get('fallback_url')),
            'source_urls': source_urls,
            'original_filename': _clean_text(asset.get('original_filename')),
            'content_type': _clean_text(asset.get('content_type')),
            'size': _to_int(asset.get('size')) or 0,
            'sha256': sha256,
            'status': _clean_text(asset.get('status')),
            'available': available,
            'failed_count': _to_int(asset.get('failed_count')) or 0,
            'error': _clean_text(asset.get('error')),
            'last_attempt_at': _clean_text(asset.get('last_attempt_at')) or None,
            'next_retry_at': _clean_text(asset.get('next_retry_at')) or None,
            'last_seen_at': _clean_text(asset.get('last_seen_at')) or None,
            'model_id': _context_value(primary_context, 'model_id'),
            'model_type': _context_value(primary_context, 'model_type'),
            'character_key': _context_value(primary_context, 'character_key'),
            'costume_key': _context_value(primary_context, 'costume_key'),
            'field': (
                _context_value(primary_context, 'live2d_field')
                or _context_value(primary_context, 'spine_field')
                or _context_value(primary_context, 'painting_field')
            ),
            'catalog_source': _context_value(primary_context, 'catalog_source'),
            'context_hashes': _string_list(asset.get('context_hashes')),
            'contexts': contexts,
        }

    def _asset_by_normalized_path(self, record: _AzurLaneRecord, normalized_path: str) -> dict[str, Any] | None:
        for asset in _iter_all_assets(record.manifest):
            local_path = _clean_text(asset.get('local_path'))
            if not local_path:
                continue
            try:
                asset_normalized_path = self._normalize_asset_path(local_path)
            except AzurLaneAssetNotFoundError:
                continue
            if asset_normalized_path == normalized_path:
                return asset
        return None

    def _resolved_asset_path(self, record: _AzurLaneRecord, normalized_path: str) -> Path | None:
        try:
            collection_root = self._root.resolve()
            record_root = record.root.resolve()
            assets_root = (record.root / 'assets').resolve()
            target = (record.root / normalized_path).resolve()
        except OSError:
            return None
        if not record_root.is_relative_to(collection_root):
            return None
        if not assets_root.is_relative_to(record_root) or not assets_root.is_relative_to(collection_root):
            return None
        if not target.is_relative_to(assets_root) or not target.is_relative_to(collection_root):
            return None
        return target

    @staticmethod
    def _asset_static_url(directory_name: str, asset_path: str, *, sha256: str = '') -> str:
        url = f'{_AZURLANE_STATIC_PREFIX}/{quote(directory_name, safe="")}/{quote(asset_path, safe="/")}'
        return f'{url}?v={quote(sha256)}' if sha256 else url

    @staticmethod
    def _normalize_asset_path(asset_path: str) -> str:
        raw_path = unquote(asset_path).replace('\\', '/').strip()
        path = PurePosixPath(raw_path)
        raw_parts = raw_path.split('/')
        if not raw_path or raw_path.startswith('/') or path.is_absolute() or not raw_parts:
            msg = f'Invalid Azur Lane asset path: {asset_path}'
            raise AzurLaneAssetNotFoundError(msg)
        if raw_parts[0] != 'assets' or any(part in {'', '.', '..'} for part in raw_parts):
            msg = f'Invalid Azur Lane asset path: {asset_path}'
            raise AzurLaneAssetNotFoundError(msg)
        return PurePosixPath(*raw_parts).as_posix()

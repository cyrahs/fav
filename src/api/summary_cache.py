from __future__ import annotations

import json
import os
import secrets
from contextlib import suppress
from dataclasses import dataclass
from pathlib import Path
from typing import Any

SUMMARY_CACHE_VERSION = 1
SUMMARY_RECORDS_VERSION = 2

type ManifestEntry = tuple[Path, int, int]
type ManifestSignature = tuple[tuple[str, int, int], ...]
type RecordFreshness = tuple[float, float, int]


@dataclass(frozen=True, slots=True)
class SummaryCacheData:
    signature: ManifestSignature
    records: list[dict[str, Any]]
    summaries: list[dict[str, Any]]


@dataclass(frozen=True, slots=True)
class SummaryRecord:
    key: str
    manifest_path: str
    freshness: RecordFreshness
    summary: dict[str, Any]


@dataclass(frozen=True, slots=True)
class SummaryRecordsData:
    signature: ManifestSignature
    records: tuple[SummaryRecord, ...]


def manifest_signature(entries: list[ManifestEntry]) -> ManifestSignature:
    return tuple((path.as_posix(), mtime_ns, size) for path, mtime_ns, size in entries)


def read_summary_cache(
    path: Path,
    *,
    expected_signature: ManifestSignature,
    log: Any,
    label: str,
) -> SummaryCacheData | None:
    try:
        payload = json.loads(path.read_text(encoding='utf-8'))
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as exc:
        log.warning('Skipping unreadable %s summary cache %s: %s', label, path, exc)
        return None

    data = _decode_summary_cache_payload(payload)
    if data is None or data.signature != expected_signature:
        return None
    return data


def write_summary_cache(
    path: Path,
    *,
    data: SummaryCacheData,
    log: Any,
    label: str,
) -> None:
    payload = {
        'version': SUMMARY_CACHE_VERSION,
        'signature': _encode_signature(data.signature),
        'records': data.records,
        'summaries': data.summaries,
    }
    _write_cache_payload(path, payload, log=log, label=label)


def read_summary_records(path: Path, *, log: Any, label: str) -> SummaryRecordsData | None:
    """Read a version-2 per-manifest summary cache.

    Unlike ``read_summary_cache`` this does not require a signature match: a stale
    cache is still valuable as the base for an incremental rebuild, so signature
    handling is left to the caller.
    """
    try:
        payload = json.loads(path.read_text(encoding='utf-8'))
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as exc:
        log.warning('Skipping unreadable %s summary cache %s: %s', label, path, exc)
        return None
    return _decode_summary_records_payload(payload)


def write_summary_records(path: Path, *, data: SummaryRecordsData, log: Any, label: str) -> None:
    payload = {
        'version': SUMMARY_RECORDS_VERSION,
        'signature': _encode_signature(data.signature),
        'records': [
            {
                'key': record.key,
                'manifest_path': record.manifest_path,
                'freshness': list(record.freshness),
                'summary': record.summary,
            }
            for record in data.records
        ],
    }
    _write_cache_payload(path, payload, log=log, label=label)


def _write_cache_payload(path: Path, payload: dict[str, Any], *, log: Any, label: str) -> None:
    tmp = path.with_name(f'.{path.name}.tmp-{os.getpid()}-{secrets.token_hex(4)}')
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(',', ':')) + '\n', encoding='utf-8')
        tmp.replace(path)
    except (OSError, TypeError) as exc:
        log.warning('Failed to write %s summary cache %s: %s', label, path, exc)
    finally:
        with suppress(FileNotFoundError):
            tmp.unlink()


def _decode_summary_records_payload(payload: Any) -> SummaryRecordsData | None:
    if not isinstance(payload, dict) or payload.get('version') != SUMMARY_RECORDS_VERSION:
        return None
    signature = _decode_signature(payload.get('signature'))
    if signature is None:
        return None
    items = payload.get('records')
    if not isinstance(items, list):
        return None

    records: list[SummaryRecord] = []
    for item in items:
        record = _decode_summary_record(item)
        if record is None:
            return None
        records.append(record)
    return SummaryRecordsData(signature=signature, records=tuple(records))


_FRESHNESS_LENGTH = 3


def _decode_summary_record(item: Any) -> SummaryRecord | None:
    if not isinstance(item, dict):
        return None
    key = item.get('key')
    manifest_path = item.get('manifest_path')
    freshness = item.get('freshness')
    summary = item.get('summary')
    if not isinstance(key, str) or not key or not isinstance(manifest_path, str) or not manifest_path:
        return None
    if not isinstance(summary, dict):
        return None
    if not isinstance(freshness, list) or len(freshness) != _FRESHNESS_LENGTH:
        return None
    if any(isinstance(value, bool) or not isinstance(value, (int, float)) for value in freshness):
        return None
    return SummaryRecord(
        key=key,
        manifest_path=manifest_path,
        freshness=(float(freshness[0]), float(freshness[1]), int(freshness[2])),
        summary=summary,
    )


def _decode_summary_cache_payload(payload: Any) -> SummaryCacheData | None:
    if not isinstance(payload, dict) or payload.get('version') != SUMMARY_CACHE_VERSION:
        return None
    signature = _decode_signature(payload.get('signature'))
    if signature is None:
        return None

    records = payload.get('records')
    summaries = payload.get('summaries')
    if not isinstance(records, list) or not isinstance(summaries, list):
        return None
    if not all(isinstance(item, dict) for item in records) or not all(isinstance(item, dict) for item in summaries):
        return None
    return SummaryCacheData(signature=signature, records=records, summaries=summaries)


def _encode_signature(signature: ManifestSignature) -> list[dict[str, int | str]]:
    return [{'path': path, 'mtime_ns': mtime_ns, 'size': size} for path, mtime_ns, size in signature]


def _decode_signature(value: Any) -> ManifestSignature | None:
    if not isinstance(value, list):
        return None

    decoded: list[tuple[str, int, int]] = []
    for item in value:
        if not isinstance(item, dict):
            return None
        path = item.get('path')
        mtime_ns = item.get('mtime_ns')
        size = item.get('size')
        if not isinstance(path, str) or isinstance(mtime_ns, bool) or not isinstance(mtime_ns, int):
            return None
        if isinstance(size, bool) or not isinstance(size, int):
            return None
        decoded.append((path, mtime_ns, size))
    return tuple(decoded)

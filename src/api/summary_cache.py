from __future__ import annotations

import json
import os
import secrets
from contextlib import suppress
from dataclasses import dataclass
from pathlib import Path
from typing import Any

SUMMARY_CACHE_VERSION = 1

type ManifestEntry = tuple[Path, int, int]
type ManifestSignature = tuple[tuple[str, int, int], ...]


@dataclass(frozen=True, slots=True)
class SummaryCacheData:
    signature: ManifestSignature
    records: list[dict[str, Any]]
    summaries: list[dict[str, Any]]


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
    tmp = path.with_name(f'.{path.name}.tmp-{os.getpid()}-{secrets.token_hex(4)}')
    payload = {
        'version': SUMMARY_CACHE_VERSION,
        'signature': _encode_signature(data.signature),
        'records': data.records,
        'summaries': data.summaries,
    }
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(',', ':')) + '\n', encoding='utf-8')
        tmp.replace(path)
    except (OSError, TypeError) as exc:
        log.warning('Failed to write %s summary cache %s: %s', label, path, exc)
    finally:
        with suppress(FileNotFoundError):
            tmp.unlink()


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

from __future__ import annotations

import re
from functools import lru_cache
from pathlib import Path

# Mirrors the client-side probe in game-view: both binary .skel files and
# exported .json skeletons carry the Spine version string within their first
# bytes, so reading the same 97-byte window keeps the two detections aligned.
_SPINE_VERSION_PATTERN = re.compile(r'4\.(\d+)\.\d+')
_SPINE_PROBE_BYTES = 97


def spine_version_from_file(path: Path) -> str | None:
    try:
        stat = path.stat()
    except OSError:
        return None
    return _cached_spine_version(str(path), stat.st_mtime_ns, stat.st_size)


@lru_cache(maxsize=4096)
def _cached_spine_version(path_str: str, _mtime_ns: int, _size: int) -> str | None:
    try:
        with Path(path_str).open('rb') as handle:
            sample = handle.read(_SPINE_PROBE_BYTES)
    except OSError:
        return None
    match = _SPINE_VERSION_PATTERN.search(sample.decode('utf-8', errors='replace'))
    return match.group(0) if match else None

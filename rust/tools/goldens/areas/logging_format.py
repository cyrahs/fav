"""`src/core/logger.py`: the file line and the console line one record becomes (WP1).

The file lines are what the MCP log tools parse (`src/api/ops.py`), so their layout is
part of the contract. Times are given in nanoseconds since the epoch and rendered in UTC;
`msecs` is derived the way Python 3.12's `LogRecord` derives it.
"""

from __future__ import annotations

import copy
import logging
import os
import time
from typing import Any

from src.core import logger

RECORDS: list[tuple[str, int, str, int]] = [
    ('embyx.run', logging.INFO, 'Job started: Jandan', 1_762_790_400_000_000_000),
    ('embyx.run', logger.NOTICE, 'Job completed: Jandan (12.3s)', 1_762_790_400_999_999_999),
    ('embyx.jandan', logging.DEBUG, 'payload keys: a, b', 1_762_790_401_123_456_789),
    ('embyx.telegram', logging.WARNING, 'flood wait 30s', 1_762_790_402_000_999_000),
    ('embyx.api', logging.ERROR, 'request failed: 中文 detail', 1_700_000_000_001_000_000),
    ('embyx.worker', logging.CRITICAL, 'giving up', 946_684_799_999_000_000),
    ('httpx', logging.WARNING, 'HTTP Request: GET https://example.com "HTTP/1.1 500"', 1_762_790_403_500_000_000),
    ('embyx', logging.INFO, 'multi\nline message', 1_762_790_404_000_000_000),
    ('embyx.x', logging.INFO, '', 1_762_790_405_010_000_000),
]


def _record(name: str, level: int, message: str, ns: int) -> logging.LogRecord:
    record = logging.LogRecord(name, level, __file__, 0, message, None, None)
    record.created = ns / 1e9
    record.msecs = (ns % 1_000_000_000) // 1_000_000 + 0.0
    return record


def _cases() -> list[dict[str, Any]]:
    file_formatter = copy.copy(logger.file_formatter)
    file_formatter.converter = time.gmtime
    saved = {name: os.environ.pop(name) for name in ('NO_COLOR', 'FORCE_COLOR') if name in os.environ}
    try:
        cases = []
        for name, level, message, ns in RECORDS:
            record = _record(name, level, message, ns)
            cases.append(
                {
                    'name': name,
                    'level': logging.getLevelName(level),
                    'message': message,
                    'time_ns': ns,
                    'file': file_formatter.format(record),
                    'console': logger.console_formatter.format(_record(name, level, message, ns)),
                }
            )
        return cases
    finally:
        os.environ.update(saved)


def _levels() -> list[dict[str, Any]]:
    # LOG_LEVEL names to the numbers the threshold compares.
    return [{'name': name, 'number': logging.getLevelName(name)} for name in ('DEBUG', 'INFO', 'NOTICE', 'WARNING', 'ERROR', 'CRITICAL')]


GOLDENS = {
    'logging/format.json': _cases,
    'logging/levels.json': _levels,
}

"""Kemono's pure helpers (WP5): post parsing, file naming, retry timing, creator URLs.

`parse_post` on well-formed and malformed listings (Python's exception class and text
for each), `sha256_from_path`, `attachment_filename`, the file and thumbnail URLs,
`retry_after_seconds` for header values whose answer does not depend on the clock,
`email.utils.parsedate_to_datetime` (as UTC, the way `retry_after_seconds` reads it), and
`extract_kemono_creator`.
"""

from __future__ import annotations

from dataclasses import asdict
from datetime import UTC
from email.utils import parsedate_to_datetime
from typing import Any

import httpx
from src.tool import kemono_creator
from src.web import kemono


def _attempt(call: Any) -> Any:
    try:
        return call()
    except Exception as exc:  # noqa: BLE001 - the error is the recorded answer
        return f'{exc.__class__.__name__}: {exc}'


POSTS: list[Any] = [
    {
        'id': 12441864,
        'user': 68870973,
        'service': 'fanbox',
        'title': '',
        'published': '2026-08-17T21:59:37',
        'file': {'name': 'main.jpeg', 'path': '/aa/bb/main-hash.jpeg', 'preview_only': 1},
        'attachments': [
            {'name': None, 'path': '/cc/dd/second-hash.png'},
            {'name': 'skipped.zip', 'path': ''},
            {'name': '', 'path': '/ee/ff/'},
            {'path': '/gg/hh/third.gif', 'preview_only': False},
        ],
    },
    {'id': 'a', 'user': 'b', 'service': 'c', 'title': 5, 'published': 0, 'file': None},
    {'id': 1.5, 'user': True, 'service': None, 'title': 'T', 'published': '', 'file': {}, 'attachments': []},
    {'id': 1, 'user': 2, 'service': 's', 'file': [], 'attachments': {}},
    {'id': 1, 'user': 2, 'service': 's', 'file': 'x'},
    {'id': 1, 'user': 2, 'service': 's', 'file': ['x']},
    {'id': 1, 'user': 2, 'service': 's', 'attachments': None},
    {'id': 1, 'user': 2, 'service': 's', 'attachments': [None]},
    {'id': 1, 'user': 2, 'service': 's', 'attachments': 'ab'},
    {'id': 1, 'user': 2, 'service': 's', 'attachments': ''},
    {'id': 1, 'user': 2, 'service': 's', 'attachments': 5},
    {'id': 1, 'user': 2, 'service': 's', 'file': {'path': 5}},
    {'id': 1, 'user': 2, 'service': 's', 'file': {'path': '/a/b.png', 'name': 5}},
    {'user': 2, 'service': 's'},
    {'id': 1, 'service': 's'},
    {'id': 1, 'user': 2},
    {'id': {'nested': [1]}, 'user': [1, 2], 'service': 's'},
    [],
    'post',
    None,
]

PATHS = [
    '/65/9e/' + '659e1483' * 8 + '.jpeg',
    '/aa/aa/' + 'A' * 64 + '.jpeg',
    '/aa/aa/' + 'a' * 63 + '.jpeg',
    '/aa/aa/' + 'a' * 65 + '.jpeg',
    '/aa/aa/' + 'g' * 64 + '.jpeg',
    '/aa/aa/' + 'a' * 64,
    '/aa/aa/' + 'a' * 64 + '.tar.gz',
    'not-a-hash.jpeg',
    '',
]

NAMES = [
    'cover.jpeg',
    '',
    'x' * 300 + '.png',
    'a/b:c*d?.txt',
    '..',
    '.hidden',
    'name.',
    '  spaced  .gif',
    '中文' * 40 + '.zip',
    'no-ext',
]

RETRY_AFTER = [
    None,
    '',
    '17',
    ' 17 ',
    '0',
    '\u0661\u0667',
    '²',
    '1.5',
    '-1',
    'garbage',
    'Mon, 17 Aug 2020 00:00:00 GMT',
    'Sun, 06 Nov 1994 08:49:37 GMT',
]

DATES = [
    'Mon, 17 Aug 2020 00:00:00 GMT',
    'Sun, 06 Nov 1994 08:49:37 GMT',
    'Sunday, 06-Nov-94 08:49:37 GMT',
    'Sun Nov  6 08:49:37 1994',
    '06 Nov 1994 08:49:37 +0530',
    '06 Nov 1994 08:49:37 -0000',
    '06 Nov 1994 08:49:37 EST',
    '06 Nov 1994 08:49:37 ADT',
    '06 Nov 1994 08:49:37 PDT',
    '06 Nov 1994 08:49:37 XYZ',
    '6 november 94 8:49 GMT',
    '06 Nov 49 08:49:37 GMT',
    '06 Nov 50 08:49:37 GMT',
    '06 Nov 0 08:49:37 GMT',
    '31 Feb 2020 00:00:00 GMT',
    '06 Nov 1994 25:00:00 GMT',
    '06 Nov 10000 00:00:00 GMT',
    '',
    'garbage',
    'Mon, 17 Aug 2020',
]

CREATORS = [
    '1' * 4300,
    '1' * 4301,
    '\u0661' * 4301,
    '70050825',
    '  70050825  ',
    '0',
    '000',
    '007',
    '\u0667\u0660',
    '²',
    '-5',
    'https://pawchive.pw/fanbox/user/70050825',
    'https://pawchive.pw/fanbox/user/70050825?o=50',
    'https://kemono.cr/patreon/user/9069743/posts',
    'https://pawchive.pw/Fantia/user/12345',
    'https://pawchive.pw/FANBOX/user/12345',
    'fanbox/user/70050825',
    'https://coomer.st/onlyfans/user/some-creator_1',
    'https://pawchive.pw/user/user/user',
    'https://pawchive.pw/fan-box/user/1/x/patreon/user/2',
    'https://pawchive.pw/fanbox//user//3',
    'https://pawchive.pw/fanbox/user/',
    'https://pawchive.pw/fanbox/user/id with spaces',
    'https://pawchive.pw/fanbox/user/%41',
    'https://[::1/fanbox/user/1',
    'maplestar',
    '',
    '   ',
]


def _post(data: Any) -> Any:
    return _attempt(lambda: asdict(kemono.parse_post(data)))


def _retry_after(value: str | None) -> Any:
    # Header bytes as a server sends them; httpx decodes them as UTF-8.
    response = None if value is None else httpx.Response(429, headers={} if value == '' else {'Retry-After': value.encode()})
    return _attempt(lambda: kemono.retry_after_seconds(response))


def _parsedate(value: str) -> Any:
    try:
        parsed = parsedate_to_datetime(value)
    except (TypeError, ValueError):
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=UTC)
    return parsed.astimezone(UTC).isoformat()


def _parsing() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = [{'parse_post': data, 'result': _post(data)} for data in POSTS]
    cases.extend({'sha256_from_path': path, 'result': kemono.sha256_from_path(path)} for path in PATHS)
    cases.extend({'attachment_filename': name, 'result': kemono.attachment_filename(name, 3)} for name in NAMES)
    cases.extend(
        {
            'base': base,
            'file_url': kemono.build_file_url(base, '/aa/bb/hash.jpeg'),
            'thumbnail_url': kemono.build_thumbnail_url(base, '/aa/bb/hash.jpeg'),
        }
        for base in ('https://file.pawchive.pw', 'https://file.pawchive.pw//', '')
    )
    cases.extend({'retry_after': value, 'result': _retry_after(value)} for value in RETRY_AFTER)
    cases.extend({'parsedate': value, 'result': _parsedate(value)} for value in DATES)
    for raw in CREATORS:
        result = _attempt(lambda raw=raw: kemono_creator.extract_kemono_creator(raw))
        cases.append({'creator': raw, 'result': list(result) if isinstance(result, tuple) else result})
    return cases


GOLDENS = {
    'kemono/parsing.json': _parsing,
}

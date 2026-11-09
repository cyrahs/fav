"""`src/tool/filename.py`: every file fav writes is named through these (WP1)."""

from __future__ import annotations

from typing import Any

from src.tool.filename import format_media_filename, sanitize

SANITIZE_CASES: list[tuple[str, int]] = [
    ('plain title', 200),
    ('  padded  ', 200),
    ('a<b>c:d"e/f\\g|h?i*j\nk', 200),
    ('tab\tand\rreturn stay', 200),
    ('\n\n', 200),
    ('中文标题：测试/斜杠', 200),  # noqa: RUF001 - the full-width colon is the point
    ('x' * 250, 200),
    ('中' * 100, 200),
    ('中' * 100, 50),
    ('ab中', 4),
    ('ab中', 5),
    ('😀' * 60, 200),
    ('😀ab', 3),
    ('trailing space after cut   x', 22),
    ('', 200),
    ('   ', 200),
    ('a' * 10, 0),
]

MEDIA_CASES: list[dict[str, Any]] = [
    {'title': 'Video', 'media_id': 'BV1xx411c7mD', 'uploader': 'uploader', 'ext': 'mp4'},
    {'title': 'Video', 'media_id': 'BV1xx411c7mD', 'uploader': None, 'ext': '.mkv'},
    {'title': 'Video', 'media_id': '123', 'uploader': '', 'ext': 'jpg'},
    {'title': 'a/b:c', 'media_id': 'id?*', 'uploader': 'up|loader', 'ext': 'mp4'},
    {'title': '标题' * 80, 'media_id': '42', 'uploader': '作者' * 40, 'ext': 'mp4'},
    {'title': 'x' * 300, 'media_id': 'y' * 150, 'uploader': 'z' * 150, 'ext': 'webp'},
    {'title': 'short', 'media_id': '7', 'uploader': None, 'ext': 'mp4', 'max_total_bytes': 10},
    {'title': '😀' * 70, 'media_id': '😀', 'uploader': None, 'ext': 'gif'},
    {'title': '  spaced  ', 'media_id': ' 9 ', 'uploader': ' u ', 'ext': 'png'},
]


def _sanitize() -> list[dict[str, Any]]:
    return [{'name': name, 'max_bytes': max_bytes, 'result': sanitize(name, max_bytes=max_bytes)} for name, max_bytes in SANITIZE_CASES]


def _media() -> list[dict[str, Any]]:
    return [{'args': case, 'result': format_media_filename(**case)} for case in MEDIA_CASES]


GOLDENS = {
    'filename/sanitize.json': _sanitize,
    'filename/format_media_filename.json': _media,
}

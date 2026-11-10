"""`src/web/pixiv.py` (WP6): the ajax payload readers, the session cookie's user id and
the output paths, over answers chosen to hit their branches beyond tests/test_pixiv.py.
"""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace
from typing import Any

from src.web import pixiv
from src.web.pixiv import PixivWork, derive_user_id, infer_extension, parse_bookmark_works, parse_page_urls, parse_ugoira_meta


def _entry(**updates: Any) -> dict[str, Any]:
    entry: dict[str, Any] = {
        'id': '96461036',
        'title': 'a title',
        'userId': '50944445',
        'userName': 'the artist',
        'illustType': 0,
        'pageCount': 1,
        'isMasked': False,
    }
    entry.update(updates)
    return entry


BOOKMARK_CASES: list[Any] = [
    {'works': [_entry(), _entry(id='2', illustType=2, title='anim'), _entry(id='3', illustType=1, pageCount='4')]},
    {'works': [{'id': 130954211, 'title': '-----', 'isMasked': True}]},
    {'works': [_entry(illustType=7), _entry(id='5'), _entry(id='6', illustType=None), _entry(id='7', illustType='1')]},
    {'works': [_entry(id=True), _entry(id=1.5), _entry(id=' 42 '), _entry(id=''), _entry(id=None), _entry(id='x1')]},
    {
        'works': [
            _entry(id='8', isMasked=1),
            _entry(id='9', isMasked='yes', illustType=9),
            _entry(id='10', isMasked=0, illustType=9),
        ],
    },
    {
        'works': [
            _entry(id='11', pageCount=0),
            _entry(id='12', pageCount=None),
            _entry(id='13', pageCount='0'),
            _entry(id='14', pageCount=-2),
        ]
    },
    {'works': [_entry(id='15', title=None, userName=0, userId=12345), _entry(id='16', title=0, userName='', userId='')]},
    {'works': [_entry(id='17', title=['list'], userName={'a': 1}), 'not a dict', None, 5]},
    {'works': [_entry(id='18', illustType=True), _entry(id='19', illustType=2.0)]},
    {'works': [_entry(id='²')]},
    {'works': []},
    {'works': None},
    {'works': 'text'},
    {},
    {'total': 3},
]

PAGE_URL_CASES: list[Any] = [
    [
        {'urls': {'original': 'https://i.pximg.net/img-original/img/x/1_p0.jpg'}},
        {'urls': {'original': 'https://i.pximg.net/img-original/img/x/1_p1.png'}},
    ],
    [{'urls': {'original': ''}}, {'urls': {}}, {'urls': None}, {}, None, 'x', {'urls': {'original': 'kept'}}],
    [{'urls': {'original': 5}}, {'urls': {'original': None}}, {'urls': 'text'}],
    None,
    {},
    {'urls': {'original': 'not in a list'}},
    'text',
    [],
]

UGOIRA_CASES: list[Any] = [
    {
        'originalSrc': 'https://i.pximg.net/img-zip-ugoira/img/x/2_ugoira1920x1080.zip',
        'frames': [{'file': '000000.jpg', 'delay': 45}, {'file': '000001.jpg', 'delay': 90}],
    },
    {
        'originalSrc': 'z.zip',
        'frames': [
            {'file': 'a.jpg', 'delay': 0},
            {'file': 'b.jpg', 'delay': -5},
            {'file': '', 'delay': 10},
            {'file': None, 'delay': 10},
            {'file': 'c.jpg', 'delay': '30'},
            {'file': 'd.jpg', 'delay': None},
            {'file': 'e.jpg', 'delay': True},
            {'file': 'f.jpg', 'delay': 1.5},
            'not a dict',
            {'file': 7, 'delay': 20},
        ],
    },
    {'originalSrc': '', 'frames': []},
    {'originalSrc': 'z.zip', 'frames': []},
    {'originalSrc': '', 'frames': [{'file': 'a.jpg', 'delay': 10}]},
    {'originalSrc': None, 'frames': None},
    {'originalSrc': 'z.zip', 'frames': 'text'},
    {'originalSrc': 12, 'frames': [{'file': 'a.jpg', 'delay': 10}]},
    {},
]

EXTENSION_CASES: list[str] = [
    'https://i.pximg.net/img-original/img/x/1_p0.png',
    'https://i.pximg.net/img-original/img/x/1_p0.JPG',
    'https://i.pximg.net/no-extension',
    'https://i.pximg.net/dir.v2/file',
    'https://i.pximg.net/a.tar.gz?x=1.png#y.gif',
    'https://i.pximg.net/trailing.',
    'https://i.pximg.net/.hidden',
    'https://i.pximg.net/',
    'https://i.pximg.net',
    '',
    'relative/path.WEBP',
    'https://i.pximg.net/%2E%2E/a.Png',
    'https://[broken/a.png',
]

USER_ID_CASES: list[str] = [
    '21029043_AbCdEf0123456789AbCdEf0123456789',
    '21029043_',
    '_abc',
    'no-underscore',
    'prefix_notdigits',
    '',
    '١٢٣_arabic-indic',
    '²_superscript',
    ' 123_space',
    '123_456_789',
]

WORK_CASES: list[dict[str, Any]] = [
    {'illust_type': 0, 'num': 1, 'ext': 'png'},
    {'illust_type': 2, 'num': 0, 'ext': 'webp'},
    {'illust_type': 1, 'num': 3, 'ext': '.jpg'},
    {'author': '', 'author_id': '42', 'num': 0, 'ext': 'jpg'},
    {'author': '', 'author_id': '', 'num': 0, 'ext': 'jpg'},
    {'author': '???', 'author_id': '43', 'num': 0, 'ext': 'jpg'},
    {'author': '  ', 'author_id': '44', 'num': 0, 'ext': 'jpg'},
    {'author': 'a/b:c', 'title': 'x/y:z*?', 'num': 0, 'ext': 'gif'},
    {'title': '', 'num': 0, 'ext': 'png'},
    {'title': '中' * 120, 'author': '作者' * 50, 'num': 12, 'ext': 'png'},
    {'title': 'emoji 😀' * 40, 'num': 0, 'ext': 'jpg'},
]


def _outcome(call: Any) -> dict[str, Any]:
    try:
        return {'result': call()}
    except Exception as exc:  # noqa: BLE001 - the error is the recorded answer
        return {'error': [type(exc).__name__, str(exc)]}


def _work_fields(work: PixivWork) -> list[Any]:
    return [work.illust_id, work.title, work.author, work.author_id, work.illust_type, work.page_count, work.masked]


def _bookmarks() -> list[dict[str, Any]]:
    return [
        {'body': body, **_outcome(lambda body=body: [_work_fields(work) for work in parse_bookmark_works(body)])} for body in BOOKMARK_CASES
    ]


def _page_urls() -> list[dict[str, Any]]:
    return [{'body': body, **_outcome(lambda body=body: parse_page_urls(body))} for body in PAGE_URL_CASES]


def _ugoira() -> list[dict[str, Any]]:
    def parse(body: Any) -> list[Any]:
        zip_url, frames = parse_ugoira_meta(body)
        return [zip_url, [list(frame) for frame in frames]]

    return [{'body': body, **_outcome(lambda body=body: parse(body))} for body in UGOIRA_CASES]


def _extensions() -> list[dict[str, Any]]:
    return [{'url': url, **_outcome(lambda url=url: infer_extension(url))} for url in EXTENSION_CASES]


def _user_ids() -> list[list[str]]:
    return [[cookie, derive_user_id(cookie)] for cookie in USER_ID_CASES]


def _output_paths() -> list[dict[str, Any]]:
    source = pixiv.Pixiv.__new__(pixiv.Pixiv)
    source.cfg = SimpleNamespace(path=Path('/data/pixiv'))
    cases = []
    for case in WORK_CASES:
        fields = {
            'illust_id': 96461036,
            'title': 'a title',
            'author': 'the artist',
            'author_id': '50944445',
            'illust_type': 0,
            'page_count': 1,
            'masked': False,
        }
        fields.update({key: value for key, value in case.items() if key not in {'num', 'ext'}})
        work = PixivWork(**fields)
        path = source._build_output_path(work, num=case['num'], ext=case['ext'])  # noqa: SLF001
        cases.append({'work': _work_fields(work), 'num': case['num'], 'ext': case['ext'], 'path': str(path)})
    return cases


GOLDENS = {
    'pixiv/bookmarks.json': _bookmarks,
    'pixiv/page_urls.json': _page_urls,
    'pixiv/ugoira_meta.json': _ugoira,
    'pixiv/extensions.json': _extensions,
    'pixiv/user_ids.json': _user_ids,
    'pixiv/output_paths.json': _output_paths,
}

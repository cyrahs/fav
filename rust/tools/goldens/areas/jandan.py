"""Jandan's pure helpers (WP5): the `data` field cipher, the date cursor and the parsers.

`encrypt_data_field`/`decrypt_data_field` (AES-256-CBC, PKCS#7, base64, URL quoting, and
every failure on the way with Python's class and text), `zulu_to_offset` (Python 3.12's
`datetime.fromisoformat`, ported from CPython), `_to_int`, and the URL and payload
parsers the crawl uses.
"""

from __future__ import annotations

from dataclasses import asdict
from typing import Any

from src.web import jandan


def _error(exc: Exception) -> str:
    return f'{exc.__class__.__name__}: {exc}'


def _attempt(call: Any) -> Any:
    try:
        return call()
    except Exception as exc:  # noqa: BLE001 - the error is the recorded answer
        return _error(exc)


PLAINTEXTS = [
    '',
    '{"fav":{"favType":"1","favNumLimit":45,"actionType":"get","userID":42920}}',
    'exactly sixteen!',
    '中文 and emoji 🎉',
    'x' * 47,
]

CIPHERTEXTS = [
    '',
    'a',
    'ab',
    'QUJD',
    'QUJDREVGR0hJSktMTU5PUA',
    'QUJDREVGR0hJSktMTU5PUA%3D%3D',
    'not base64!!',
    'é',
    '%C3%A9',
    'QUJD%ZZ',
    'QU=JD',
    '=QUJD',
    'QUJ=====',
    'Q===',
    'QUJDREVGR0hJSktMTU5PUA==trailing',
    '  QUJD REVG\nR0hJ\tSktM TU5PUA==  ',
    # Valid base64 of whole blocks whose padding is wrong once decrypted.
    'AAAAAAAAAAAAAAAAAAAAAA==',
    'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=',
]


def _cipher() -> Any:
    return jandan.AES.new(jandan._JD_KEY, jandan.AES.MODE_CBC, iv=jandan._JD_IV)  # noqa: SLF001 - the module's fixed key


def _crypto() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for plain in PLAINTEXTS:
        encrypted = jandan.encrypt_data_field(plain)
        cases.append({'encrypt': plain, 'result': encrypted, 'roundtrip': jandan.decrypt_data_field(encrypted)})
    # A plaintext that is not UTF-8 once decrypted.
    raw = _cipher().encrypt(jandan._pkcs7_pad(b'\xff\xfe ok'))  # noqa: SLF001 - the module's own padding
    cases.append({'decrypt': jandan.quote(jandan.base64.b64encode(raw).decode(), safe=''), 'result': None})
    # Padding bytes that disagree with the pad length, and a pad length beyond a block.
    for padded in (b'0123456789abcd\x01\x02', b'0123456789abcde\x11', b'0123456789abcde\x00'):
        raw = _cipher().encrypt(padded)
        cases.append({'decrypt': jandan.base64.b64encode(raw).decode(), 'result': None})
    cases.extend({'decrypt': text, 'result': None} for text in CIPHERTEXTS)
    for case in cases:
        if 'decrypt' in case:
            case['result'] = _attempt(lambda text=case['decrypt']: jandan.decrypt_data_field(text))
    return cases


DATES = [
    '2026-02-14T02:06:39.000Z',
    '2026-02-14T02:06:39Z',
    '2026-02-14T02:06:39.123456Z',
    '2026-02-14T02:06:39.1234567Z',
    '2026-02-14T02:06:39,5Z',
    '2026-02-14T02:06:39.5+08:00',
    '2026-02-14T02:06:39-05:30',
    '2026-02-14T02:06:39+05:30:15.25',
    '2026-02-14T02:06:39+0530',
    '2026-02-14T02:06:39+05',
    '2026-02-14 02:06:39Z',
    '2026-02-14x02:06Z',
    '2026-02-14T02Z',
    '2026-02-14T0206Z',
    '2026-02-14T020639Z',
    '20260214T020639Z',
    '2026-02-14',
    '20260214',
    '2026-02-14T02:06:39',
    '2026-W07-6T02:06:39Z',
    '2026W076',
    '2026W076T02:06:39Z',
    '2026-W07T02:06:39Z',
    '2026W07T02:06:39Z',
    '2026-W07',
    '2026W07',
    '2026-W53-1',
    '2020-W53-1',
    '2026-W00-1',
    '2026-W07-0',
    '2026-W07-8',
    '9999-W52-6',
    '9999-W52-5',
    '0001-W01-1',
    '0000-W01-1',
    '2026-W',
    '2026-W07-',
    '2026-W07-12:00',
    '2026-W07x12',
    '2026W0761200',
    '2026W07612:00',
    '2026W07-6',
    '2026-W076',
    '2026-W07-6X02',
    '2026-Wab-1',
    '2026-W07-a',
    '2026W',
    '2026Wxx',
    '2026-W1',
    '2026-07',
    '2026-07-1',
    '0001-01-01T07:59:59Z',
    '0001-01-01T08:00:00Z',
    '9999-12-31T23:59:59Z',
    '2026-02-30T00:00:00Z',
    '2026-13-01T00:00:00Z',
    '0000-01-01T00:00:00Z',
    '2026-02-14T24:00:00Z',
    '2026-02-14T23:60:00Z',
    '2026-02-14T23:59:60Z',
    '2026-02-14T02:06:39+24:00',
    '2026-02-14T02:06:39-23:59:59.999999',
    '2026-02-14T02:06:39ZZ',
    '2026-02-14T02:06:39.Z',
    '2026-02-14T02:06:39.000Zjunk',
    '2026-02-14T02:06:39 ',
    '2026-02-14T2:06:39Z',
    '2026-2-14T02:06:39Z',
    '',
    'Z',
    'not a date',
    '\uff12\uff10\uff12\uff16-02-14T02:06:39Z',  # fullwidth digits
    '2026-02-14T02:06:39+08:00Z',
]


def _zulu_to_offset() -> list[dict[str, Any]]:
    return [{'date': date, 'result': _attempt(lambda date=date: jandan.zulu_to_offset(date))} for date in DATES]


INTS = [0, 7, -3, 2.9, -2.9, True, None, '42', ' 42 ', '-1', '٤٢', '²', '4.2', '', [1], {'a': 1}]


def _image(image: jandan.JandanImage) -> dict[str, Any]:
    return asdict(image)


PICS = [
    [
        {
            'id': 6095815,
            'date': '2026-02-25T02:14:46.000Z',
            'author': 'author-a',
            'contents': [
                {'contentType': 'image', 'content': ' http://img.wangmoyu.com/mw600/demo-a.jpg ', 'md5': ' abc ', 'imageType': 'JPEG'},
                {'contentType': 'video', 'content': 'http://img.wangmoyu.com/demo.mp4'},
                {'contentType': 'IMAGE', 'content': '//img.wangmoyu.com/mw600/demo-b.png', 'md5': '', 'imageType': ''},
                {'contentType': 'image', 'content': '   '},
                {'contentType': 'image', 'content': 5},
                'not a dict',
            ],
        },
        {'id': '6095816', 'date': '', 'author': '', 'contents': [{'contentType': 'image', 'content': 'https://x/a.gif'}]},
        {'id': 'abc', 'contents': [{'contentType': 'image', 'content': 'https://x/skipped.gif'}]},
        {'id': 1.9, 'author': 7, 'date': 0, 'contents': [{'contentType': 'image', 'content': 'https://x/float-id.gif'}]},
        {'id': 3, 'contents': 'not a list'},
        {'id': True, 'contents': [{'contentType': 'image', 'content': 'https://x/bool-id.gif'}]},
    ],
    [{'id': '²', 'contents': []}],
]

FAVS = [
    (1, {'pics': [{'id': 1}, 'skip', {'id': 2}]}),
    (2, {'girls': [{'id': 1}], 'pics': [{'id': 2}]}),
    (6, {'nzs': 'not a list', 'pics': [{'id': 3}]}),
    (6, {'other': [{'id': 4}], 'more': [{'id': 5}]}),
    (9, {'meta': {'a': 1}}),
    (1, {}),
]

IMAGE_URLS = [
    'http://wx1.sinaimg.cn/mw600/abc.jpg',
    'http://wx4.sinaimg.cn/mw1024/abc.jpg?x=1&y=2',
    'http://img.wangmoyu.com/mw600/mw600/abc.png',
    'http://WX1.SINAIMG.CN/MW600/abc.jpg',
    '//tvax3.sinaimg.cn/mw2000/abc.jpg#frag',
    'https://sinaimg.cn/mw600/abc.jpg',
    'https://cdn.example.com/mw600/abc.jpg',
    'https://img.wangmoyu.com/large/abc.jpg',
    'https://notwangmoyu.com/mw600/abc.jpg',
    'https://x.sinaimg.cn.evil/mw600/abc.jpg',
    'http://wx5.sinaimg.cn/large/abc.jpg',
    'http://wx12.sinaimg.cn/large/abc.jpg',
    'http://Wx2.Sinaimg.cn:8080/large/abc.jpg',
    'http://[::1/abc.jpg',
    'not a url',
    '',
]

EXTENSIONS = [
    ('https://img.example.com/path/abc.unknown', 'jpeg'),
    ('https://img.example.com/path/abc.gif?token=1', None),
    ('https://img.example.com/path/no-ext', None),
    ('https://img.example.com/path/ABC.WEBP', ''),
    ('https://img.example.com/path/abc.png', ' PNG '),
    ('https://img.example.com/path/abc.png', 'tiff'),
    ('https://img.example.com/path/abc.jpeg/', None),
    ('https://img.example.com/', None),
]


def _parsing() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = [{'to_int': value, 'result': _attempt(lambda value=value: jandan._to_int(value))} for value in INTS]  # noqa: SLF001
    cases.extend(
        {
            'extract_pic_images': pics,
            'result': _attempt(lambda pics=pics: [_image(image) for image in jandan.extract_pic_images(fav_type=1, pics=pics)]),
            'last_date': jandan.extract_last_pic_date(pics),
        }
        for pics in PICS
    )
    cases.extend(
        {'extract_fav_items': fav, 'fav_type': fav_type, 'result': jandan.extract_fav_items(fav_type=fav_type, fav=fav)}
        for fav_type, fav in FAVS
    )
    cases.extend(
        {
            'url': url,
            'candidates': _attempt(lambda url=url: jandan.build_image_download_candidates(url)),
            'shards': _attempt(lambda url=url: jandan.build_sinaimg_shard_candidates(url)),
            'headers': _attempt(lambda url=url: list(jandan.build_image_request_headers(url).items())),
        }
        for url in IMAGE_URLS
    )
    cases.extend(
        {
            'extension': url,
            'image_type': image_type,
            'result': _attempt(lambda url=url, image_type=image_type: jandan.infer_image_extension(url, image_type)),
        }
        for url, image_type in EXTENSIONS
    )
    return cases


GOLDENS = {
    'jandan/crypto.json': _crypto,
    'jandan/zulu_to_offset.json': _zulu_to_offset,
    'jandan/parsing.json': _parsing,
}

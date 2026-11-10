"""`src/web/rednote.py` and `src/web/rednote_browser.py` (WP17): the pure parts.

- `browser_helpers.json`: `describe_shape`, `redact_probe`, `summarize_pending`,
  `cursor_of` and `_url_head` of the browser module.
- `parsers.json`: the source's parsers over the API's snake_case and the page's
  camelCase payloads: `_to_int`, `format_published_at`, `infer_image_extension`,
  `normalize_media_url`, `pick_image_url`, `pick_video_url` with `_best_video_stream`,
  `note_source_long_edge`, `build_note_url`, `build_media_filename`,
  `parse_api_envelope` (its errors as class and message), `extract_like_page`,
  `extract_note_media` and `decide_login_state`.
- `proxy_probe.json`: `probe_proxy` over proxy strings it refuses before dialing (the
  result as a dict, or the error it raises), which is where httpx's own wording of an
  unusable proxy URL shows.
"""

from __future__ import annotations

import dataclasses
from typing import Any

from src.web import rednote, rednote_browser

_NOTE_TIME_MS = 1754969696000

_SHAPE_VALUES: list[Any] = [
    None,
    True,
    0,
    1.5,
    '',
    '我的私人笔记',
    [],
    [1, 'a'],
    {},
    {'a': {'b': {'c': {'d': {'e': {'f': 1}}}}}},
    {str(index): index for index in range(45)},
    {'notes': [{'note_id': '64f1a2b3000000001e02c0de', 'tags': [], 'cover': {'url': 'x', 'w': 3}}], 'has_more': False},
    [[[[[[1]]]]]],
]

_PROBES: list[dict[str, Any]] = [
    {},
    {'logged_in': True, 'user_id': '5ff', 'profile_href': '/user/profile/5ff', 'qr_src': ''},
    {'logged_in': False, 'qr_src': 'data:image/png;base64,' + 'A' * 70, 'modal_html': '<div>x</div>', 'qr_stage': 'login'},
    {'user_id': None, 'qr_src': 0, 'profile_href': True, 'guest': True},
    {'has_login_modal': True, 'has_verify_modal': False, 'user_id': '小红书'},
]

_PENDING: list[tuple[list[tuple[str, str, float]], float, int]] = [
    ([], 0.0, 8),
    ([('document', 'https://www.xiaohongshu.com/explore?x=1', 1.0)], 31.0, 8),
    ([('xhr', 'https://edith.xiaohongshu.com/api/sns/web/v1/note/like/page?cursor=abc', 7.0), ('script', 'https://a/b/c', 2.0)], 9.25, 1),
    ([('other', 'about:blank', 0.0), ('fetch', 'data:text/plain,hello-there-this-is-long-enough-to-be-cut-off', 0.5)], 0.75, 8),
    ([(f'image{index}', f'https://cdn{index}.example/x/y', float(index)) for index in range(10)], 20.0, 8),
    ([('xhr', 'https://h/a', 1.0), ('xhr', 'https://h/b', 1.0)], 1.05, 8),
]

_URLS: list[str] = [
    'https://edith.xiaohongshu.com/api/sns/web/v1/note/like/page?num=30&cursor=C1',
    'https://edith.xiaohongshu.com/api/sns/web/v1/note/like/page?num=30',
    'https://edith.xiaohongshu.com/api/sns/web/v1/note/like/page?cursor=&num=30',
    'https://e/x?cursor=a%2Bb&cursor=second',
    'https://e/x?cursor=a+b',
    'https://e/x?cursor',
    '/relative/path?cursor=r',
    'about:blank',
    'https://www.xiaohongshu.com',
    'https://www.xiaohongshu.com/',
    'https://www.xiaohongshu.com//explore/abc',
    'not a url at all, just text that runs past forty characters',
]


def _browser_helpers() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = [
        {'function': 'describe_shape', 'value': value, 'depth': depth, 'result': rednote_browser.describe_shape(value, depth=depth)}
        for value in _SHAPE_VALUES
        for depth in (5, 1, 0)
    ]
    cases += [{'function': 'redact_probe', 'probe': probe, 'result': rednote_browser.redact_probe(probe)} for probe in _PROBES]
    cases += [
        {
            'function': 'summarize_pending',
            'entries': [list(entry) for entry in entries],
            'now': now,
            'limit': limit,
            'result': rednote_browser.summarize_pending(entries, now=now, limit=limit),
        }
        for entries, now, limit in _PENDING
    ]
    cases += [{'function': 'cursor_of', 'url': url, 'result': rednote_browser.cursor_of(url)} for url in _URLS]
    cases += [{'function': 'url_head', 'url': url, 'result': rednote_browser._url_head(url)} for url in _URLS]  # noqa: SLF001
    return cases


def _call(function: Any, *args: Any, **kwargs: Any) -> dict[str, Any]:
    try:
        return {'result': function(*args, **kwargs)}
    except Exception as exc:  # noqa: BLE001 - the error is the answer
        error = {'class': type(exc).__name__, 'message': str(exc)}
        if isinstance(exc, rednote.RedNoteError):
            error['dedupe_key'] = exc.notification_dedupe_key
        return {'error': error}


def _media_rows(items: list[rednote.RedNoteMedia]) -> list[dict[str, Any]]:
    return [dataclasses.asdict(item) for item in items]


def _image_card(**updates: Any) -> dict[str, Any]:
    card: dict[str, Any] = {
        'type': 'normal',
        'title': ' a caption ',
        'time': _NOTE_TIME_MS,
        'user': {'nickname': 'artist', 'user_id': '5ff0000000000000000fffff'},
        'image_list': [
            {
                'info_list': [
                    {'image_scene': 'WB_PRV', 'url': 'https://sns-webpic.xhscdn.com/1!nd_prv_wlteh_webp_3'},
                    {'image_scene': 'wb_dft ', 'url': 'https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3'},
                ],
            },
        ],
    }
    card.update(updates)
    return card


def _page_card(**updates: Any) -> dict[str, Any]:
    card: dict[str, Any] = {
        'type': 'normal',
        'title': 'a caption',
        'time': str(_NOTE_TIME_MS),
        'user': {'nickname': '', 'userId': 'u'},
        'imageList': [
            {
                'infoList': [{'imageScene': 'WB_PRV', 'url': 'http://sns-webpic.xhscdn.com/p'}, {'imageScene': 'WB_DFT', 'url': ' '}],
                'urlDefault': 'http://sns-webpic.xhscdn.com/default',
                'stream': {'EF4': [{'masterUrl': 'http://cdn/live.mp4'}]},
            },
            'not-a-dict',
            {'urlPre': 'http://cdn/pre-only', 'infoList': 'not-a-list'},
            {'stream': {'h264': [{'master_url': 'https://cdn/clip-only.mp4', 'width': '720', 'height': 1280}]}},
            {},
        ],
    }
    card.update(updates)
    return card


_STREAMS: list[Any] = [
    None,
    {},
    [],
    {'EF4': None, 'EF5': 'x', 'EF6': [None, 3, {}]},
    {
        'EF4': [{'width': 720, 'height': 1280, 'videoBitrate': 3_000_000, 'masterUrl': 'https://cdn/720.mp4'}],
        'EF5': [
            {'width': 2160, 'height': 3840, 'videoBitrate': 9_000_000, 'masterUrl': 'https://cdn/2160.mp4'},
            {'width': 1080, 'height': 1920, 'videoBitrate': 1_700_000, 'masterUrl': 'https://cdn/1080.mp4'},
        ],
    },
    {
        'EF5': [
            {'width': 1080, 'height': 1920, 'videoBitrate': 2, 'masterUrl': 'https://cdn/low'},
            {'width': 1080, 'height': 1920, 'videoBitrate': 6, 'masterUrl': 'https://cdn/high'},
        ]
    },
    {
        'EF5': [
            {'width': 1080, 'height': 1920, 'videoBitrate': 6, 'masterUrl': 'https://cdn/first'},
            {'width': 1080, 'height': 1920, 'videoBitrate': 6, 'masterUrl': 'https://cdn/tie'},
        ]
    },
    {'EF7': [{'masterUrl': 'https://cdn/only.mp4'}, {'masterUrl': ''}]},
    {'h264': [{'width': 720.9, 'height': '1280', 'video_bitrate': '5', 'master_url': 'http://cdn/legacy.mp4'}]},
    {'a': [{'width': True, 'height': 5, 'masterUrl': 'https://cdn/bool'}, {'width': -1, 'height': -1, 'masterUrl': 'https://cdn/neg'}]},
]


def _each(function: str, call: Any, arguments: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """One case per argument set: the arguments by name, then Python's answer."""
    return [{'function': function, **arguments_, **_call(call, *arguments_.values())} for arguments_ in arguments]


def _like_page_result(data: Any) -> list[Any]:
    notes, cursor, has_more = rednote.extract_like_page(data)
    return [[dataclasses.asdict(note) for note in notes], cursor, has_more]


def _note_media_rows(card: Any) -> list[dict[str, Any]]:
    return _media_rows(rednote.extract_note_media(card, note_id='n1', xsec_token='T'))  # noqa: S106 - a note token, not a password


_MEDIA_BASE: dict[str, Any] = {
    'note_id': '64f1a2b3000000001e02c0de',
    'media_index': 1,
    'media_type': 'image',
    'media_url': 'https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3',
    'title': 'a caption',
    'author': 'artist',
    'author_id': '5ff',
    'note_type': 'normal',
    'published_at': '2025-08-12 11:34:56',
    'xsec_token': 'TOKEN',
}

_VIDEO_CARD: dict[str, Any] = {
    'type': 'video',
    'title': 'a clip',
    'time': _NOTE_TIME_MS,
    'user': {'nickname': ' artist ', 'user_id': 'u'},
    'image_list': [{'info_list': [{'image_scene': 'WB_DFT', 'url': 'https://cover'}]}],
    'video': {'media': {'stream': _STREAMS[4]}, 'mediaV2': '{"video": {"width": 2160, "height": 3840}}'},
}


def _parsers() -> list[dict[str, Any]]:
    to_int_values = [None, True, 0, 12, -3, 2.9, -2.9, '12', ' 7 ', '-4', '+4', '1.5', '', '٣', [], 1e18]
    # Past year 9999 each limit `fromtimestamp` meets raises its own error: UTC+8 only, UTC,
    # `gmtime`'s `int` year, then `time_t`.
    out_of_range = [
        *(253402272000000, 253402300800000, 10**18),
        *(67767976233532799000, 67767976233532800000, 67768036191676780000, 67768036191676800000),
        *(10**20, 10**30, 9223372036854775807000),
    ]
    times = [_NOTE_TIME_MS, str(_NOTE_TIME_MS), 0, -1, None, 'x', 1.5e12, 1, 253402300799000, *out_of_range]
    image_urls = [
        'https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_webp_3',
        'https://sns-webpic.xhscdn.com/1!nd_dft_wlteh_avif_3',
        'https://sns-webpic.xhscdn.com/photo.jpeg',
        'https://sns-webpic.xhscdn.com/photo.PNG?x=1',
        'https://sns-webpic.xhscdn.com/opaque',
        'https://sns-webpic.xhscdn.com/opaque?format=heic',
        'https://sns-webpic.xhscdn.com/a.b/c',
        'https://sns-webpic.xhscdn.com/x.tiff',
        'https://sns-webpic.xhscdn.com/gif',
        '',
    ]
    media_urls = ['http://cdn/x', ' https://cdn/x ', '', '   ', 'HTTP://cdn/x', 'http://cdn/x?q=1#f', '//cdn/x']
    image_entries = [
        None,
        {},
        {'info_list': [{'image_scene': 'WB_PRV', 'url': 'http://a'}, {'image_scene': 'WB_DFT', 'url': 'http://b'}]},
        {'infoList': [{'imageScene': 'WB_DFT', 'url': ''}, {'imageScene': 'X', 'url': 'http://c'}], 'urlDefault': 'http://d'},
        {'infoList': [], 'urlDefault': '', 'urlPre': 'http://e'},
        {'infoList': [None, 'x', {'url': None}], 'urlDefault': None},
        {'info_list': [{'image_scene': None, 'url': 'http://f'}]},
    ]
    video_blocks = [
        None,
        {},
        {'mediaV2': '{"video": {"width": 2160, "height": 3840}}'},
        {'media_v2': {'video': {'width': 1080, 'height': 1920}}},
        {'mediaV2': 'not json'},
        {'mediaV2': '[1, 2]'},
        {'mediaV2': {'video': {'width': '4096'}}},
        {'mediaV2': None, 'media_v2': {'video': {'height': 720.5}}},
    ]
    media_updates = [
        ({}, ''),
        ({'media_type': 'video', 'media_url': ''}, ''),
        ({'media_type': 'live', 'media_index': 3}, ''),
        ({'author': 'a/b:c*?', 'published_at': ''}, ''),
        ({'author': ''}, 'png'),
        ({'author': '很长的名字' * 40}, ''),
        ({'media_url': 'https://cdn/x.jpeg'}, ''),
    ]
    payloads = [
        {'code': 0, 'data': {'notes': []}},
        {'code': 0, 'data': None},
        {'data': {'x': 1}},
        {'code': '0', 'data': {}},
        {'code': -101, 'msg': 'no login'},
        {'code': -100, 'message': ' expired '},
        {'code': '-100'},
        {'code': -510001, 'msg': ''},
        {'code': 300013, 'msg': '访问频次异常'},
        {'code': True, 'data': {}},
        {'code': 1.0},
        'not an object',
        None,
        [],
    ]
    like_pages = [
        {},
        {
            'notes': [{'note_id': 'a', 'xsec_token': ' T '}, {'id': 'b'}, {'xsec_token': 'x'}, 'x', {'note_id': ' ', 'id': 'c'}],
            'cursor': ' c2 ',
            'has_more': 1,
        },
        {'notes': 'x', 'cursor': None, 'has_more': 'false'},
        {'notes': [{'note_id': 5, 'xsec_token': None}], 'cursor': 7, 'has_more': []},
    ]
    cards = [
        _image_card(),
        _image_card(time=None, user=None, title=None),
        _image_card(image_list='x'),
        _page_card(),
        _VIDEO_CARD,
        {**_VIDEO_CARD, 'video': {}},
        {**_VIDEO_CARD, 'video': None},
        {'type': ' normal ', 'imageList': [{'urlDefault': 'http://cdn/only-default'}], 'user': {'nickname': None, 'userId': 7}},
        {},
    ]
    logins = [
        ({'has_login_modal': True}, {'web_session': 'still-here'}),
        ({'logged_in': True}, {}),
        ({'logged_in': False}, {}),
        ({}, {'web_session': 'v'}),
        ({}, {'web_session': '  '}),
        ({'qr_src': 'x'}, {'web_session': 'v'}),
        ({'has_verify_modal': 1}, {'web_session': 'v'}),
        ({'logged_in': 0, 'qr_src': ''}, {'web_session': 'v'}),
    ]
    return [
        *_each('to_int', rednote._to_int, [{'value': value} for value in to_int_values]),  # noqa: SLF001
        *_each('format_published_at', rednote.format_published_at, [{'value': value} for value in times]),
        *_each('infer_image_extension', rednote.infer_image_extension, [{'url': url} for url in image_urls]),
        *_each('normalize_media_url', rednote.normalize_media_url, [{'url': url} for url in media_urls]),
        *_each('pick_image_url', rednote.pick_image_url, [{'entry': entry} for entry in image_entries]),
        *_each(
            'best_video_stream',
            lambda stream: list(rednote._best_video_stream(stream)),  # noqa: SLF001
            [{'stream': stream} for stream in _STREAMS],
        ),
        *_each('note_source_long_edge', rednote.note_source_long_edge, [{'block': block} for block in video_blocks]),
        *_each(
            'build_note_url',
            rednote.build_note_url,
            [{'note_id': note_id, 'token': token} for note_id, token in [('64f1', 'TOKEN'), ('64f1', ''), ('a b', 't&u')]],
        ),
        *_each(
            'build_media_filename',
            lambda media, ext: rednote.build_media_filename(rednote.RedNoteMedia(**media), ext=ext),
            [{'media': {**_MEDIA_BASE, **updates}, 'ext': ext} for updates, ext in media_updates],
        ),
        *_each('parse_api_envelope', rednote.parse_api_envelope, [{'payload': payload} for payload in payloads]),
        *_each('extract_like_page', _like_page_result, [{'data': data} for data in like_pages]),
        *_each('extract_note_media', _note_media_rows, [{'card': card} for card in cards]),
        *_each('decide_login_state', rednote.decide_login_state, [{'probe': probe, 'cookies': cookies} for probe, cookies in logins]),
    ]


_REFUSED_PROXIES: list[str] = [
    'socks5://user:pw@home.example:1080',
    'socks5h://user@home.example',
    'http://',
    '://nohost',
    'ftp://Host.Example:21/x',
    'ftp://user:secret@Host.Example:2121/path?q=1#frag',
    'socks4://home.example:1080',
    'gopher://home.example',
    'HTTPX://user:pw@[2001:db8::1]:8080',
    'http://home.example:abc',
    'http://home.example:70000',
    'http://[::1',
]


def _proxy_probe() -> list[dict[str, Any]]:
    def probe(proxy: str) -> dict[str, Any]:
        return dataclasses.asdict(rednote_browser.probe_proxy(proxy))

    return [{'proxy': proxy, **_call(probe, proxy)} for proxy in _REFUSED_PROXIES]


GOLDENS = {
    'rednote/browser_helpers.json': _browser_helpers,
    'rednote/parsers.json': _parsers,
    'rednote/proxy_probe.json': _proxy_probe,
}

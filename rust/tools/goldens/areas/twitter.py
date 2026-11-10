"""`src/web/twitter.py` (WP7): the gallery-dl command and how gallery-dl itself reads it,
sidecar rows, the `twid` cookie, the account-settings check, the user-lookup retry rule
and where each media type is filed. Every value comes from the real Python code (and the
installed gallery-dl's own option parser); X is never contacted, answers come from an
`httpx.MockTransport`.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from typing import Any
from unittest import mock

import httpx
from gallery_dl import config as gallery_dl_config
from gallery_dl import option as gallery_dl_option
from src.core import settings
from src.tool.site_sessions import StoredCookie
from src.web import twitter


def _cfg(**updates: Any) -> settings.Twitter:
    return settings.Twitter.model_validate({'username': 'me', **updates})


_CONFIGS: list[tuple[dict[str, Any], bool]] = [
    ({'path': '/data/twitter', 'abort_after': 20}, False),
    ({'path': '/data/twitter', 'abort_after': 7}, True),
    ({}, False),
    ({}, True),
    ({'include_retweets': False, 'include_videos': False}, True),
    ({'proxy': 'http://proxy.example:8080', 'sleep_request_seconds': 3.5}, False),
    ({'sleep_request_seconds': 0}, False),
    ({'sleep_request_seconds': 0.1}, False),
    ({'sleep_request_seconds': 1e-05}, False),
    ({'sleep_request_seconds': 1e17}, False),
    ({'sleep_request_seconds': 123456789.123}, True),
    ({'path': 'relative/dir/', 'username': '@Some_User', 'video_path': '/v'}, True),
    ({'path': './collection/twitter'}, False),
]


def _resolve(command: list[str]) -> dict[str, Any]:
    """How the installed gallery-dl reads the command, as tests/test_twitter.py checks it."""
    parser = gallery_dl_option.build_parser()
    args = parser.parse_args(command[1:])
    gallery_dl_config.clear()
    for path, key, value in args.options:
        gallery_dl_config.set(path, key, value)
    resolved = {
        'urls': args.urls,
        'postprocessors': [pp if isinstance(pp, str) else pp.get('name') for pp in args.postprocessors or []],
        'options': [[list(path), key, value] for path, key, value in args.options],
    }
    gallery_dl_config.clear()
    return resolved


def _commands() -> list[dict[str, Any]]:
    cases = []
    for updates, incremental in _CONFIGS:
        cfg = _cfg(**updates)
        command = twitter.build_command(
            cfg, cookie_path=Path('/run/c.txt'), archive_path=cfg.path / twitter.ARCHIVE_FILENAME, incremental=incremental
        )
        cases.append({'settings': updates, 'incremental': incremental, 'command': command, 'gallery_dl': _resolve(command)})
    return cases


def _sidecar(**updates: Any) -> dict[str, Any]:
    payload = {
        'tweet_id': 1234567890123456789,
        'num': 2,
        'author': {'id': 42, 'name': 'artist', 'nick': 'The Artist'},
        'content': 'a caption',
        'date': '2026-05-01 12:34:56',
        'type': 'photo',
        'extension': 'jpg',
    }
    payload.update(updates)
    return payload


_SIDECARS: list[tuple[str, str, str]] = [
    (json.dumps(_sidecar()), '/data/twitter/artist/pic.jpg', '/data/twitter'),
    (json.dumps(_sidecar()), '/elsewhere/pic.jpg', '/data/twitter'),
    (json.dumps(_sidecar(author={})), '/p.jpg', '/data'),
    (json.dumps({'hello': 'world'}), '/p.jpg', '/data'),
    (json.dumps(_sidecar(num='2')), '/p.jpg', '/data'),
    (json.dumps(_sidecar(tweet_id=1.0)), '/p.jpg', '/data'),
    (json.dumps(_sidecar(author='not a dict', content=None, date=None, type=None)), '/data/a/b.mp4', '/data'),
    (json.dumps(_sidecar(author={'id': 0, 'name': None, 'nick': 12.5}, content=['a', 'b'], date={'d': 1})), '/data/x', '/data'),
    (
        json.dumps(_sidecar(content='多行\n"引号"\t</script>   \U0001f600', extra={'nested': [1, 2.0, True, None]}), ensure_ascii=False),  # noqa: RUF001 - the line separator is part of the case
        'relative/a.jpg',
        'relative',
    ),
    ('{"tweet_id": 5, "num": 1, "big": 123456789012345678901234567890, "f": 1e400, "n": NaN, "e": 1.5e-7}', '/r/x.jpg', '/r'),
    ('{"num": 1, "tweet_id": 6, "a": 1, "a": 2}', '/r/../r/x.jpg', '/r'),
    (json.dumps(_sidecar(type='animated_gif')), '/data/twitter/artist/loop.mp4', '/data/twitter/'),
]


def _sidecars() -> list[dict[str, Any]]:
    cases = []
    for text, media_path, root in _SIDECARS:
        payload = json.loads(text)
        row = twitter.parse_sidecar(payload, media_path=Path(media_path), root=Path(root))
        cases.append({'sidecar': text, 'media_path': media_path, 'root': root, 'row': row})
    return cases


_TWIDS: list[str | None] = [None, 'u%3D4242', '"u=77"', 'garbage', 'u=', 'u=12a', 'u%253D5', '"u%3D9"', 'u=٣', 'U=1', ' u=1']


def _user_ids() -> list[dict[str, Any]]:
    cases = []
    for value in _TWIDS:
        cookies = () if value is None else (StoredCookie(name='twid', value=value, domain='.x.com'),)
        cases.append({'twid': value, 'user_id': twitter.user_id_from_twid(cookies)})
    return cases


_SIGNED_IN = (
    StoredCookie(name='auth_token', value='tok', domain='.x.com', expires=1_900_000_000.0),
    StoredCookie(name='ct0', value='csrf', domain='.x.com'),
    StoredCookie(name='twid', value='u%3D4242', domain='.x.com'),
)


def _verify() -> list[dict[str, Any]]:
    scripts: list[tuple[tuple[StoredCookie, ...], str, Any]] = [
        (_SIGNED_IN, 'Me', (200, {'screen_name': 'me'})),
        (_SIGNED_IN, 'me', (200, {'screen_name': 'someone_else'})),
        (_SIGNED_IN, 'me', (401, {'errors': []})),
        (_SIGNED_IN, 'me', (403, 'blocked')),
        (_SIGNED_IN[1:], 'me', None),
        (_SIGNED_IN[:1], 'me', None),
        (_SIGNED_IN, 'me', (404, {'errors': []})),
        (_SIGNED_IN, '', (404, {'errors': []})),
        (_SIGNED_IN[:2], '', (429, 'slow down')),
        (_SIGNED_IN, '', (200, {'screen_name': 'Found'})),
        (_SIGNED_IN, 'me', (200, {'screen_name': ''})),
        (_SIGNED_IN, 'me', (200, {'screen_name': 5})),
        (_SIGNED_IN, 'me', (200, [1])),
        (_SIGNED_IN, 'me', (200, b'<html>')),
        (_SIGNED_IN, 'STRASSE', (200, {'screen_name': 'straße'})),
        (_SIGNED_IN, 'me', (500, {'screen_name': 'me'})),
    ]
    cases = []
    for cookies, username, answer in scripts:
        seen: list[httpx.Request] = []

        def handler(request: httpx.Request, answer: Any = answer) -> httpx.Response:
            seen.append(request)  # noqa: B023 - the closure runs within this iteration
            status, payload = answer
            if isinstance(payload, bytes):
                return httpx.Response(status, content=payload)
            if isinstance(payload, str):
                return httpx.Response(status, text=payload)
            return httpx.Response(status, json=payload)

        warnings: list[str] = []
        with mock.patch.object(twitter.log, 'warning', lambda msg, *args: warnings.append(msg % args)):  # noqa: B023 - the closure runs within this iteration
            check = asyncio.run(twitter.verify_session(cookies, username=username, transport=httpx.MockTransport(handler)))
        cases.append(
            {
                'cookies': [c.name for c in cookies],
                'username': username,
                'answer': None if answer is None else {'status': answer[0], 'body': _body(answer[1])},
                'requests': [
                    {
                        'method': r.method,
                        'url': str(r.url),
                        'headers': {k: v for k, v in r.headers.items() if k not in {'host', 'accept', 'accept-encoding', 'connection'}},
                    }
                    for r in seen
                ],
                'check': {'ok': check.ok, 'account': check.account, 'message': check.message},
                'warnings': warnings,
            }
        )
    return cases


def _body(payload: Any) -> str:
    if isinstance(payload, bytes):
        return payload.decode('latin-1')
    if isinstance(payload, str):
        return payload
    return httpx.Response(200, json=payload).content.decode()


_LOOKUPS: list[tuple[int, list[str]]] = [
    (4, [twitter.USER_LOOKUP_ERROR]),
    (4, ['[twitter][info] Initializing client transaction keys', twitter.USER_LOOKUP_ERROR]),
    (16, [twitter.USER_LOOKUP_ERROR]),
    (20, [twitter.USER_LOOKUP_ERROR]),
    (4, ['[twitter][warning] API errors (1/10):', twitter.USER_LOOKUP_ERROR]),
    (4, ['[twitter][error] Unable to retrieve Tweets from this timeline']),
    (0, []),
    (4, []),
    (4, [f'prefix {twitter.USER_LOOKUP_ERROR}']),
    (4, [f'{twitter.USER_LOOKUP_ERROR} suffix']),
    (4, [twitter.USER_LOOKUP_ERROR, twitter.USER_LOOKUP_ERROR]),
    (4, ['[info] a', '[twitter][info] b', twitter.USER_LOOKUP_ERROR]),
]


def _lookups() -> list[dict[str, Any]]:
    return [
        {'returncode': code, 'errors': errors, 'transient': twitter.is_transient_user_lookup_failure(code, errors)}
        for code, errors in _LOOKUPS
    ]


_DESTINATIONS: list[tuple[str, str | None, str, str]] = [
    ('/data/twitter', None, 'video', '/data/twitter/artist/clip.mp4'),
    ('/data/twitter', None, 'photo', '/data/twitter/artist/pic.jpg'),
    ('/data/twitter', '/media/vid', 'video', '/data/twitter/artist/clip.mp4'),
    ('/data/twitter', '/media/vid', 'animated_gif', '/data/twitter/artist/loop.mp4'),
    ('/data/twitter', '/media/vid', 'photo', '/data/twitter/artist/pic.jpg'),
    ('/data/twitter', '/media/vid', '', '/data/twitter/artist/x'),
    ('/data/twitter', '/media/vid', 'video', '/elsewhere/clip.mp4'),
    ('/data/twitter', '/data/twitter', 'video', '/data/twitter/a/clip.mp4'),
    ('collection/twitter', 'collection/videos', 'video', 'collection/twitter/a/b/c.mp4'),
    ('/data/twitter', '/media/vid', 'Video', '/data/twitter/a.mp4'),
]


def _destinations() -> list[dict[str, Any]]:
    cases = []
    for path, video_path, media_type, media_path in _DESTINATIONS:
        source = twitter.Twitter.__new__(twitter.Twitter)
        source.cfg = _cfg(path=path, video_path=video_path)
        warnings: list[str] = []
        with mock.patch.object(twitter.log, 'warning', lambda msg, *args: warnings.append(msg % args)):  # noqa: B023 - the closure runs within this iteration
            root = source._destination_root(media_type)  # noqa: SLF001
            destination = source._destination_path(Path(media_path), root)  # noqa: SLF001
        cases.append(
            {
                'path': path,
                'video_path': video_path,
                'media_type': media_type,
                'media_path': media_path,
                'root': str(root),
                'destination': str(destination),
                'warnings': warnings,
            }
        )
    return cases


def _texts() -> list[dict[str, Any]]:
    return [
        {'name': 'FILENAME_FORMAT', 'value': twitter.FILENAME_FORMAT},
        {'name': 'DIRECTORY_FORMAT', 'value': list(twitter.DIRECTORY_FORMAT)},
        {'name': 'ARCHIVE_FILENAME', 'value': twitter.ARCHIVE_FILENAME},
        {'name': 'COOKIE_FILENAME', 'value': twitter.COOKIE_FILENAME},
        {'name': 'USER_LOOKUP_ERROR', 'value': twitter.USER_LOOKUP_ERROR},
        {'name': 'LOGIN_URL', 'value': twitter.LOGIN_URL},
        {'name': 'SESSION_COOKIE_URLS', 'value': list(twitter.SESSION_COOKIE_URLS)},
    ]


GOLDENS = {
    'twitter/commands.json': _commands,
    'twitter/sidecars.json': _sidecars,
    'twitter/user_ids.json': _user_ids,
    'twitter/verify_session.json': _verify,
    'twitter/user_lookup.json': _lookups,
    'twitter/destinations.json': _destinations,
    'twitter/texts.json': _texts,
}

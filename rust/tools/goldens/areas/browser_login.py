"""`src/api/browser_login.py` and the browser helpers it shares with RedNote (WP16).

- `proxy_settings.json`: `rednote_browser.build_proxy_settings`, answers and errors.
- `plain_chromium_command.json`: the command line of the plain login window.
- `cookies_for_urls.json`: the DevTools jar narrowed to a site's cookies.
- `user_id_from_probe.json`: `rednote.user_id_from_probe`.
- `login_sites.json`: every field of the sites the 登录态 page offers, per account list.
- `playwright_argv.json`: the command line Playwright itself gives Chromium for fav's two
  `launch_persistent_context` calls (the login window and the RedNote crawl), captured by
  pointing `executable_path` at a script that records its arguments and exits. Nothing is
  started but that script; the profile directory is a temporary one, written as
  `<profile>`.
"""

from __future__ import annotations

import asyncio
import stat
import tempfile
from pathlib import Path
from typing import Any

from src.api import browser_login
from src.core import settings
from src.web import rednote, rednote_browser

_PROXIES = [
    '',
    '   ',
    'home.example:3128',
    'http://home.example:3128',
    'http://user:pw@home.example:3128',
    'http://us%40er:p%3Aw@home.example:3128',
    'http://user@home.example',
    'http://:pw@home.example:8080',
    'HTTP://Home.Example:3128',
    'https://proxy.example',
    'https://proxy.example:443',
    'http://proxy.example:80',
    'socks5://home.example:1080',
    'socks5://user:pw@home.example:1080',
    'socks5h://user@home.example:1080',
    'http://[::1]:3128',
    'http://[fe80::1%25eth0]:3128',
    'http://home.example:0',
    'http://home.example:65535',
    'http://home.example:65536',
    'http://home.example:port',
    'http://home.example:-1',
    'http://home.example: 80',
    'http://home.example:+80',
    'http://home.example:８０',
    'http://',
    'http:///path',
    '://nohost',
    'http://user:pw@',
    'http://[::1',
    '  http://home.example:3128  ',
    'proxy.svc.cluster.local',
    '10.0.0.1:8888',
]


def _raised(exc: Exception) -> dict[str, str]:
    return {'class': exc.__class__.__name__, 'message': str(exc)}


def _proxy_settings() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for proxy in _PROXIES:
        try:
            cases.append({'proxy': proxy, 'result': rednote_browser.build_proxy_settings(proxy)})
        except ValueError as exc:
            cases.append({'proxy': proxy, 'error': _raised(exc)})
    return cases


def _plain_chromium_command() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for proxy in ['', 'http://home.proxy.svc.cluster.local', 'home.example:3128', 'socks5://home.example:1080',
                  'http://user:pass@proxy.example:3128', 'http://:pw@proxy.example', 'socks5://user:pw@h:1', 'http://h:x']:
        for url in ['https://x.com/i/flow/login', 'about:blank']:
            arguments = {
                'executable': '/opt/chrome',
                'profile_dir': '/data/profiles/x',
                'proxy': proxy,
                'devtools_port': 5998,
                'url': url,
            }
            case: dict[str, Any] = dict(arguments)
            try:
                case['command'] = browser_login.build_plain_chromium_command(
                    executable=arguments['executable'],
                    profile_dir=Path(arguments['profile_dir']),
                    proxy=proxy,
                    devtools_port=5998,
                    url=url,
                )
            except ValueError as exc:
                case['error'] = _raised(exc)
            except browser_login.LoginError as exc:
                case['error'] = _raised(exc)
            cases.append(case)
    return cases


_JARS: list[list[dict[str, Any]]] = [
    [
        {'name': 'auth_token', 'domain': '.x.com'},
        {'name': 'host_only', 'domain': 'x.com'},
        {'name': 'sub_only', 'domain': 'api.x.com'},
        {'name': 'other', 'domain': '.pixiv.net'},
        {'name': 'lookalike', 'domain': '.notx.com'},
        {'name': 'dots', 'domain': '..x.com'},
        {'name': 'none', 'domain': None},
        {'name': 'missing'},
        {'name': 'empty', 'domain': ''},
        {'name': 'upper', 'domain': '.X.com'},
    ],
    [
        {'name': 'SESSDATA', 'value': 'v', 'domain': '.bilibili.com', 'path': '/', 'expires': 1.9e9},
        {'name': 'bili_jct', 'value': 'w', 'domain': 'www.bilibili.com', 'path': '/'},
        {'name': 'passport', 'value': 'p', 'domain': 'passport.bilibili.com'},
    ],
]
_URL_SETS: list[list[str]] = [
    ['https://x.com/'],
    ['https://www.x.com/'],
    ['https://api.x.com/1.1/'],
    ['https://www.bilibili.com/'],
    ['https://www.bilibili.com/', 'https://passport.bilibili.com/login'],
    ['https://WWW.Bilibili.com/'],
    [],
    ['not a url'],
    ['https://www.pixiv.net/', 'https://x.com'],
]


def _cookies_for_urls() -> list[dict[str, Any]]:
    return [
        {'cookies': jar, 'urls': urls, 'result': browser_login.cookies_for_urls(jar, tuple(urls))} for jar in _JARS for urls in _URL_SETS
    ]


_PROBES: list[dict[str, Any]] = [
    {},
    {'logged_in': True, 'user_id': '5ff'},
    {'logged_in': True, 'user_id': '  5ff  '},
    {'logged_in': True, 'user_id': '', 'profile_href': '/user/profile/5ff?tab=liked'},
    {'logged_in': True, 'profile_href': '  /user/profile/abc  '},
    {'logged_in': True, 'profile_href': '/elsewhere/abc?x=1'},
    {'logged_in': True, 'user_id': 42},
    {'logged_in': True, 'user_id': None, 'profile_href': None},
    {'logged_in': 1, 'user_id': 'x'},
    {'logged_in': 'yes', 'user_id': 'x'},
    {'logged_in': False, 'guest': True, 'user_id': '6a7d8554000000001400c801'},
    {'logged_in': 0, 'user_id': 'x'},
    {'logged_in': True},
]


def _user_id_from_probe() -> list[dict[str, Any]]:
    return [{'probe': probe, 'user_id': rednote.user_id_from_probe(probe)} for probe in _PROBES]


def _login_sites() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for names in [[], ['main'], ['main', 'alt']]:
        snapshot = settings.Settings()
        snapshot.web.bilibili = settings.Bilibili.model_validate({'accounts': [{'name': name} for name in names]})
        settings.use(snapshot)
        try:
            sites = browser_login.login_sites()
        finally:
            settings.use(settings.Settings())
        cases.append({
            'bilibili_accounts': names,
            'sites': [
                {
                    'key': site.key,
                    'label': site.label,
                    'login_url': site.login_url,
                    'cookie_urls': list(site.cookie_urls),
                    'session_cookie': site.session_cookie,
                    'controlled': site.controlled,
                    'store_cookie_values': site.store_cookie_values,
                    'verifies_in_browser': site.verify_in_browser is not None,
                    'has_profile_dir': site.profile_dir is not None,
                    'has_lock_name': site.lock_name is not None,
                    'has_after_login': site.after_login is not None,
                    'hint': site.hint,
                }
                for site in sites.values()
            ],
        })
    return cases


_RECORDER = """#!/bin/sh
for arg in "$@"; do printf '%s\\0' "$arg"; done > "$(dirname "$0")/argv"
exit 1
"""


async def _capture(options: dict[str, Any], workdir: Path) -> list[str]:
    from playwright.async_api import async_playwright  # noqa: PLC0415 - only this golden needs it

    recorder = workdir / 'chrome'
    recorder.write_text(_RECORDER, encoding='utf-8')
    recorder.chmod(recorder.stat().st_mode | stat.S_IXUSR)
    (workdir / 'argv').unlink(missing_ok=True)
    launch = {key: value for key, value in options.items() if key != 'env'}
    async with async_playwright() as playwright:
        try:
            await playwright.chromium.launch_persistent_context(executable_path=str(recorder), timeout=10_000, **launch)
        except Exception:  # noqa: BLE001, S110 - the recorder always exits 1; the arguments are what matter
            pass
    argv = (workdir / 'argv').read_bytes().split(b'\0')[:-1]
    profile = str(launch['user_data_dir'])
    return [arg.decode().replace(profile, '<profile>') for arg in argv]


def _playwright_argv() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory() as directory:
        workdir = Path(directory)
        profile = workdir / 'profile'
        builders = [
            ('login', lambda proxy, headless: browser_login.build_login_launch_options(profile_dir=profile, proxy=proxy, display=99)),
            (
                'rednote',
                lambda proxy, headless: rednote_browser.build_launch_options(user_data_dir=profile, proxy=proxy, headless=headless),
            ),
        ]
        proxies = ['', 'http://user:pw@proxy.example:3128', 'socks5://proxy.example:1080', 'https://proxy.example:443', 'http://proxy.example:80']
        for name, build in builders:
            for headless in ([False] if name == 'login' else [True, False]):
                for proxy in proxies:
                    options = build(proxy, headless)
                    argv = asyncio.run(_capture(options, workdir))
                    cases.append({
                        'launch': name,
                        'headless': headless,
                        'proxy': proxy,
                        'args': list(options['args']),
                        'ignore_default_args': list(options['ignore_default_args']),
                        'argv': argv,
                    })
    return cases


GOLDENS = {
    'browser_login/proxy_settings.json': _proxy_settings,
    'browser_login/plain_chromium_command.json': _plain_chromium_command,
    'browser_login/cookies_for_urls.json': _cookies_for_urls,
    'browser_login/user_id_from_probe.json': _user_id_from_probe,
    'browser_login/login_sites.json': _login_sites,
    'browser_login/playwright_argv.json': _playwright_argv,
}

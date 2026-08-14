#!/usr/bin/env python
"""Find out what RedNote makes of the browser this app actually ships.

The cluster reaches the site through a home line, and this machine is on that same
line, so the address -- the axis that cost the account its sessions once already --
is no longer a variable between here and there. What is left to measure is the other
two: what the browser looks like, and how it behaves.

This deliberately drives `src/web/rednote_browser.py` rather than a copy of it. A
probe that configured its own Chromium would answer a question nobody asked.

    uv run python script/rednote_probe.py egress --proxy http://home:3128
    uv run python script/rednote_probe.py door --proxy http://home:3128
    uv run python script/rednote_probe.py door --headful --channel chrome
    uv run python script/rednote_probe.py note https://www.xiaohongshu.com/explore/<id>?xsec_token=...

`door` and `note` never touch the account: they are anonymous page loads, and the
question they answer -- does this browser get served the same site a person gets --
is the one worth settling before spending a login attempt on it.
"""
# ruff: noqa: E402, T201

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

# This probe never reaches the database, but importing the app's logger pulls in the
# bootstrap config, which insists on both. Placeholders keep the script runnable
# without a deployment behind it.
os.environ.setdefault('POSTGRES_DSN', 'postgresql://probe/unused')
os.environ.setdefault('API_TOKEN', 'unused-token-for-a-local-probe')
# Quiet by default so the report below is the only thing on stdout. Export
# LOG_LEVEL=DEBUG to see the module's own running commentary alongside it.
os.environ.setdefault('LOG_LEVEL', 'WARNING')

from src.web.rednote_browser import (
    RISK_CONTROL_STATUS_CODES,
    WEB_ORIGIN,
    build_launch_options,
    describe_shape,
    probe_proxy,
)

# Reads as a plain page load rather than as an interrogation, and every value has a
# known-good answer in a real Chrome on the same machine.
_FINGERPRINT_SCRIPT = """() => {
    const gl = (() => {
        try {
            const c = document.createElement('canvas').getContext('webgl');
            const dbg = c && c.getExtension('WEBGL_debug_renderer_info');
            return dbg ? {vendor: c.getParameter(dbg.UNMASKED_VENDOR_WEBGL), renderer: c.getParameter(dbg.UNMASKED_RENDERER_WEBGL)} : null;
        } catch (e) { return {error: String(e)}; }
    })();
    const uad = navigator.userAgentData;
    return {
        webdriver: navigator.webdriver,
        userAgent: navigator.userAgent,
        uaDataBrands: uad ? uad.brands.map(b => b.brand + ' ' + b.version) : null,
        uaDataPlatform: uad ? uad.platform : null,
        uaDataMobile: uad ? uad.mobile : null,
        languages: navigator.languages,
        platform: navigator.platform,
        hardwareConcurrency: navigator.hardwareConcurrency,
        deviceMemory: navigator.deviceMemory ?? null,
        plugins: navigator.plugins.length,
        mimeTypes: navigator.mimeTypes.length,
        webgl: gl,
        hasChromeObject: Boolean(window.chrome),
        chromeKeys: window.chrome ? Object.keys(window.chrome).slice(0, 8) : null,
        // The classic headless mismatch: denied permission with default notification state.
        notificationPermission: (typeof Notification !== 'undefined') ? Notification.permission : null,
        screen: {w: screen.width, h: screen.height, availW: screen.availWidth, dpr: devicePixelRatio},
        outerSize: {w: outerWidth, h: outerHeight},
        timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
        visibility: document.visibilityState,
    };
}"""

_SITE_STATE_SCRIPT = """() => {
    const unwrap = (v) => (v && typeof v === 'object' && '_value' in v) ? v._value : v;
    const state = window.__INITIAL_STATE__ || {};
    const user = state.user || {};
    const info = unwrap(user.userInfo) || {};
    return {
        url: location.href,
        title: document.title,
        hasInitialState: Boolean(window.__INITIAL_STATE__),
        initialStateKeys: Object.keys(state).slice(0, 20),
        loggedIn: unwrap(user.loggedIn),
        guest: info.guest === true,
        hasLoginModal: Boolean(document.querySelector('.login-modal')),
        hasVerifyModal: Boolean(document.querySelector('.r-captcha-modal')),
        hasCaptchaApp: Boolean(document.querySelector('.fe-captcha-app')),
        loginQr: Boolean(document.querySelector('.login-modal img.qrcode-img')),
        verifyQr: Boolean(document.querySelector('.r-captcha-modal img.qrcode-img')),
        feedCards: document.querySelectorAll('section.note-item, a[href^="/explore/"]').length,
        bodyTextHead: (document.body.innerText || '').replace(/\\s+/g, ' ').trim().slice(0, 200),
        // Every QR on the page with its ancestry, because the container varies by
        // theme, by entry point (/explore overlay vs the standalone /login page) and
        // by how much the site trusts the browser. The image class is the constant.
        qrImages: [...document.querySelectorAll('img.qrcode-img, img[class*="qrcode"]')].map((i) => {
            const path = [];
            for (let e = i, k = 0; e && k < 6; e = e.parentElement, k++) {
                const cls = (typeof e.className === 'string' && e.className) ? '.' + e.className.trim().split(/\\s+/).join('.') : '';
                path.push(e.tagName.toLowerCase() + cls);
            }
            const src = i.getAttribute('src') || '';
            return {path: path.join(' < '), len: src.length, isData: src.startsWith('data:image/')};
        }),
        // What kind of challenge, if any. A slider, a QR and an iframed third-party
        // widget each call for completely different handling -- or for giving up.
        captcha: (() => {
            const app = document.querySelector('.fe-captcha-app');
            if (!app) { return null; }
            const el = (sel, f) => [...app.querySelectorAll(sel)].map(f).slice(0, 6);
            return {
                classes: app.className,
                text: (app.innerText || '').replace(/\\s+/g, ' ').trim().slice(0, 240),
                modals: el('[class*="modal"]', (e) => e.className),
                images: el('img', (i) => {
                    const src = i.getAttribute('src') || '';
                    return {cls: i.className, kind: src.slice(0, 22), len: src.length};
                }),
                iframes: el('iframe', (f) => f.getAttribute('src') || '(no src)'),
                canvases: app.querySelectorAll('canvas').length,
                sliders: el('[class*="slide"], [class*="drag"], [class*="track"]', (e) => e.className),
                buttons: el('button, [role="button"], [class*="btn"]', (e) => (e.innerText || '').trim().slice(0, 24)).filter(Boolean),
            };
        })(),
    };
}"""

_TABS_SCRIPT = """() => {
    const tabs = [...document.querySelectorAll('[class*="tab"]')];
    return tabs.map((t) => ({cls: t.className, text: (t.innerText || '').replace(/\\s+/g, ' ').trim().slice(0, 30)}))
        .filter((t) => t.text).slice(0, 12);
}"""

_NOTE_STATE_SCRIPT = """(id) => {
    const state = window.__INITIAL_STATE__ || {};
    const map = (state.note && state.note.noteDetailMap) || {};
    const entry = map[id] || Object.values(map)[0] || null;
    return (entry && entry.note) || null;
}"""


async def _close(handle: Any, page: Any) -> None:
    """`--via-source` hands back the source's browser, which closes itself."""
    if hasattr(handle, 'aclose'):
        await handle.aclose()
        return
    await page.context.close()
    await handle.stop()


def _profile_dir(args: argparse.Namespace) -> Path:
    return Path(args.profile).expanduser()


def _options(args: argparse.Namespace) -> dict[str, Any]:
    """Production launch options, with only the knobs under test overridden."""
    options = build_launch_options(
        user_data_dir=_profile_dir(args),
        proxy=args.proxy,
        headless=not args.headful,
    )
    if args.channel:
        options['channel'] = args.channel
    if args.user_agent:
        options['user_agent'] = args.user_agent
    return options


class _Recorder:
    """Every response the page pulls, so a wall shows up even if the DOM hides it."""

    def __init__(self) -> None:
        self.statuses: dict[int, int] = {}
        self.walled: list[str] = []
        # Every API path the page pulls, deduped. The endpoint the likes grid actually
        # drives has only ever been assumed; this is what would show it.
        self.api_paths: dict[str, int] = {}

    def on_response(self, response: Any) -> None:
        self.statuses[response.status] = self.statuses.get(response.status, 0) + 1
        if response.status in RISK_CONTROL_STATUS_CODES:
            self.walled.append(f'{response.status} {response.url[:120]}')
        path = urlsplit(response.url).path
        if '/api/' in path:
            key = f'{response.status} {urlsplit(response.url).netloc}{path}'
            self.api_paths[key] = self.api_paths.get(key, 0) + 1


async def _open(args: argparse.Namespace) -> tuple[Any, Any, _Recorder]:
    """Open a page, either raw or through the class the source actually uses.

    `--via-source` is the honest check: it exercises everything `PlaywrightNoteBrowser`
    does on the way up, the User-Agent correction included, so a clean result here is
    a result about the shipped code rather than about the probe's own settings.
    """
    from playwright.async_api import async_playwright  # noqa: PLC0415

    recorder = _Recorder()
    profile = _profile_dir(args)
    if args.via_source:
        from src.web.rednote_browser import PlaywrightNoteBrowser  # noqa: PLC0415

        browser = PlaywrightNoteBrowser(
            user_data_dir=profile,
            proxy=args.proxy,
            headless=not args.headful,
        )
        await browser.start()
        browser._page.on('response', recorder.on_response)  # noqa: SLF001 - a probe, deliberately
        return browser, browser._page, recorder  # noqa: SLF001

    playwright = await async_playwright().start()
    context = await playwright.chromium.launch_persistent_context(**_options(args))
    page = context.pages[0] if context.pages else await context.new_page()
    page.on('response', recorder.on_response)
    return playwright, page, recorder


async def _settle(page: Any, seconds: float = 20.0) -> None:
    """This SPA mounts well after domcontentloaded; sampling early reads as an empty page.

    Waits on the login prompt specifically, not on the feed: the feed renders first,
    and returning there reports "no login modal" for a page that was about to show one.
    """
    for _ in range(int(seconds * 4)):
        state = await _evaluate(page, _SITE_STATE_SCRIPT)
        if state and (state.get('loginQr') or state.get('verifyQr') or state.get('loggedIn')):
            return
        await asyncio.sleep(0.25)


async def _evaluate(page: Any, script: str, *args: Any) -> Any:
    """The site routes itself while settling; an evaluate that lands mid-navigation throws."""
    try:
        return await page.evaluate(script, *args)
    except Exception as exc:  # noqa: BLE001
        print(f'  (evaluate landed mid-navigation: {str(exc).splitlines()[0]})', file=sys.stderr)
        return None


async def cmd_egress(args: argparse.Namespace) -> None:
    """Confirm the premise: is this machine's egress the deployment's egress?"""
    direct = probe_proxy('')
    print(json.dumps({'direct': {'ok': direct.ok, 'code': direct.code, 'exit_ip': direct.exit_ip}}, indent=2))
    if args.proxy:
        through = probe_proxy(args.proxy)
        print(json.dumps({'proxy': {'ok': through.ok, 'code': through.code, 'exit_ip': through.exit_ip}}, indent=2))
        if direct.exit_ip and direct.exit_ip == through.exit_ip:
            print('\nSame exit address both ways: local findings carry to the deployment unchanged.')
        else:
            print('\nDifferent exit addresses. The IP axis is still a variable between here and the cluster.')


async def cmd_door(args: argparse.Namespace) -> None:
    """What the site serves this browser, and what this browser tells the site."""
    playwright, page, recorder = await _open(args)
    try:
        response = await page.goto(WEB_ORIGIN, wait_until='domcontentloaded')
        await _settle(page)
        report = {
            'document_status': response.status if response else None,
            'launch': {k: v for k, v in _options(args).items() if k in ('channel', 'headless', 'proxy', 'locale', 'timezone_id')},
            'site': await _evaluate(page, _SITE_STATE_SCRIPT),
            'fingerprint': await _evaluate(page, _FINGERPRINT_SCRIPT),
            'response_statuses': dict(sorted(recorder.statuses.items())),
            'risk_control_hits': recorder.walled,
        }
        print(json.dumps(report, indent=2, ensure_ascii=False))
    finally:
        await _close(playwright, page)


async def cmd_note(args: argparse.Namespace) -> None:
    """Note shapes, from public pages, with no account involved."""
    playwright, page, recorder = await _open(args)
    try:
        for url in args.urls:
            note_id = url.split('/explore/')[-1].split('?')[0]
            await page.goto(url, wait_until='domcontentloaded')
            for _ in range(40):
                card = await _evaluate(page, _NOTE_STATE_SCRIPT, note_id)
                if card:
                    break
                await asyncio.sleep(0.25)
            print(json.dumps({'note_id': note_id, 'shape': describe_shape(card) if card else None}, indent=2, ensure_ascii=False))
        if recorder.walled:
            print(json.dumps({'risk_control_hits': recorder.walled}, indent=2))
    finally:
        await _close(playwright, page)


async def cmd_login(args: argparse.Namespace) -> None:
    """Sign in by hand, then capture the two things only a session can answer.

    Runs the source's own browser, so the selectors, the staging and the reload are
    the shipped ones. Headful by default: the security code lives about a minute,
    which a scan off this screen fits inside and a round trip through anything else
    does not. In the cluster that code goes straight to Telegram instead.
    """
    from src.web.rednote import decide_login_state, extract_like_page, parse_api_envelope, user_id_from_probe  # noqa: PLC0415

    browser, page, recorder = await _open(args)
    try:
        deadline = asyncio.get_running_loop().time() + args.wait
        seen: set[str] = set()
        probe: dict[str, Any] = {}
        while asyncio.get_running_loop().time() < deadline:
            probe = await browser.probe_login()
            state = decide_login_state(probe, await browser.cookie_dict())
            if state == 'logged_in':
                break
            qr_src = str(probe.get('qr_src') or '')
            if qr_src and qr_src not in seen:
                seen.add(qr_src)
                print(f'>>> SCAN THE {str(probe.get("qr_stage") or "login").upper()} QR in the window ({len(qr_src)} bytes)', flush=True)
            elif not qr_src:
                await browser.reload_login()
            await asyncio.sleep(2)

        if str(probe.get('logged_in') or '') != 'True' and not probe.get('logged_in'):
            print(json.dumps({'signed_in': False, 'last_probe_stage': probe.get('qr_stage')}, indent=2))
            return

        user_id = user_id_from_probe(probe)
        cookies = await browser.cookie_dict()
        print(json.dumps({'signed_in': True, 'user_id_len': len(user_id), 'has_web_session': bool(cookies.get('web_session'))}, indent=2))
        print(json.dumps({'signed_in_user_shape': describe_shape(probe)}, indent=2, ensure_ascii=False))

        # The one thing this whole source is built on and has never been seen.
        await browser.open_likes(user_id=user_id)
        captured = await browser.next_like_page(timeout_seconds=30)
        if captured is None:
            print(json.dumps({'like_page': 'no response intercepted', 'risk_hits': recorder.walled}, indent=2))
            return
        print(
            json.dumps(
                {'like_page_status': captured.get('status'), 'like_page_shape': describe_shape(captured.get('body'))},
                indent=2,
                ensure_ascii=False,
            )
        )
        notes, cursor, has_more = extract_like_page(parse_api_envelope(captured.get('body')))
        print(json.dumps({'parsed_notes': len(notes), 'cursor_len': len(cursor), 'has_more': has_more}, indent=2))
    finally:
        await _close(browser, page)


async def cmd_likes(args: argparse.Namespace) -> None:
    """What the 赞过 tab actually does, using the session already in the profile.

    The whole crawl rests on that tab driving `note/like/page`, which was taken from
    the API this source used to call directly and has never been watched. This clicks
    the tab the way the source does and reports every API path the page pulls.
    """
    from src.web.rednote import decide_login_state, user_id_from_probe  # noqa: PLC0415

    browser, page, recorder = await _open(args)
    try:
        probe = await browser.probe_login()
        state = decide_login_state(probe, await browser.cookie_dict())
        if state != 'logged_in':
            print(json.dumps({'signed_in': False, 'state': state, 'hint': 'run the login command first'}, indent=2))
            return

        user_id = user_id_from_probe(probe)
        opened = await browser.open_likes(user_id=user_id)
        print(json.dumps({'liked_tab_opened': opened}, indent=2))

        captured = await browser.next_like_page(timeout_seconds=args.watch)
        if captured is not None:
            from src.web.rednote import extract_like_page, parse_api_envelope  # noqa: PLC0415

            print(
                json.dumps({'status': captured.get('status'), 'shape': describe_shape(captured.get('body'))}, indent=2, ensure_ascii=False)
            )
            notes, cursor, has_more = extract_like_page(parse_api_envelope(captured.get('body')))
            print(
                json.dumps(
                    {
                        'parsed_notes': len(notes),
                        'with_token': sum(1 for n in notes if n.xsec_token),
                        'cursor_len': len(cursor),
                        'has_more': has_more,
                    },
                    indent=2,
                )
            )
        tabs = await _evaluate(page, _TABS_SCRIPT)
        print(json.dumps({'url_after_open_likes': (await _evaluate(page, '() => location.href') or '')[:110]}, indent=2))
        print(json.dumps({'tabs': tabs}, indent=2, ensure_ascii=False))
        print(json.dumps({'api_paths': recorder.api_paths}, indent=2))
        print(json.dumps({'risk_hits': recorder.walled}, indent=2))
    finally:
        await _close(browser, page)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--proxy', default='', help='Same value as web.rednote.proxy. Empty means direct.')
    parser.add_argument('--headful', action='store_true', help='Show the window. Headless is what the cluster runs.')
    parser.add_argument('--channel', default='', help="Override the browser channel, e.g. 'chrome' to compare against the real one.")
    parser.add_argument(
        '--user-agent',
        default='',
        help="Override the UA. Pass 'strip-headless' to reuse the browser's own, minus the Headless token.",
    )
    parser.add_argument('--via-source', action='store_true', help='Drive PlaywrightNoteBrowser instead of a raw launch.')
    parser.add_argument('--profile', default='./data/rednote-probe-profile', help='Kept out of the real profile by default.')
    sub = parser.add_subparsers(dest='command', required=True)
    sub.add_parser('egress', help='Exit address, direct and through the proxy')
    sub.add_parser('door', help='What the site serves this browser, anonymously')
    note = sub.add_parser('note', help='Dump the shape of public note pages')
    note.add_argument('urls', nargs='+')
    login = sub.add_parser('login', help='Sign in by hand, then capture the likes envelope')
    login.add_argument('--wait', type=float, default=300.0, help='Seconds to wait for the scans.')
    likes = sub.add_parser('likes', help='Watch what the 赞过 tab pulls, using the stored session')
    likes.add_argument('--watch', type=float, default=20.0, help='Seconds to record traffic after opening the tab.')

    args = parser.parse_args()
    handlers = {'egress': cmd_egress, 'door': cmd_door, 'note': cmd_note, 'login': cmd_login, 'likes': cmd_likes}
    asyncio.run(handlers[args.command](args))


if __name__ == '__main__':
    main()

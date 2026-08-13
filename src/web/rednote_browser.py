"""The signed-in Chromium profile behind the RedNote source.

Everything that touches Playwright lives here, so ``src/web/rednote.py`` can be
tested without a browser: it talks to the ``NoteBrowser`` protocol below and never
imports Playwright itself.

The browser is the point rather than an implementation detail. RedNote reads IP,
device fingerprint and behaviour together, and a plain HTTP client replaying a
session presents badly on all three at once. A profile that stays on a volume,
accumulates history, and issues the site's own XHRs is the same account seen from
the same browser it was signed in on -- and it also means the crawl never has to
compute a request signature, so there is nothing to break when the site rotates one.

The launch options and the cookie conversion are pure functions on purpose: they are
the parts worth asserting in tests, and none of them needs a browser to run.
"""

from __future__ import annotations

import asyncio
import base64
import binascii
import contextlib
import time
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Protocol
from urllib.parse import parse_qs, unquote, urlsplit

import httpx

from src.core import logger

if TYPE_CHECKING:
    from collections.abc import Mapping, Sequence
    from pathlib import Path

log = logger.get('rednote')

WEB_ORIGIN = 'https://www.xiaohongshu.com'
LIKE_PAGE_PATH = '/api/sns/web/v1/note/like/page'
LIKED_TAB_LABEL = '赞过'

# The captcha wall. Whether it is a crawl being refused or this module's probe being
# turned away, it means the same thing: the address is not one the site will serve.
RISK_CONTROL_STATUS_CODES = frozenset({461, 471})

# Chromium writes these into the profile and stamps them with `hostname-pid`. Every
# pod gets a new hostname, so after a SIGKILL the next one reads them as a lock held
# by another machine and either refuses to start or hangs.
PROFILE_SINGLETON_FILES = ('SingletonLock', 'SingletonCookie', 'SingletonSocket')

_QR_DATA_URL_MARKER = ';base64,'
# The settings form's proxy test. Short, because an operator is watching it spin.
_PROBE_TIMEOUT_SECONDS = 30.0
_EXIT_IP_TIMEOUT_SECONDS = 15.0
_EXIT_IP_URL = 'https://checkip.amazonaws.com'
_HTTP_ERROR_MIN = 400
_DEFAULT_VIEWPORT = {'width': 1280, 'height': 800}
_SCROLL_STEPS = 3
_SCROLL_PIXELS = 900
_SCROLL_PAUSE_SECONDS = 0.35
_PAGE_POLL_SECONDS = 0.5
# Where the pointer is parked before scrolling. `mouse.wheel` dispatches wherever the
# cursor happens to be, and it starts at (0, 0) -- outside the grid, over nothing.
_GRID_POINTER = (640, 500)

# Read once when the page is asked about its login state. One round trip, and the
# decision itself is made in Python where it can be tested.
_LOGIN_PROBE_SCRIPT = """() => {
    const modal = document.querySelector('.login-modal');
    const qr = document.querySelector('.login-modal img.qrcode-img');
    const state = window.__INITIAL_STATE__ || {};
    const user = (state.user && (state.user.userInfo || state.user.loggedUser)) || {};
    const profile = document.querySelector('a[href^="/user/profile/"]');
    return {
        has_login_modal: Boolean(modal),
        qr_src: (qr && qr.getAttribute('src')) || '',
        user_id: String(user.userId || user.user_id || ''),
        profile_href: (profile && profile.getAttribute('href')) || '',
        modal_html: modal ? modal.outerHTML.slice(0, 4000) : '',
    };
}"""

# Narrowed inside the page: __INITIAL_STATE__ as a whole is megabytes of feed and
# user state, and only one note is ever wanted.
_NOTE_STATE_SCRIPT = """(id) => {
    const state = window.__INITIAL_STATE__ || {};
    const map = (state.note && state.note.noteDetailMap) || {};
    const entry = map[id] || Object.values(map)[0] || null;
    return (entry && entry.note) || null;
}"""

_WEBDRIVER_INIT_SCRIPT = "Object.defineProperty(navigator, 'webdriver', {get: () => undefined});"


class BrowserDependencyError(RuntimeError):
    """Playwright or its Chromium build is not installed."""


class ProxyConfigurationError(ValueError):
    """The configured proxy cannot be expressed to Chromium."""


class NoteBrowser(Protocol):
    """What the source needs from a browser. Implemented for real below, faked in tests."""

    async def probe_login(self) -> dict[str, Any]: ...
    async def cookie_dict(self) -> dict[str, str]: ...
    async def netscape_cookies(self) -> str: ...
    async def user_agent(self) -> str: ...
    async def reload_login(self) -> None: ...
    async def open_likes(self, *, user_id: str) -> None: ...
    async def next_like_page(self, *, timeout_seconds: float) -> dict[str, Any] | None: ...
    async def note_state(self, *, note_url: str, note_id: str) -> dict[str, Any]: ...


def clear_stale_profile_locks(user_data_dir: Path) -> list[str]:
    """Drop the singleton files a killed Chromium left behind.

    Safe only because the caller holds the advisory lock on this profile: nothing
    else can be using it, so a lock file here is by definition stale.
    """
    cleared: list[str] = []
    for name in PROFILE_SINGLETON_FILES:
        path = user_data_dir / name
        if path.is_symlink() or path.exists():
            path.unlink(missing_ok=True)
            cleared.append(name)
    return cleared


def build_proxy_settings(proxy: str) -> dict[str, str] | None:
    """Split a proxy URL the way Chromium needs it.

    Chromium ignores userinfo in ``--proxy-server``, so credentials have to travel
    beside the server rather than inside it, and it cannot authenticate to a SOCKS
    proxy at all -- which is worth failing loudly about rather than silently
    connecting as nobody.
    """
    raw = proxy.strip()
    if not raw:
        return None

    parts = urlsplit(raw if '://' in raw else f'http://{raw}')
    host = parts.hostname or ''
    if not host:
        msg = f'Could not read a host out of the proxy setting: {proxy!r}'
        raise ProxyConfigurationError(msg)
    if parts.scheme.startswith('socks') and (parts.username or parts.password):
        msg = 'Chromium cannot authenticate to a SOCKS proxy; use an http:// or https:// proxy instead.'
        raise ProxyConfigurationError(msg)

    server = f'{parts.scheme}://{host}:{parts.port}' if parts.port else f'{parts.scheme}://{host}'
    settings: dict[str, str] = {'server': server}
    if parts.username:
        settings['username'] = unquote(parts.username)
    if parts.password:
        settings['password'] = unquote(parts.password)
    return settings


@dataclass(frozen=True, slots=True)
class ProxyProbe:
    ok: bool
    code: str
    message: str
    exit_ip: str = ''
    # True when nothing was configured and the request left on the pod's own address.
    # A reachable direct egress is still only usable with `allow_direct_connection`.
    direct: bool = False


def probe_proxy(proxy: str, *, timeout: float = _PROBE_TIMEOUT_SECONDS, client: httpx.Client | None = None) -> ProxyProbe:
    """Check the egress this source would use, without saving it and without the account.

    The request is anonymous -- no profile, no cookies, nothing a session could be
    lost over. What it answers is the question the account cannot afford to have
    answered the expensive way: whether this address is one the site serves or one
    it walls off, plus the exit address itself, which is how an operator tells a home
    line from a datacenter range.

    It proves the address, not the browser: Chromium is not launched, so a profile
    that is signed out or a wrong `user_id` are still only found at run time. The
    proxy string is nevertheless parsed exactly the way Chromium needs it, which is
    what turns an authenticated SOCKS proxy into an error here rather than into a
    silent anonymous connection during a run.
    """
    raw = proxy.strip()
    try:
        build_proxy_settings(raw)
    except ProxyConfigurationError as exc:
        return ProxyProbe(ok=False, code='invalid', message=str(exc))

    if client is not None:
        return _run_proxy_probe(client, direct=not raw)

    # Chromium infers a scheme where httpx demands one, and `build_proxy_settings`
    # above infers the same one -- so filling it in here is what makes the address
    # this dials the address a run would.
    dialable = (raw if '://' in raw else f'http://{raw}') if raw else None
    try:
        owned_client = httpx.Client(
            proxy=dialable,
            follow_redirects=True,
            timeout=timeout,
            headers={'Accept-Language': 'zh-CN,zh;q=0.9'},
            # Chromium does not read HTTP_PROXY/HTTPS_PROXY, so a probe speaking for it
            # must not either -- otherwise an empty setting would report the environment's
            # egress while the browser quietly used the pod's own.
            trust_env=False,
            limits=httpx.Limits(max_keepalive_connections=0),
        )
    except ValueError as exc:
        # An unusable scheme, say. Chromium would refuse it too, so name it here
        # rather than letting it surface as a 500 from the settings form.
        return ProxyProbe(ok=False, code='invalid', message=f'Not a usable proxy URL: {exc}')

    with owned_client:
        return _run_proxy_probe(owned_client, direct=not raw)


def _run_proxy_probe(client: httpx.Client, *, direct: bool) -> ProxyProbe:
    # Asked first, and not only for the readout: whether this call got through is what
    # later tells a dead proxy apart from a proxy that works and a site that will not
    # answer it. The two want completely different things done about them.
    exit_ip = _probe_exit_ip(client)
    try:
        response = client.get(WEB_ORIGIN)
    except httpx.ProxyError as exc:
        return ProxyProbe(ok=False, code='proxy_error', message=f'The proxy refused the connection: {exc}', direct=direct)
    except httpx.HTTPError as exc:
        if not direct and not exit_ip:
            # Everything goes through the proxy, and nothing has come back through it.
            return ProxyProbe(ok=False, code='proxy_error', message=f'Could not reach the proxy: {exc}', direct=direct)
        return ProxyProbe(ok=False, code='unreachable', message=f'Could not reach {WEB_ORIGIN}: {exc}', exit_ip=exit_ip, direct=direct)

    if response.status_code in RISK_CONTROL_STATUS_CODES:
        message = f'The site answered HTTP {response.status_code}: this address is behind the captcha wall.'
        return ProxyProbe(ok=False, code='risk_control', message=message, exit_ip=exit_ip, direct=direct)
    if response.status_code >= _HTTP_ERROR_MIN:
        message = f'{WEB_ORIGIN} returned HTTP {response.status_code}.'
        return ProxyProbe(ok=False, code='http_error', message=message, exit_ip=exit_ip, direct=direct)

    how = 'directly' if direct else 'through the proxy'
    return ProxyProbe(ok=True, code='ok', message=f'Reached {WEB_ORIGIN} {how}.', exit_ip=exit_ip, direct=direct)


def _probe_exit_ip(client: httpx.Client) -> str:
    """Best-effort exit address, shown so an operator can tell residential from datacenter."""
    try:
        response = client.get(_EXIT_IP_URL, timeout=_EXIT_IP_TIMEOUT_SECONDS)
    except httpx.HTTPError:
        return ''
    return response.text.strip() if response.status_code < _HTTP_ERROR_MIN else ''


def build_launch_options(*, user_data_dir: Path, proxy: str, headless: bool) -> dict[str, Any]:
    """Everything ``launch_persistent_context`` is given, as data.

    Deliberately absent, and each for a reason:

    * ``user_agent`` -- Playwright does not regenerate the Sec-CH-UA client hints to
      match an overridden one, and a UA that disagrees with its own hints is a
      louder signal than the default UA ever was. The real one is read back out of
      the browser for yt-dlp, so the two provably agree.
    * ``--no-sandbox`` -- Playwright already runs Chromium unsandboxed
      (``chromium_sandbox`` defaults to false), which is why the image's root smoke
      test passes. Passing it again would just imply it mattered.
    * stealth patches -- an over-patched prototype is itself detectable.
    """
    return {
        'user_data_dir': str(user_data_dir),
        # Not cosmetic: since Playwright 1.49 a bare `headless=True` prefers the
        # separate chromium-headless-shell build, which is the most identifiable
        # thing in the stack. Naming the channel gets the real browser running in
        # new-headless mode instead.
        'channel': 'chromium',
        'headless': headless,
        'proxy': build_proxy_settings(proxy),
        # A Chinese consumer account browsing in en-US/UTC is a free anomaly. This
        # also lines the browser up with how the archive renders note timestamps.
        'locale': 'zh-CN',
        'timezone_id': 'Asia/Shanghai',
        # Fixed, never randomised: consistency across runs is the signal here.
        'viewport': dict(_DEFAULT_VIEWPORT),
        'ignore_default_args': ['--enable-automation'],
        # /dev/shm is 64 MB in a default container, and Chromium answers that with
        # renderer crashes that read like site failures.
        'args': ['--disable-dev-shm-usage', '--disable-blink-features=AutomationControlled'],
    }


def decode_qr_data_url(src: str) -> bytes:
    """The login QR arrives as a data: URL in the DOM, so no screenshot is needed."""
    value = src.strip()
    if not value.startswith('data:image/') or _QR_DATA_URL_MARKER not in value:
        msg = 'The login modal did not carry a base64 QR image'
        raise ValueError(msg)
    try:
        return base64.b64decode(value.split(_QR_DATA_URL_MARKER, 1)[1], validate=True)
    except (binascii.Error, ValueError) as exc:
        msg = 'The login QR image could not be decoded'
        raise ValueError(msg) from exc


def cursor_of(url: str) -> str:
    """The ``cursor`` a likes-page response was fetched with.

    Used to recognise a page already seen, which also absorbs the duplicate request
    the site's own client sometimes fires for one scroll.
    """
    values = parse_qs(urlsplit(url).query).get('cursor') or ['']
    return values[0]


def browser_cookies_to_netscape(cookies: Sequence[Mapping[str, Any]]) -> str:
    """A cookie jar for yt-dlp, in the format it reads.

    Written to a throwaway file rather than into the profile: yt-dlp rewrites the
    file it is handed, and the profile is Chromium's to own.
    """
    lines = [
        '# Netscape HTTP Cookie File',
        '# https://curl.se/docs/http-cookies.html',
        '# This file was generated from the RedNote browser profile',
    ]
    for cookie in cookies:
        name = str(cookie.get('name') or '')
        if not name:
            continue
        domain = str(cookie.get('domain') or '')
        include_subdomains = 'TRUE' if domain.startswith('.') else 'FALSE'
        secure = 'TRUE' if cookie.get('secure') else 'FALSE'
        expires = cookie.get('expires')
        # A session cookie comes back as -1; the file format spells that 0.
        expiry = int(expires) if isinstance(expires, (int, float)) and expires > 0 else 0
        path = str(cookie.get('path') or '/')
        lines.append(f'{domain}\t{include_subdomains}\t{path}\t{secure}\t{expiry}\t{name}\t{cookie.get("value") or ""}')
    return '\n'.join(lines)


class PlaywrightNoteBrowser:
    """A persistent Chromium context, opened for the length of one crawl."""

    def __init__(self, *, user_data_dir: Path, proxy: str, headless: bool) -> None:
        self._launch_options = build_launch_options(user_data_dir=user_data_dir, proxy=proxy, headless=headless)
        self._user_data_dir = user_data_dir
        self._playwright: Any = None
        self._context: Any = None
        self._page: Any = None
        self._like_pages: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
        self._capture_tasks: set[asyncio.Task[None]] = set()

    async def start(self) -> None:
        try:
            from playwright.async_api import async_playwright  # noqa: PLC0415
        except ImportError as exc:
            msg = 'Install Playwright and its Chromium build before running the RedNote source.'
            raise BrowserDependencyError(msg) from exc

        cleared = await asyncio.to_thread(clear_stale_profile_locks, self._user_data_dir)
        if cleared:
            log.info('Cleared stale Chromium profile locks: %s', ', '.join(cleared))

        self._playwright = await async_playwright().start()
        try:
            self._context = await self._playwright.chromium.launch_persistent_context(**self._launch_options)
            await self._context.add_init_script(_WEBDRIVER_INIT_SCRIPT)
            self._page = self._context.pages[0] if self._context.pages else await self._context.new_page()
            self._page.on('response', self._on_response)
        except Exception:
            await self.aclose()
            raise

    async def aclose(self) -> None:
        for task in list(self._capture_tasks):
            task.cancel()
        self._capture_tasks.clear()
        if self._context is not None:
            with contextlib.suppress(Exception):
                await self._context.close()
            self._context = None
        if self._playwright is not None:
            with contextlib.suppress(Exception):
                await self._playwright.stop()
            self._playwright = None
        self._page = None

    # ---------- request capture ----------

    def _on_response(self, response: Any) -> None:
        if LIKE_PAGE_PATH not in response.url:
            log.debug('RedNote XHR: %s %s', response.status, response.url[:160])
            return
        # The body has to be read while the response is still live, so this is
        # scheduled rather than awaited by the handler.
        task = asyncio.create_task(self._capture_like_page(response))
        self._capture_tasks.add(task)
        task.add_done_callback(self._capture_tasks.discard)

    async def _capture_like_page(self, response: Any) -> None:
        body: Any = None
        with contextlib.suppress(Exception):
            body = await response.json()
        await self._like_pages.put({'url': response.url, 'status': response.status, 'body': body})

    # ---------- NoteBrowser ----------

    async def probe_login(self) -> dict[str, Any]:
        if self._page.url in ('', 'about:blank'):
            await self._page.goto(WEB_ORIGIN, wait_until='domcontentloaded')
        result = await self._page.evaluate(_LOGIN_PROBE_SCRIPT)
        return result if isinstance(result, dict) else {}

    async def cookie_dict(self) -> dict[str, str]:
        cookies = await self._context.cookies()
        return {str(cookie.get('name') or ''): str(cookie.get('value') or '') for cookie in cookies if cookie.get('name')}

    async def netscape_cookies(self) -> str:
        return browser_cookies_to_netscape(await self._context.cookies())

    async def user_agent(self) -> str:
        return str(await self._page.evaluate('() => navigator.userAgent'))

    async def reload_login(self) -> None:
        await self._page.goto(WEB_ORIGIN, wait_until='domcontentloaded')

    async def open_likes(self, *, user_id: str) -> None:
        await self._page.goto(f'{WEB_ORIGIN}/user/profile/{user_id}', wait_until='domcontentloaded')
        # The profile lands on 笔记; the click on 赞过 is what fires the first
        # like/page request, which is the response the crawl is waiting for.
        with contextlib.suppress(Exception):
            await self._page.get_by_text(LIKED_TAB_LABEL, exact=True).first.click(timeout=15_000)

    async def next_like_page(self, *, timeout_seconds: float) -> dict[str, Any] | None:
        deadline = time.monotonic() + timeout_seconds
        while True:
            with contextlib.suppress(TimeoutError):
                return await asyncio.wait_for(self._like_pages.get(), timeout=_PAGE_POLL_SECONDS)
            if time.monotonic() > deadline:
                return None
            await self._scroll_once()

    async def note_state(self, *, note_url: str, note_id: str) -> dict[str, Any]:
        await self._page.goto(note_url, wait_until='domcontentloaded')
        note = await self._page.evaluate(_NOTE_STATE_SCRIPT, note_id)
        return note if isinstance(note, dict) else {}

    async def _scroll_once(self) -> None:
        # Move into the grid first: the wheel is dispatched wherever the pointer is,
        # and it starts at (0, 0) where there is nothing to scroll.
        await self._page.mouse.move(*_GRID_POINTER)
        for _ in range(_SCROLL_STEPS):
            await self._page.mouse.wheel(0, _SCROLL_PIXELS)
            await asyncio.sleep(_SCROLL_PAUSE_SECONDS)

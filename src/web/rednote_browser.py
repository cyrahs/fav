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
    from collections.abc import Mapping
    from pathlib import Path

log = logger.get('rednote')

WEB_ORIGIN = 'https://www.xiaohongshu.com'
LIKE_PAGE_PATH = '/api/sns/web/v1/note/like/page'
# The tab that lists what the account has liked, as the profile page labels it.
# Observed live as 点赞, in a strip reading 笔记 / 收藏 / 点赞. `赞过` is kept behind it
# because it is what the app says and what this was originally written against --
# and because a label that matches nothing fails silently, which is how the first
# version got all the way to a live account before anyone noticed it never clicked.
LIKED_TAB_LABELS = ('点赞', '赞过')

# The captcha wall. Whether it is a crawl being refused or this module's probe being
# turned away, it means the same thing: the address is not one the site will serve.
RISK_CONTROL_STATUS_CODES = frozenset({461, 471})

# Chromium writes these into the profile and stamps them with `hostname-pid`. Every
# pod gets a new hostname, so after a SIGKILL the next one reads them as a lock held
# by another machine and either refuses to start or hangs.
PROFILE_SINGLETON_FILES = ('SingletonLock', 'SingletonCookie', 'SingletonSocket')

_QR_DATA_URL_MARKER = ';base64,'
# How long a freshly loaded page is given to mount and say whether it is signed in.
# Measured against the live site: DOMContentLoaded lands around 1.5s, and the login
# modal follows it.
_LOGIN_RENDER_TIMEOUT_SECONDS = 20.0
_LOGIN_RENDER_POLL_SECONDS = 0.5
_LIKED_TAB_TIMEOUT_MS = 15_000
# Where the site sends a note that no longer exists.
_NOT_FOUND_PATH = '/404'
_SHAPE_MAX_KEYS = 40
# What Chromium puts in its own UA when it has no window, and what the site refuses.
_HEADLESS_UA_TOKEN = 'HeadlessChrome'  # noqa: S105 - a User-Agent token, not a credential
_ACCEPT_LANGUAGE = 'zh-CN,zh;q=0.9'
_PROBE_LENGTH_ONLY_FIELDS = ('qr_src', 'user_id', 'profile_href')
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
    // __INITIAL_STATE__ is Vue's store, so most leaves are refs and the value is
    // behind _value. Reading them raw yields undefined, which used to make every
    // signed-in check fall through to the DOM.
    const unwrap = (value) => (value && typeof value === 'object' && '_value' in value) ? value._value : value;
    const state = window.__INITIAL_STATE__ || {};
    const user = state.user || {};
    const info = unwrap(user.userInfo) || {};
    // A signed-out visitor is still issued a userInfo, carrying a guest id shaped
    // exactly like a real profile id -- 24 hex characters. `guest` is what tells the
    // two apart, and crawling the wrong one would look exactly like success.
    // Only an explicit true disqualifies: `loggedIn` is the signal, and a signed-in
    // payload that simply omits `guest` must not be read as signed out.
    const guest = info.guest === true;
    const loggedIn = unwrap(user.loggedIn) === true && !guest;
    // Signing in takes two scans, and the second one lives in a different DOM tree:
    // the account QR is inside `.login-modal`, and the account-security QR that
    // follows it is inside a separate captcha app. Both use `img.qrcode-img`, so a
    // selector scoped to the login modal sees only the first half of the flow.
    // Anchored on the captcha *app*, not on the modal inside it: the wrapper varies
    // with the theme the site picks -- observed as `.r-captcha-modal` in one context
    // and absent in another, with the same `img.qrcode-img` underneath either way.
    const modal = document.querySelector('.login-modal');
    const verifyModal = document.querySelector('.fe-captcha-app');
    // `img.qrcode-img` is the one constant. Its container is not: observed as a
    // `.login-modal` overlay on /explore, as `.login-modal.full-page` on the
    // standalone /login page the site sometimes redirects to, and as a
    // `.fe-captcha-app` with no modal wrapper at all for the security scan.
    const verifyQr = document.querySelector('.fe-captcha-app img.qrcode-img');
    // The security scan sits on top and is the live step, so it wins when both exist.
    const qr = verifyQr || document.querySelector('img.qrcode-img');
    // Read off the element rather than off which query found it, so a fourth layout
    // still lands in the right stage instead of silently in the wrong one.
    const stage = qr ? (qr.closest('.fe-captcha-app') ? 'verify' : 'login') : '';
    // Scoped to the sidebar deliberately: an unscoped a[href^="/user/profile/"] matches
    // every note author in the feed -- about thirty of them, none of them this account.
    const own = document.querySelector('.side-bar a[href^="/user/profile/"]');
    return {
        logged_in: loggedIn,
        guest: guest,
        has_login_modal: Boolean(modal),
        has_verify_modal: Boolean(verifyModal),
        qr_stage: stage,
        qr_src: (qr && qr.getAttribute('src')) || '',
        user_id: loggedIn ? String(info.userId || '') : '',
        profile_href: (own && own.getAttribute('href')) || '',
        modal_html: (verifyModal || modal) ? (verifyModal || modal).outerHTML.slice(0, 4000) : '',
    };
}"""

# Clicking the image is how the verification QR is reminted -- the modal says so and
# there is no button. Only that one: the login modal's `重新扫码` control is also shown
# while it waits for the phone to confirm a scan that worked, so clicking it there
# would throw away a successful scan.
_REFRESH_VERIFY_QR_SCRIPT = """() => {
    const qr = document.querySelector('.fe-captcha-app img.qrcode-img');
    if (!qr) { return false; }
    qr.click();
    return true;
}"""

# Narrowed inside the page: __INITIAL_STATE__ as a whole is megabytes of feed and
# user state, and only one note is ever wanted.
_NOTE_STATE_SCRIPT = """(id) => {
    const state = window.__INITIAL_STATE__ || {};
    const map = (state.note && state.note.noteDetailMap) || {};
    const entry = map[id];
    // Deliberately no fallback to "whatever note happens to be in the map". This is a
    // single long-lived page, so a note that fails to load leaves the previous one
    // sitting there, and extracting that would file one note's images under another
    // note's id -- rows keyed to media that was never in it, and no way to tell later.
    // Missing means missing; the caller retries the note on the next run.
    return (entry && entry.note) || null;
}"""

# The liked tab arrives with its first screenful already in the page -- server
# rendered, no request involved. Reading only intercepted XHRs therefore starts at
# whatever comes *after* them, which is to say it skips the newest likes: exactly the
# ones an incremental run exists to pick up. The entries are shaped differently from
# the XHR envelope, so they are renamed here to the field names it uses.
_INITIAL_LIKES_SCRIPT = """() => {
    const unwrap = (v) => (v && typeof v === 'object' && '_value' in v) ? v._value : v;
    const user = (window.__INITIAL_STATE__ || {}).user || {};
    const buckets = unwrap(user.notes);
    if (!Array.isArray(buckets)) { return []; }
    const tab = unwrap(user.activeTab) || {};
    // The buckets are per tab and mostly empty; prefer the active one and fall back
    // to whichever has anything, rather than to a hardcoded index.
    const active = Array.isArray(buckets[tab.index]) && buckets[tab.index].length ? buckets[tab.index] : null;
    const bucket = active || buckets.find((b) => Array.isArray(b) && b.length) || [];
    return bucket.map((entry) => {
        const card = (entry && entry.noteCard) || {};
        return {
            note_id: String(card.noteId || (entry && entry.id) || ''),
            xsec_token: String((entry && entry.xsecToken) || card.xsecToken || ''),
            display_title: String(card.displayTitle || ''),
            type: String(card.type || ''),
        };
    }).filter((n) => n.note_id);
}"""

_WEBDRIVER_INIT_SCRIPT = "Object.defineProperty(navigator, 'webdriver', {get: () => undefined});"

_USER_AGENT_HINTS_SCRIPT = """async () => {
    const data = navigator.userAgentData;
    if (!data) { return {}; }
    const high = await data.getHighEntropyValues(['architecture', 'model', 'platformVersion', 'uaFullVersion']);
    return {
        brands: data.brands.map(b => ({brand: b.brand, version: b.version})),
        mobile: data.mobile,
        platform: data.platform,
        ...high,
    };
}"""


class BrowserDependencyError(RuntimeError):
    """Playwright or its Chromium build is not installed."""


class ProxyConfigurationError(ValueError):
    """The configured proxy cannot be expressed to Chromium."""


class NoteGoneError(LookupError):
    """The site said the note does not exist, rather than failing to show it.

    Raised only on the site's own verdict -- the redirect to /404 -- and never on a
    timeout, a navigation error or an empty page. The caller counts these toward
    retiring a note, so anything less than the site saying so would let a bad night
    retire notes that are perfectly fine.
    """


class NoteBrowser(Protocol):
    """What the source needs from a browser. Implemented for real below, faked in tests."""

    async def probe_login(self) -> dict[str, Any]: ...
    async def refresh_qr(self) -> bool: ...
    async def cookie_dict(self) -> dict[str, str]: ...
    async def user_agent(self) -> str: ...
    async def reload_login(self) -> None: ...
    async def open_likes(self, *, user_id: str) -> bool: ...
    async def initial_like_notes(self) -> list[dict[str, Any]]: ...
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

    * ``user_agent`` -- not because the UA is left alone, but because it cannot be
      corrected from here: the browser's own version is only readable once it is
      running. ``_hide_headless_user_agent`` does it over CDP after launch, carrying
      the client hints with it. An earlier revision argued the default was safer than
      any override; measuring it said otherwise, and said so at the login endpoint.
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


def describe_shape(value: Any, *, depth: int = 5) -> Any:
    """Key names and types, never values.

    What is unverified about this source is the *shape* of what the site returns --
    which keys exist, and whether a field is a list or a scalar. What is in those
    fields is the user's own liked notes, so a run that explains itself should say
    ``{'note_id': 'str[24]'}`` and never the id itself.
    """
    if depth <= 0:
        return '...'
    if isinstance(value, dict):
        return {str(key): describe_shape(item, depth=depth - 1) for key, item in list(value.items())[:_SHAPE_MAX_KEYS]}
    if isinstance(value, list):
        # One sample stands for the list: these are homogeneous arrays of notes or
        # image entries, and the second one never says anything the first did not.
        return [f'list[{len(value)}]', describe_shape(value[0], depth=depth - 1)] if value else 'list[0]'
    # bool before str/int: it is a subclass of int, and 'bool' is the more useful answer.
    if isinstance(value, bool):
        return 'bool'
    if isinstance(value, str):
        return f'str[{len(value)}]'
    return 'null' if value is None else type(value).__name__


def redact_probe(probe: Mapping[str, Any]) -> dict[str, Any]:
    """The login probe with the QR image and the account id taken out.

    The QR is kilobytes of base64 and a live credential for as long as it lasts; the
    account id is the user's. Neither belongs in a log, but their presence does.
    """
    redacted: dict[str, Any] = {}
    for key, value in probe.items():
        if key in _PROBE_LENGTH_ONLY_FIELDS:
            redacted[key] = f'str[{len(str(value or ""))}]'
        elif key == 'modal_html':
            continue
        else:
            redacted[key] = value
    return redacted


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
        self._logged_like_shape = False
        self._cdp: Any = None

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
            await self._hide_headless_user_agent()
        except Exception:
            await self.aclose()
            raise

    async def aclose(self) -> None:
        for task in list(self._capture_tasks):
            task.cancel()
        self._capture_tasks.clear()
        if self._cdp is not None:
            with contextlib.suppress(Exception):
                await self._cdp.detach()
            self._cdp = None
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
        if not self._logged_like_shape:
            # Once per run: the envelope this source is built around has never been
            # seen coming back from the site, only inferred.
            self._logged_like_shape = True
            log.debug('RedNote like/page %s shape: %s', response.status, describe_shape(body))
        await self._like_pages.put({'url': response.url, 'status': response.status, 'body': body})

    # ---------- NoteBrowser ----------

    async def probe_login(self) -> dict[str, Any]:
        """Sample the page once it has actually decided what it is showing.

        ``domcontentloaded`` resolves a second or more before this SPA mounts anything:
        the login modal is rendered after hydration, behind a fade transition. Sampling
        the DOM the instant navigation returns reliably sees no modal and no session,
        which reads as "signed out, no QR" -- and the answer to that used to be an
        immediate re-navigation, throwing away the modal that was about to appear.
        """
        if not self._on_site():
            await self._page.goto(WEB_ORIGIN, wait_until='domcontentloaded')

        deadline = time.monotonic() + _LOGIN_RENDER_TIMEOUT_SECONDS
        probe: dict[str, Any] = {}
        while True:
            probe = await self._read_login_probe()
            # Either answer is the page having spoken; anything else is it still loading.
            if probe.get('logged_in') or probe.get('qr_src') or time.monotonic() >= deadline:
                log.debug('RedNote login probe: %s', redact_probe(probe))
                return probe
            await asyncio.sleep(_LOGIN_RENDER_POLL_SECONDS)

    async def _hide_headless_user_agent(self) -> None:
        """Drop the `Headless` token Chromium puts in its own User-Agent.

        Measured against the live site from a residential address, with everything
        else held equal: the default headless UA is answered with HTTP 461 on the
        login endpoints -- `login/qrcode/create` among them, so the site will not
        even mint a QR. The same browser with that one token removed is served
        normally. Headful behaves like the stripped UA, so it is the string being
        judged rather than the absence of a window.

        This overrides `Emulation.setUserAgentOverride` rather than the launch option
        so the real version is read off the browser first and only that token changes
        -- and so the client hints can be carried across with it, which the launch
        option does not do.
        """
        user_agent = str(await self._page.evaluate('() => navigator.userAgent'))
        if _HEADLESS_UA_TOKEN not in user_agent:
            return
        cleaned = user_agent.replace(_HEADLESS_UA_TOKEN, 'Chrome')
        override: dict[str, Any] = {'userAgent': cleaned, 'acceptLanguage': _ACCEPT_LANGUAGE}
        # Only when they can actually be read. The hints never carry the Headless
        # token, so they need no correction -- but sending an empty `brands` blanks
        # `navigator.userAgentData`, and a browser with no brands at all is a stranger
        # signal than the one being fixed.
        metadata = await self._user_agent_metadata(cleaned)
        if metadata.get('brands'):
            override['userAgentMetadata'] = metadata
        # Held open for the life of the browser on purpose: detaching the session
        # reverts every emulation override applied through it, which is how the first
        # version of this set the UA and was still served the old one.
        self._cdp = await self._context.new_cdp_session(self._page)
        await self._cdp.send('Emulation.setUserAgentOverride', override)
        log.info('RedNote hid the Headless token in the browser User-Agent')

    async def _user_agent_metadata(self, user_agent: str) -> dict[str, Any]:
        """The client hints as the browser reports them, with the version it reports.

        Read from somewhere other than ``about:blank``, which is not a secure context
        and therefore has no ``navigator.userAgentData`` at all. Somewhere neutral,
        too: this runs before the UA is corrected, and the one origin that must not
        see the uncorrected string is the site itself.
        """
        if not await self._page.evaluate('() => Boolean(navigator.userAgentData)'):
            with contextlib.suppress(Exception):
                await self._page.goto(_EXIT_IP_URL, wait_until='domcontentloaded')
        hints = await self._page.evaluate(_USER_AGENT_HINTS_SCRIPT)
        from_ua = user_agent.rsplit('Chrome/', maxsplit=1)[-1].split('.', maxsplit=1)[0]
        version = str(hints.get('uaFullVersion') or '').split('.')[0] or from_ua
        return {
            'brands': hints.get('brands') or [],
            'fullVersion': hints.get('uaFullVersion') or '',
            'platform': hints.get('platform') or '',
            'platformVersion': hints.get('platformVersion') or '',
            'architecture': hints.get('architecture') or '',
            'model': hints.get('model') or '',
            'mobile': bool(hints.get('mobile')),
            'majorVersion': version,
        }

    def _on_site(self) -> bool:
        """Whether the page is somewhere this module's selectors mean anything.

        Not just "is it blank": reading the client hints parks the page on a neutral
        origin first, and a check for `about:blank` alone let a login probe run
        against that page and report a signed-in profile as signed out.
        """
        return self._page.url.startswith(WEB_ORIGIN)

    async def _read_login_probe(self) -> dict[str, Any]:
        """One sample, or an empty one if the page moved under it.

        The site routes itself while it settles, and an evaluate that lands during a
        navigation comes back as `Execution context was destroyed` rather than as a
        result. That is a reason to look again on the next poll, not to end the run.
        """
        try:
            result = await self._page.evaluate(_LOGIN_PROBE_SCRIPT)
        except Exception as exc:  # noqa: BLE001
            log.debug('RedNote login probe landed mid-navigation: %s', exc)
            return {}
        return result if isinstance(result, dict) else {}

    async def refresh_qr(self) -> bool:
        """Remint the verification QR. True if there was one to remint."""
        return bool(await self._page.evaluate(_REFRESH_VERIFY_QR_SCRIPT))

    async def cookie_dict(self) -> dict[str, str]:
        cookies = await self._context.cookies()
        return {str(cookie.get('name') or ''): str(cookie.get('value') or '') for cookie in cookies if cookie.get('name')}

    async def user_agent(self) -> str:
        return str(await self._page.evaluate('() => navigator.userAgent'))

    async def reload_login(self) -> None:
        # A reload, not a fresh goto: navigating to the URL the page is already on while
        # the SPA is still bootstrapping is answered with net::ERR_ABORTED, which used
        # to end the run on the second poll of every signed-out wait.
        if not self._on_site():
            await self._page.goto(WEB_ORIGIN, wait_until='domcontentloaded')
            return
        await self._page.reload(wait_until='domcontentloaded')

    async def open_likes(self, *, user_id: str) -> bool:
        """Open the profile and switch to its liked tab. False if the tab was not found.

        The profile lands on 笔记, and the click is what fires the first like/page
        request -- the response the whole crawl waits for. Reported rather than
        suppressed: a tab that is never found produces a run that walks no pages and
        finishes clean, which is indistinguishable from having nothing new to fetch.
        """
        # `?tab=liked` is what the site puts in the address bar once the tab is
        # clicked, so ask for it directly and leave the click as the belt to its
        # braces -- a label can be renamed, a query parameter is the site's own API
        # to itself.
        await self._page.goto(f'{WEB_ORIGIN}/user/profile/{user_id}?tab=liked', wait_until='domcontentloaded')
        for label in LIKED_TAB_LABELS:
            tab = self._page.locator('.reds-tab-item', has_text=label).first
            try:
                await tab.click(timeout=_LIKED_TAB_TIMEOUT_MS)
            except Exception as exc:  # noqa: BLE001
                log.debug('RedNote found no liked tab labelled %s: %s', label, str(exc).splitlines()[0])
                continue
            log.debug('RedNote opened the liked tab via %s', label)
            return True
        log.warning('RedNote could not find the liked tab (tried %s) on the profile page', ', '.join(LIKED_TAB_LABELS))
        return False

    async def initial_like_notes(self) -> list[dict[str, Any]]:
        """The liked notes the page was served with, before any request was made."""
        result = await self._page.evaluate(_INITIAL_LIKES_SCRIPT)
        return [item for item in result if isinstance(item, dict)] if isinstance(result, list) else []

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
        # A note that is gone redirects to /404 rather than rendering an empty one.
        # This is the only positive evidence of deletion available, so it is the only
        # thing raised as such: the caller cannot otherwise tell "deleted" from "did
        # not load in time", and those deserve very different patience.
        if _NOT_FOUND_PATH in urlsplit(self._page.url).path:
            msg = f'RedNote redirected note {note_id} to 404'
            raise NoteGoneError(msg)
        note = await self._page.evaluate(_NOTE_STATE_SCRIPT, note_id)
        return note if isinstance(note, dict) else {}

    async def _scroll_once(self) -> None:
        # Move into the grid first: the wheel is dispatched wherever the pointer is,
        # and it starts at (0, 0) where there is nothing to scroll.
        await self._page.mouse.move(*_GRID_POINTER)
        for _ in range(_SCROLL_STEPS):
            await self._page.mouse.wheel(0, _SCROLL_PIXELS)
            await asyncio.sleep(_SCROLL_PAUSE_SECONDS)

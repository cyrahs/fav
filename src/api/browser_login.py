"""One-off browser logins for the 登录态 page.

A source that needs a signed-in session gets it from a browser the user drives by
hand: the page asks for a login, this module starts a virtual display (TigerVNC's
Xvnc, listening on loopback only) with a real, headed Chromium on it, opens the
site's login page, and lets the user operate it through noVNC in the web UI. The
WebSocket that carries VNC is bridged here, gated by a per-login ticket, so the
VNC port itself never leaves the pod.

The window is a plain Chromium process, not one Playwright drives: nothing attaches
to its pages, injects scripts or enables a debugging domain on them, so to the site
it is an ordinary browser. X refuses sign-ins from a Playwright-controlled window
outright ("temporarily restricted"). The only thing fav asks of the browser is its
cookie jar, over a browser-level DevTools connection on loopback that never touches
a page. RedNote is the exception: its crawl is itself a Playwright browser on the
same profile, presented with an overridden user agent, so its window is that
controlled browser too (``LoginSite.controlled``).

While the window is open the cookies are polled. As soon as the session cookie
appears it is checked against the site itself, and a login that passes is stored
in ``site_sessions`` -- then the browser and the display are shut down again. A
login that never completes is abandoned after a timeout. Nothing stays resident
between logins, which is the whole point over a browser kept running for the
purpose: the session lives on the site's servers, not in a process.

Each site keeps its own Chromium profile under the data volume, so every login
comes from the same "device" rather than a fresh one each time. A site whose crawl
is itself a browser (RedNote) instead opens the crawl's own profile, presented the
way the crawl presents it, and holds the crawl's profile lock while the window is
open: signing in there *is* signing the crawl in, and nothing is copied anywhere.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import os
import secrets
import signal
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any, Protocol
from urllib.parse import urlsplit

import httpx
import websockets
from starlette.websockets import WebSocket, WebSocketDisconnect

from src.core import logger, settings
from src.tool import database, site_sessions
from src.tool.site_sessions import StoredCookie, cookie_expiry, cookie_value, cookies_from_browser, without_values
from src.web import bilibili, bilibili_session, nga_post, pixiv, rednote, rednote_browser, twitter
from src.web.rednote_browser import build_proxy_settings, clear_stale_profile_locks

if TYPE_CHECKING:
    from collections.abc import Awaitable, Callable, Mapping

log = logger.get('browser-login')

STATE_STARTING = 'starting'
STATE_WAITING = 'waiting'
STATE_SAVED = 'saved'
STATE_FAILED = 'failed'
STATE_TIMEOUT = 'timeout'
STATE_CANCELLED = 'cancelled'

DEFAULT_PROFILE_ROOT = Path('./data/login-profiles')
DEFAULT_DISPLAY = 99
DEFAULT_VNC_PORT = 5999
# The plain window's DevTools port, on loopback: how its cookie jar is read.
DEFAULT_DEVTOOLS_PORT = 5998
DEFAULT_TIMEOUT_SECONDS = 15 * 60
DEFAULT_POLL_SECONDS = 2.0
# A cookie that did not verify is asked about again after this long even when its
# value has not changed, so a transient network error does not stall the login.
_REVERIFY_SECONDS = 15.0
_SCREEN_WIDTH = 1280
_SCREEN_HEIGHT = 800
_XVNC_READY_TIMEOUT_SECONDS = 10.0
_PROCESS_STOP_TIMEOUT_SECONDS = 5.0
_GOTO_TIMEOUT_MS = 60_000
# Short, because a verification that reloads holds up the poll loop -- and with it a
# cancel the user is waiting on -- for as long as this.
_RELOAD_TIMEOUT_MS = 10_000
_MESSAGE_MAX_CHARS = 300
_VNC_READ_BYTES = 65536
_WS_POLICY_VIOLATION = 1008
_WS_INTERNAL_ERROR = 1011
# How long the RedNote window's page is given to say what it is showing before a
# page with neither a session nor a login modal is taken for a stale snapshot.
_REDNOTE_SETTLE_SECONDS = 6.0
_REDNOTE_SETTLE_POLL_SECONDS = 0.5


class LoginError(RuntimeError):
    pass


class UnknownLoginSiteError(LoginError):
    pass


class LoginBusyError(LoginError):
    pass


class LoginCheck(Protocol):
    ok: bool
    account: str
    message: str


@dataclass(frozen=True)
class LoginSite:
    key: str
    label: str
    login_url: str
    # URLs whose cookies are read back from the browser.
    cookie_urls: tuple[str, ...]
    # The cookie whose arrival means a login may have happened.
    session_cookie: str
    # Asks the site, from the cookies alone, whether they are a real login.
    verify: Callable[[tuple[StoredCookie, ...]], Awaitable[LoginCheck]] | None = None
    # Asks the login window itself instead, for a site whose cookies must never be
    # presented by any other client (RedNote).
    verify_in_browser: Callable[[LoginBrowser, tuple[StoredCookie, ...]], Awaitable[LoginCheck]] | None = None
    # The egress the source crawls through, so the login comes from the same place.
    proxy: Callable[[], str] = lambda: ''
    # The profile the window opens. Unset, the login page keeps one of its own.
    profile_dir: Callable[[], Path] | None = None
    # An advisory lock held while the window is open, for a profile a crawl opens too.
    lock_name: Callable[[], str] | None = None
    # Run on the page before the login URL is opened, so the window presents the way
    # the crawl does. What it returns is kept for the life of the window.
    prepare_page: Callable[[Any, Any], Awaitable[Any]] | None = None
    # One line for the page while the window is open: how this site's login goes.
    hint: str = ''
    # False for a site whose session lives in its browser profile: site_sessions then
    # records which cookies exist and when they expire, not their values.
    store_cookie_values: bool = True
    # Read from the window once the login is verified, before it is closed: secrets
    # the site keeps outside its cookies (Bilibili's refresh token, in localStorage).
    # Stored with the session as site_sessions' ``extra``.
    after_login: Callable[[LoginBrowser], Awaitable[dict[str, str]]] | None = None

    def __post_init__(self) -> None:
        if self.verify is None and self.verify_in_browser is None:
            msg = f'Login site {self.key} needs a way to verify a login'
            raise ValueError(msg)

    @property
    def controlled(self) -> bool:
        """Whether the window has to be a Playwright browser: only when the login needs its page."""
        return self.verify_in_browser is not None or self.prepare_page is not None


class LoginBrowser(Protocol):
    """What a login needs from a browser. Implemented for real below, faked in tests."""

    @property
    def closed(self) -> bool: ...
    async def open(self, url: str) -> None: ...
    async def cookies(self, urls: tuple[str, ...]) -> list[dict[str, Any]]: ...
    async def evaluate(self, script: str) -> Any: ...
    async def reload(self) -> None: ...
    async def local_storage(self, url: str, key: str) -> str: ...
    async def close(self) -> None: ...


class BrowserLauncher(Protocol):
    async def __call__(  # noqa: PLR0913 - keyword-only, mirrors launch_vnc_browser
        self,
        *,
        profile_dir: Path,
        proxy: str,
        display: int,
        vnc_port: int,
        prepare_page: Callable[[Any, Any], Awaitable[Any]] | None = None,
        controlled: bool = False,
    ) -> LoginBrowser: ...


@dataclass
class LoginAttempt:
    site: str
    ticket: str
    started_at: datetime
    deadline: datetime
    state: str = STATE_STARTING
    message: str = ''
    account: str = ''
    finished_at: datetime | None = None
    cancel: asyncio.Event = field(default_factory=asyncio.Event)

    @property
    def active(self) -> bool:
        return self.finished_at is None


class BrowserLoginManager:
    """At most one login at a time, owned by the API process."""

    def __init__(  # noqa: PLR0913 - every knob is keyword-only and defaulted
        self,
        sites: Mapping[str, LoginSite] | Callable[[], Mapping[str, LoginSite]],
        *,
        launcher: BrowserLauncher | None = None,
        profile_root: Path = DEFAULT_PROFILE_ROOT,
        display: int = DEFAULT_DISPLAY,
        vnc_port: int = DEFAULT_VNC_PORT,
        timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
        poll_seconds: float = DEFAULT_POLL_SECONDS,
        busy_check: Callable[[], Awaitable[str]] | None = None,
    ) -> None:
        # A callable is asked again each time, for sites that follow the settings.
        self._sites = sites
        self.vnc_port = vnc_port
        self._launcher = launcher or launch_vnc_browser
        self._profile_root = profile_root
        self._display = display
        self._timeout = timedelta(seconds=timeout_seconds)
        self._poll_seconds = poll_seconds
        self._busy_check = busy_check
        self._lock = asyncio.Lock()
        self._attempt: LoginAttempt | None = None
        self._task: asyncio.Task[None] | None = None

    @property
    def sites(self) -> dict[str, LoginSite]:
        return dict(self._sites() if callable(self._sites) else self._sites)

    @property
    def attempt(self) -> LoginAttempt | None:
        """The running login, or the last one, so the page can say how it ended."""
        return self._attempt

    def ticket_matches(self, ticket: str) -> bool:
        attempt = self._attempt
        if attempt is None or not attempt.active or not ticket:
            return False
        return secrets.compare_digest(ticket, attempt.ticket)

    async def start(self, site_key: str) -> LoginAttempt:
        site = self.sites.get(site_key)
        if site is None:
            msg = f'Unknown login site: {site_key}'
            raise UnknownLoginSiteError(msg)
        async with self._lock:
            if self._attempt is not None and self._attempt.active:
                msg = f'A {self._attempt.site} login is already open; finish or cancel it first.'
                raise LoginBusyError(msg)
            if self._busy_check is not None:
                reason = await self._busy_check()
                if reason:
                    raise LoginBusyError(reason)
            lock = await self._hold_profile_lock(site)
            now = datetime.now(UTC)
            attempt = LoginAttempt(site=site.key, ticket=secrets.token_urlsafe(32), started_at=now, deadline=now + self._timeout)
            self._attempt = attempt
            self._task = asyncio.create_task(self._run(site, attempt, lock), name=f'browser-login:{site.key}')
        log.info('Browser login started site=%s', site.key)
        return attempt

    async def cancel(self) -> LoginAttempt | None:
        attempt, task = self._attempt, self._task
        if attempt is not None and attempt.active:
            attempt.cancel.set()
            if task is not None:
                with contextlib.suppress(asyncio.CancelledError):
                    await task
        return attempt

    async def close(self) -> None:
        await self.cancel()

    async def _hold_profile_lock(self, site: LoginSite) -> contextlib.AsyncExitStack:
        """Take the profile's lock for the length of the login, or refuse the login.

        Two Chromiums cannot share a profile, and the launcher clears the singleton
        files a killed one leaves behind -- which is only safe while nothing else can
        be using the profile. The lock is the crawl's own, so a crawl that starts
        while the window is open skips its run rather than racing it.
        """
        stack = contextlib.AsyncExitStack()
        if site.lock_name is None:
            return stack
        acquired = await stack.enter_async_context(database.advisory_lock(site.lock_name()))
        if not acquired:
            await stack.aclose()
            msg = f'The {site.label} browser profile is in use by a crawl right now; try again in a few minutes.'
            raise LoginBusyError(msg)
        return stack

    async def _run(self, site: LoginSite, attempt: LoginAttempt, lock: contextlib.AsyncExitStack) -> None:
        browser: LoginBrowser | None = None
        try:
            browser = await self._launcher(
                profile_dir=site.profile_dir() if site.profile_dir is not None else self._profile_root / site.key,
                proxy=site.proxy(),
                display=self._display,
                vnc_port=self.vnc_port,
                prepare_page=site.prepare_page,
                controlled=site.controlled,
            )
            attempt.state = STATE_WAITING
            await browser.open(site.login_url)
            await self._await_login(site, attempt, browser)
        except Exception as exc:
            log.exception('Browser login failed site=%s', site.key)
            attempt.state = STATE_FAILED
            attempt.message = f'{exc.__class__.__name__}: {exc}'[:_MESSAGE_MAX_CHARS]
        finally:
            try:
                if browser is not None:
                    try:
                        await browser.close()
                    except Exception:
                        log.exception('Closing the login browser failed site=%s', site.key)
            finally:
                # Only once the browser is down, but even if closing it was cancelled: the
                # lock is what keeps a crawl out of the profile.
                try:
                    await lock.aclose()
                except Exception:
                    log.exception('Releasing the login profile lock failed site=%s', site.key)
                attempt.finished_at = datetime.now(UTC)
                log.info('Browser login ended site=%s state=%s', site.key, attempt.state)

    async def _await_login(self, site: LoginSite, attempt: LoginAttempt, browser: LoginBrowser) -> None:
        last_value = ''
        last_checked = 0.0
        loop = asyncio.get_running_loop()
        while True:
            if attempt.cancel.is_set():
                attempt.state = STATE_CANCELLED
                return
            if datetime.now(UTC) >= attempt.deadline:
                attempt.state = STATE_TIMEOUT
                return
            if browser.closed:
                attempt.state = STATE_CANCELLED
                attempt.message = 'The browser window was closed.'
                return

            cookies = cookies_from_browser(await browser.cookies(site.cookie_urls))
            value = cookie_value(cookies, site.session_cookie)
            if value and (value != last_value or loop.time() - last_checked >= _REVERIFY_SECONDS):
                last_value, last_checked = value, loop.time()
                check = await self._verify(site, browser, cookies)
                if check.ok:
                    await site_sessions.save_signed_in(
                        site.key,
                        cookies if site.store_cookie_values else without_values(cookies),
                        account=check.account,
                        expires_at=cookie_expiry(cookies, site.session_cookie),
                        extra=await self._after_login(site, browser),
                    )
                    attempt.state = STATE_SAVED
                    attempt.account = check.account
                    attempt.message = ''
                    return
                attempt.message = check.message[:_MESSAGE_MAX_CHARS]

            with contextlib.suppress(TimeoutError):
                await asyncio.wait_for(attempt.cancel.wait(), timeout=self._poll_seconds)

    @staticmethod
    async def _after_login(site: LoginSite, browser: LoginBrowser) -> dict[str, str]:
        if site.after_login is None:
            return {}
        try:
            return await site.after_login(browser)
        except Exception:
            # The login itself is good; only what it could have renewed by itself is lost.
            log.exception('Reading the extras of a login failed site=%s', site.key)
            return {}

    @staticmethod
    async def _verify(site: LoginSite, browser: LoginBrowser, cookies: tuple[StoredCookie, ...]) -> LoginCheck:
        if site.verify_in_browser is not None:
            return await site.verify_in_browser(browser, cookies)
        assert site.verify is not None  # noqa: S101 - LoginSite guarantees one of the two
        return await site.verify(cookies)


# --- The real browser: Xvnc + headed Chromium -------------------------------------------------


def _reap_stale_display(display: int) -> None:
    """Stop an Xvnc an earlier API process left on our display, and drop its lock files.

    The API process can be restarted inside a running container; the display
    number is fav's own, so anything still holding it is a leftover login.
    """
    lock = Path(f'/tmp/.X{display}-lock')  # noqa: S108 - the X server's own lock path
    with contextlib.suppress(OSError, ValueError):
        pid = int(lock.read_text().strip())
        cmdline = Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0', 1)[0]
        if cmdline.endswith(b'Xvnc'):
            os.kill(pid, signal.SIGTERM)
    for path in (lock, Path(f'/tmp/.X11-unix/X{display}')):  # noqa: S108
        path.unlink(missing_ok=True)


async def _wait_for_port(port: int, process: asyncio.subprocess.Process, timeout: float) -> None:  # noqa: ASYNC109
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while loop.time() < deadline:
        if process.returncode is not None:
            msg = f'Xvnc exited with code {process.returncode} before listening'
            raise LoginError(msg)
        try:
            _, writer = await asyncio.open_connection('127.0.0.1', port)
        except OSError:
            await asyncio.sleep(0.2)
            continue
        writer.close()
        with contextlib.suppress(OSError):
            await writer.wait_closed()
        return
    msg = f'Xvnc did not listen on port {port} within {timeout:.0f}s'
    raise LoginError(msg)


async def _stop_process(process: asyncio.subprocess.Process) -> None:
    if process.returncode is not None:
        return
    with contextlib.suppress(ProcessLookupError):
        process.terminate()
    try:
        await asyncio.wait_for(process.wait(), timeout=_PROCESS_STOP_TIMEOUT_SECONDS)
    except TimeoutError:
        with contextlib.suppress(ProcessLookupError):
            process.kill()
        await process.wait()


def build_login_launch_options(*, profile_dir: Path, proxy: str, display: int) -> dict[str, Any]:
    """``launch_persistent_context`` arguments for a headed login window, as data.

    Same posture as the RedNote profile (``rednote_browser.build_launch_options``):
    the real Chromium build, no automation banner, no stealth patches. The window
    fills the virtual screen; with no window manager there is nothing to resize it.
    """
    return {
        'user_data_dir': str(profile_dir),
        'channel': 'chromium',
        'headless': False,
        'proxy': build_proxy_settings(proxy),
        'env': {**os.environ, 'DISPLAY': f':{display}'},
        'locale': 'zh-CN',
        'timezone_id': 'Asia/Shanghai',
        'no_viewport': True,
        'ignore_default_args': ['--enable-automation'],
        'args': [
            '--disable-dev-shm-usage',
            '--disable-blink-features=AutomationControlled',
            f'--window-size={_SCREEN_WIDTH},{_SCREEN_HEIGHT}',
            '--window-position=0,0',
            '--no-first-run',
            '--no-default-browser-check',
        ],
    }


class _VncChromium:
    def __init__(
        self,
        xvnc: asyncio.subprocess.Process,
        playwright: Any,
        context: Any,
        prepare_page: Callable[[Any, Any], Awaitable[Any]] | None = None,
    ) -> None:
        self._xvnc = xvnc
        self._playwright = playwright
        self._context = context
        self._prepare_page = prepare_page
        self._page: Any = None
        # Whatever prepare_page returned (RedNote: the CDP session carrying the UA
        # override, which reverts if it is let go).
        self._prepared: Any = None
        self._closing_tabs: set[asyncio.Task[None]] = set()
        self._closed = False
        context.on('close', lambda _context: self._mark_closed())

    def _mark_closed(self) -> None:
        self._closed = True

    @property
    def closed(self) -> bool:
        return self._closed or self._xvnc.returncode is not None

    async def open(self, url: str) -> None:
        page = self._context.pages[0] if self._context.pages else await self._context.new_page()
        self._page = page
        if self._prepare_page is not None:
            self._prepared = await self._prepare_page(self._context, page)
            # The override is per tab, so a second tab -- a target=_blank link, a popup
            # -- would present the session with the real Linux UA beside the "Mac"
            # one. One tab is all a login needs; any other is closed as it opens.
            self._context.on('page', self._close_extra_tab)
        try:
            await page.goto(url, wait_until='domcontentloaded', timeout=_GOTO_TIMEOUT_MS)
        except Exception as exc:  # noqa: BLE001 - a slow page is still usable by hand
            log.warning('Login page did not finish loading: %s', exc.__class__.__name__)

    def _close_extra_tab(self, page: Any) -> None:
        if page is self._page:
            return
        log.info('Closing a second tab the login window opened')
        task = asyncio.create_task(page.close())
        self._closing_tabs.add(task)
        task.add_done_callback(self._closing_tabs.discard)

    async def evaluate(self, script: str) -> Any:
        return await self._page.evaluate(script)

    async def reload(self) -> None:
        await self._page.reload(wait_until='domcontentloaded', timeout=_RELOAD_TIMEOUT_MS)

    async def local_storage(self, url: str, key: str) -> str:  # noqa: ARG002 - part of the protocol
        msg = 'Reading localStorage is only implemented for the plain login window'
        raise LoginError(msg)

    async def cookies(self, urls: tuple[str, ...]) -> list[dict[str, Any]]:
        if self._closed:
            return []
        return list(await self._context.cookies(list(urls)))

    async def close(self) -> None:
        try:
            if not self._closed:
                await self._context.close()
        finally:
            try:
                await self._playwright.stop()
            finally:
                await _stop_process(self._xvnc)


async def _prepare_profile_and_display(profile_dir: Path, display: int) -> None:
    await asyncio.to_thread(profile_dir.mkdir, parents=True, exist_ok=True)
    cleared = clear_stale_profile_locks(profile_dir)
    if cleared:
        log.info('Cleared stale profile locks %s', ', '.join(cleared))
    _reap_stale_display(display)


async def _start_xvnc(display: int, vnc_port: int) -> asyncio.subprocess.Process:
    xvnc = await asyncio.create_subprocess_exec(
        'Xvnc',
        f':{display}',
        '-geometry',
        f'{_SCREEN_WIDTH}x{_SCREEN_HEIGHT}',
        '-depth',
        '24',
        '-rfbport',
        str(vnc_port),
        '-interface',
        '127.0.0.1',
        '-localhost',
        '-SecurityTypes',
        'None',
        '-AlwaysShared',
        '-nolisten',
        'tcp',
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.DEVNULL,
    )
    try:
        await _wait_for_port(vnc_port, xvnc, _XVNC_READY_TIMEOUT_SECONDS)
    except BaseException:
        await _stop_process(xvnc)
        raise
    return xvnc


async def launch_vnc_browser(  # noqa: PLR0913 - keyword-only
    *,
    profile_dir: Path,
    proxy: str,
    display: int,
    vnc_port: int,
    prepare_page: Callable[[Any, Any], Awaitable[Any]] | None = None,
    controlled: bool = False,
) -> LoginBrowser:
    """The login window for a site: a plain Chromium, or a Playwright one where the site needs its page."""
    if controlled:
        return await launch_vnc_chromium(
            profile_dir=profile_dir, proxy=proxy, display=display, vnc_port=vnc_port, prepare_page=prepare_page
        )
    return await launch_plain_chromium(profile_dir=profile_dir, proxy=proxy, display=display, vnc_port=vnc_port)


async def launch_vnc_chromium(
    *,
    profile_dir: Path,
    proxy: str,
    display: int,
    vnc_port: int,
    prepare_page: Callable[[Any, Any], Awaitable[Any]] | None = None,
) -> LoginBrowser:
    from playwright.async_api import async_playwright  # noqa: PLC0415 - heavy, and only needed for a login

    options = build_login_launch_options(profile_dir=profile_dir, proxy=proxy, display=display)
    await _prepare_profile_and_display(profile_dir, display)
    xvnc = await _start_xvnc(display, vnc_port)
    playwright = None
    try:
        playwright = await async_playwright().start()
        context = await playwright.chromium.launch_persistent_context(**options)
    except BaseException:
        if playwright is not None:
            await playwright.stop()
        await _stop_process(xvnc)
        raise
    return _VncChromium(xvnc, playwright, context, prepare_page)


# --- The plain browser: Xvnc + an undriven Chromium -----------------------------------------------


def build_plain_chromium_command(*, executable: str, profile_dir: Path, proxy: str, devtools_port: int, url: str) -> list[str]:
    """The command line of a login window nothing drives, as data.

    No ``--enable-automation`` and no Playwright: the only automation surface is a
    DevTools port on loopback, which fav uses at the browser level for the cookie
    jar and never to attach to a page. A proxy is given as a server only: without
    a driver there is nobody to answer its credential challenge, so a proxy that
    needs one is refused rather than prompting in the window.
    """
    command = [
        executable,
        f'--user-data-dir={profile_dir}',
        f'--remote-debugging-port={devtools_port}',
        '--remote-debugging-address=127.0.0.1',
        # The pod runs as root, where Chromium refuses to start sandboxed.
        '--no-sandbox',
        '--disable-dev-shm-usage',
        '--no-first-run',
        '--no-default-browser-check',
        # Playwright's choice too, so a profile it wrote before still decrypts.
        '--password-store=basic',
        # Stopping the window is how every login ends; don't offer to restore it.
        '--hide-crash-restore-bubble',
        '--lang=zh-CN',
        f'--window-size={_SCREEN_WIDTH},{_SCREEN_HEIGHT}',
        '--window-position=0,0',
    ]
    proxy_settings = build_proxy_settings(proxy)
    if proxy_settings is not None:
        if proxy_settings.get('username') or proxy_settings.get('password'):
            msg = 'The login window cannot authenticate to a proxy; use one without credentials for this site.'
            raise LoginError(msg)
        command.append(f'--proxy-server={proxy_settings["server"]}')
    else:
        # Direct means direct, whatever proxy the environment happens to carry.
        command.append('--no-proxy-server')
    command.append(url)
    return command


def cookies_for_urls(cookies: list[dict[str, Any]], urls: tuple[str, ...]) -> list[dict[str, Any]]:
    """The cookies a browser would send to any of ``urls``, by domain alone (DevTools returns the whole jar)."""
    hosts = {urlsplit(url).hostname or '' for url in urls}
    selected: list[dict[str, Any]] = []
    for cookie in cookies:
        domain = str(cookie.get('domain') or '')
        bare = domain.lstrip('.')
        if any(host == bare or (domain.startswith('.') and host.endswith(f'.{bare}')) for host in hosts):
            selected.append(cookie)
    return selected


async def read_devtools_cookies(port: int) -> list[dict[str, Any]]:
    """The browser's whole cookie jar, asked of its browser-level DevTools target.

    ``Storage.getCookies`` on the browser target needs no page session, so no page
    is attached to and nothing runs in one: this is what keeps the window an
    ordinary browser to the site being signed in to.
    """
    async with httpx.AsyncClient(trust_env=False, timeout=5) as client:
        version = (await client.get(f'http://127.0.0.1:{port}/json/version')).json()
    async with websockets.connect(str(version['webSocketDebuggerUrl']), max_size=None, proxy=None, open_timeout=5) as socket:
        await socket.send(json.dumps({'id': 1, 'method': 'Storage.getCookies'}))
        while True:
            reply = json.loads(await asyncio.wait_for(socket.recv(), timeout=5))
            if reply.get('id') == 1:
                break
    if 'error' in reply:
        msg = f'DevTools refused Storage.getCookies: {reply["error"]}'
        raise LoginError(msg)
    return list(reply.get('result', {}).get('cookies') or [])


async def _devtools_call(socket: Any, counter: list[int], method: str, params: dict[str, Any], session: str = '') -> dict[str, Any]:
    counter[0] += 1
    message: dict[str, Any] = {'id': counter[0], 'method': method, 'params': params}
    if session:
        message['sessionId'] = session
    await socket.send(json.dumps(message))
    while True:
        reply = json.loads(await asyncio.wait_for(socket.recv(), timeout=10))
        if reply.get('id') == counter[0]:
            break
    if 'error' in reply:
        msg = f'DevTools refused {method}: {reply["error"]}'
        raise LoginError(msg)
    return dict(reply.get('result') or {})


async def read_devtools_local_storage(port: int, url: str, key: str, *, timeout: float = 10) -> str:  # noqa: ASYNC109
    """One localStorage entry of ``url``'s origin, read in a background tab opened for it.

    The tab is the browser's own (``Target.createTarget``), opened on ``url`` --
    best a static file of the site such as robots.txt -- and closed again; the
    tab the user signs in with is never attached to.
    """
    async with httpx.AsyncClient(trust_env=False, timeout=5) as client:
        version = (await client.get(f'http://127.0.0.1:{port}/json/version')).json()
    async with websockets.connect(str(version['webSocketDebuggerUrl']), max_size=None, proxy=None, open_timeout=5) as socket:
        counter = [0]
        target = (await _devtools_call(socket, counter, 'Target.createTarget', {'url': url, 'background': True}))['targetId']
        try:
            session = (await _devtools_call(socket, counter, 'Target.attachToTarget', {'targetId': target, 'flatten': True}))['sessionId']
            expression = (
                f"document.readyState === 'complete' && location.origin === {json.dumps(_origin(url))}"
                f' ? (localStorage.getItem({json.dumps(key)}) || "") : null'
            )
            loop = asyncio.get_running_loop()
            deadline = loop.time() + timeout
            while True:
                result = await _devtools_call(
                    socket, counter, 'Runtime.evaluate', {'expression': expression, 'returnByValue': True}, session
                )
                value = result.get('result', {}).get('value')
                if isinstance(value, str) or loop.time() >= deadline:
                    return value if isinstance(value, str) else ''
                await asyncio.sleep(0.5)
        finally:
            with contextlib.suppress(LoginError, OSError, TimeoutError, websockets.WebSocketException):
                await _devtools_call(socket, counter, 'Target.closeTarget', {'targetId': target})


def _origin(url: str) -> str:
    parts = urlsplit(url)
    return f'{parts.scheme}://{parts.netloc}'


class _PlainChromium:
    def __init__(self, xvnc: asyncio.subprocess.Process, *, executable: str, profile_dir: Path, proxy: str, display: int) -> None:
        self._xvnc = xvnc
        self._executable = executable
        self._profile_dir = profile_dir
        self._proxy = proxy
        self._display = display
        self._chromium: asyncio.subprocess.Process | None = None

    @property
    def closed(self) -> bool:
        if self._xvnc.returncode is not None:
            return True
        return self._chromium is not None and self._chromium.returncode is not None

    async def open(self, url: str) -> None:
        command = build_plain_chromium_command(
            executable=self._executable, profile_dir=self._profile_dir, proxy=self._proxy, devtools_port=DEFAULT_DEVTOOLS_PORT, url=url
        )
        # Its own session, so stopping it reaches the renderers and helpers as well.
        self._chromium = await asyncio.create_subprocess_exec(
            *command,
            env={**os.environ, 'DISPLAY': f':{self._display}', 'TZ': 'Asia/Shanghai'},
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
            start_new_session=True,
        )

    async def cookies(self, urls: tuple[str, ...]) -> list[dict[str, Any]]:
        if self.closed:
            return []
        try:
            jar = await read_devtools_cookies(DEFAULT_DEVTOOLS_PORT)
        except (OSError, httpx.HTTPError, websockets.WebSocketException, TimeoutError, ValueError, KeyError) as exc:
            # Still starting, or between windows; the next poll asks again.
            log.debug('DevTools not answering yet: %s', exc.__class__.__name__)
            return []
        return cookies_for_urls(jar, urls)

    async def evaluate(self, script: str) -> Any:  # noqa: ARG002 - part of the protocol
        msg = 'A plain login window has no page to evaluate in'
        raise LoginError(msg)

    async def reload(self) -> None:
        msg = 'A plain login window cannot be reloaded from here'
        raise LoginError(msg)

    async def local_storage(self, url: str, key: str) -> str:
        if self.closed:
            return ''
        return await read_devtools_local_storage(DEFAULT_DEVTOOLS_PORT, url, key)

    async def close(self) -> None:
        try:
            if self._chromium is not None:
                await _stop_process_group(self._chromium)
        finally:
            await _stop_process(self._xvnc)


async def _stop_process_group(process: asyncio.subprocess.Process) -> None:
    if process.returncode is None:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGTERM)
        try:
            await asyncio.wait_for(process.wait(), timeout=_PROCESS_STOP_TIMEOUT_SECONDS)
        except TimeoutError:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            await process.wait()
    # Helpers outlive the main process for a moment; make sure none stay behind.
    with contextlib.suppress(ProcessLookupError):
        os.killpg(process.pid, signal.SIGKILL)


async def _chromium_executable() -> str:
    from playwright.async_api import async_playwright  # noqa: PLC0415 - only to locate the bundled build

    playwright = await async_playwright().start()
    try:
        return playwright.chromium.executable_path
    finally:
        await playwright.stop()


async def launch_plain_chromium(*, profile_dir: Path, proxy: str, display: int, vnc_port: int) -> LoginBrowser:
    executable = await _chromium_executable()
    # Fails on a proxy the window cannot use before anything is started.
    build_plain_chromium_command(
        executable=executable, profile_dir=profile_dir, proxy=proxy, devtools_port=DEFAULT_DEVTOOLS_PORT, url='about:blank'
    )
    await _prepare_profile_and_display(profile_dir, display)
    xvnc = await _start_xvnc(display, vnc_port)
    return _PlainChromium(xvnc, executable=executable, profile_dir=profile_dir, proxy=proxy, display=display)


# --- VNC over WebSocket -------------------------------------------------------------------------


async def bridge_vnc(websocket: WebSocket, manager: BrowserLoginManager) -> None:
    """Relay noVNC's WebSocket to the loopback VNC port of the running login."""
    if not manager.ticket_matches(websocket.query_params.get('ticket', '')):
        await websocket.close(code=_WS_POLICY_VIOLATION)
        return
    try:
        reader, writer = await asyncio.open_connection('127.0.0.1', manager.vnc_port)
    except OSError:
        await websocket.close(code=_WS_INTERNAL_ERROR)
        return

    requested = websocket.scope.get('subprotocols') or []
    await websocket.accept(subprotocol='binary' if 'binary' in requested else None)

    async def client_to_vnc() -> None:
        while True:
            message = await websocket.receive()
            if message['type'] == 'websocket.disconnect':
                return
            data = message.get('bytes') or (message.get('text') or '').encode()
            if data:
                writer.write(data)
                await writer.drain()

    async def vnc_to_client() -> None:
        while chunk := await reader.read(_VNC_READ_BYTES):
            await websocket.send_bytes(chunk)

    tasks = [asyncio.create_task(client_to_vnc()), asyncio.create_task(vnc_to_client())]
    try:
        await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
    finally:
        await _close_bridge(tasks, writer, websocket)


async def _close_bridge(tasks: list[asyncio.Task[None]], writer: asyncio.StreamWriter, websocket: WebSocket) -> None:
    for task in tasks:
        task.cancel()
    for task in tasks:
        with contextlib.suppress(asyncio.CancelledError, WebSocketDisconnect, OSError, RuntimeError):
            await task
    writer.close()
    with contextlib.suppress(OSError):
        await writer.wait_closed()
    with contextlib.suppress(RuntimeError, OSError):
        await websocket.close()


# --- Sites -------------------------------------------------------------------------------------


async def _verify_pixiv(cookies: tuple[StoredCookie, ...]) -> LoginCheck:
    cfg = settings.load().web.pixiv
    return await pixiv.verify_session(cookie_value(cookies, 'PHPSESSID'), user_id=cfg.user_id, proxy=cfg.proxy)


async def _verify_bilibili(cookies: tuple[StoredCookie, ...]) -> LoginCheck:
    return await bilibili.verify_session(cookies)


# Where the login page may have left the refresh token: the passport page signs in
# on its own origin, the main site's login popup on www. A static file of each
# origin is enough to read its localStorage.
_BILIBILI_STORAGE_URLS = ('https://passport.bilibili.com/robots.txt', 'https://www.bilibili.com/robots.txt')
_BILIBILI_TOKEN_WAIT_SECONDS = 10


async def _bilibili_extras(browser: LoginBrowser) -> dict[str, str]:
    """The refresh token the login page keeps in localStorage, which fav needs to refresh SESSDATA.

    The page writes it as the login answer arrives, about when SESSDATA does, so it
    is asked for a few times before giving up.
    """
    loop = asyncio.get_running_loop()
    deadline = loop.time() + _BILIBILI_TOKEN_WAIT_SECONDS
    while True:
        for url in _BILIBILI_STORAGE_URLS:
            token = await browser.local_storage(url, bilibili_session.REFRESH_TOKEN_STORAGE_KEY)
            if token:
                log.info('Captured the Bilibili refresh token from %s', _origin(url))
                return {bilibili_session.REFRESH_TOKEN_KEY: token}
        if loop.time() >= deadline:
            log.warning('The Bilibili login left no refresh token; fav will not be able to refresh SESSDATA itself')
            return {}
        await asyncio.sleep(1)


async def _verify_nga(cookies: tuple[StoredCookie, ...]) -> LoginCheck:
    return await nga_post.verify_session(cookies)


async def _verify_twitter(cookies: tuple[StoredCookie, ...]) -> LoginCheck:
    cfg = settings.load().web.twitter
    return await twitter.verify_session(cookies, username=cfg.username, proxy=cfg.proxy)


@dataclass(frozen=True)
class _PageCheck:
    ok: bool
    account: str = ''
    message: str = ''


async def _verify_rednote(browser: LoginBrowser, cookies: tuple[StoredCookie, ...]) -> LoginCheck:  # noqa: ARG001 - the page, not the jar, is the authority
    """Ask the login window's own page whether it is signed in.

    Never a second client: RedNote's session is only ever presented by this browser,
    which is the whole design of the source. The page needs a moment after loading
    to show anything, and a page that then shows neither a session nor a login modal
    is what a finished scan leaves behind -- the page's store still holds the
    signed-out snapshot it was loaded with -- so it is reloaded, and the next poll
    reads the answer from the fresh page.
    """
    loop = asyncio.get_running_loop()
    deadline = loop.time() + _REDNOTE_SETTLE_SECONDS
    while True:
        probe = await rednote_browser.read_login_probe(browser)
        if probe.get('logged_in'):
            return _PageCheck(ok=True, account=rednote.user_id_from_probe(probe))
        if probe.get('has_login_modal') or probe.get('has_verify_modal') or probe.get('qr_src'):
            # Still signing in: the account QR, the phone form, or the security scan.
            return _PageCheck(ok=False)
        if loop.time() >= deadline:
            break
        await asyncio.sleep(_REDNOTE_SETTLE_POLL_SECONDS)
    try:
        await browser.reload()
    except Exception as exc:  # noqa: BLE001 - the site navigating itself is the usual cause, and harmless
        log.info('Reloading the RedNote login page was interrupted: %s', exc.__class__.__name__)
    return _PageCheck(ok=False, message='Reloaded the page to confirm the sign-in.')


def _rednote_proxy() -> str:
    cfg = settings.load().web.rednote
    if cfg.validate_runnable():
        # The crawl refuses to leave from the pod's own address, and so does its login.
        msg = 'RedNote has no proxy configured (web.rednote.proxy); set one before signing in.'
        raise LoginError(msg)
    return cfg.proxy


LOGIN_SITES: dict[str, LoginSite] = {
    pixiv.SESSION_SITE: LoginSite(
        key=pixiv.SESSION_SITE,
        label='Pixiv',
        login_url=pixiv.LOGIN_URL,
        cookie_urls=pixiv.SESSION_COOKIE_URLS,
        session_cookie='PHPSESSID',
        verify=_verify_pixiv,
        proxy=lambda: settings.load().web.pixiv.proxy,
    ),
    twitter.SESSION_SITE: LoginSite(
        key=twitter.SESSION_SITE,
        label='X',
        login_url=twitter.LOGIN_URL,
        cookie_urls=twitter.SESSION_COOKIE_URLS,
        session_cookie=twitter.SESSION_COOKIE,
        verify=_verify_twitter,
        proxy=lambda: settings.load().web.twitter.proxy,
        hint='用 X 账号（要和 X 设置里的用户名是同一个）登录；X 多半会再要邮箱或手机验证码，按页面提示输入即可。',  # noqa: RUF001
    ),
    nga_post.SESSION_SITE: LoginSite(
        key=nga_post.SESSION_SITE,
        label='NGA',
        login_url=nga_post.LOGIN_URL,
        cookie_urls=nga_post.SESSION_COOKIE_URLS,
        session_cookie=nga_post.SESSION_COOKIE,
        verify=_verify_nga,
        hint='用 NGA 账号登录（账号密码或手机验证码）；微信源里转发的 NGA 帖子靠这个登录读取。',  # noqa: RUF001
    ),
    rednote.SESSION_SITE: LoginSite(
        key=rednote.SESSION_SITE,
        label='小红书',
        login_url=rednote.LOGIN_URL,
        cookie_urls=(rednote_browser.WEB_ORIGIN,),
        session_cookie=rednote.SESSION_COOKIE,
        verify_in_browser=_verify_rednote,
        proxy=_rednote_proxy,
        profile_dir=lambda: settings.load().web.rednote.profile_path,
        lock_name=lambda: rednote.profile_lock_name(settings.load().web.rednote.profile_path),
        prepare_page=rednote_browser.prepare_page,
        store_cookie_values=False,
        hint='用手机小红书扫画面里的二维码，或用手机号和验证码登录；之后多半还会弹一个安全验证码，同样用 App 扫。',  # noqa: RUF001
    ),
}


def bilibili_login_site(account_name: str) -> LoginSite:
    """One configured Bilibili account's login: each account is its own signed-in user."""
    return LoginSite(
        key=bilibili.session_site(account_name),
        label=f'B 站（{account_name}）',  # noqa: RUF001
        login_url=bilibili.LOGIN_URL,
        cookie_urls=bilibili.SESSION_COOKIE_URLS,
        session_cookie=bilibili.SESSION_COOKIE,
        verify=_verify_bilibili,
        after_login=_bilibili_extras,
        hint=f'用要给账号 {account_name} 抓取的 B 站账号登录：B 站 App 扫码，或手机号加验证码。',  # noqa: RUF001
    )


def login_sites() -> dict[str, LoginSite]:
    """Every site the 登录态 page offers right now: the fixed ones, then one per Bilibili account."""
    sites = dict(LOGIN_SITES)
    for account in settings.load().web.bilibili.accounts:
        site = bilibili_login_site(account.name)
        sites[site.key] = site
    return sites


async def rednote_running() -> str:
    """Refuse a login while the RedNote crawl has its own Chromium up.

    Both browsers share the pod's memory limit; two at once is what would push it
    over, and a login can always wait the few minutes a RedNote run takes.
    """
    try:
        rows = await database.query_db("SELECT 1 FROM control_requests WHERE status = 'running' AND target = 'rednote' LIMIT 1")
    except Exception:  # noqa: BLE001 - the check is a courtesy, not a gate worth failing on
        return ''
    return 'RedNote is crawling with its own browser right now; try again in a few minutes.' if rows else ''

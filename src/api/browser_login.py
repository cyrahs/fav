"""One-off browser logins for the 登录态 page.

A source that needs a signed-in session gets it from a browser the user drives by
hand: the page asks for a login, this module starts a virtual display (TigerVNC's
Xvnc, listening on loopback only) with a real, headed Chromium on it, opens the
site's login page, and lets the user operate it through noVNC in the web UI. The
WebSocket that carries VNC is bridged here, gated by a per-login ticket, so the
VNC port itself never leaves the pod.

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
import os
import secrets
import signal
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any, Protocol

from starlette.websockets import WebSocket, WebSocketDisconnect

from src.core import logger, settings
from src.tool import database, site_sessions
from src.tool.site_sessions import StoredCookie, cookie_expiry, cookie_value, cookies_from_browser
from src.web import pixiv, rednote, rednote_browser
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


class CheckUnsupportedError(LoginError):
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
    # Asks the site, from the cookies alone, whether they are a real login. Also what
    # 检测 runs on a stored session.
    verify: Callable[[tuple[StoredCookie, ...]], Awaitable[LoginCheck]] | None = None
    # Asks the login window itself instead, for a site whose cookies must never be
    # presented by any other client (RedNote). Only usable while a login is open, so
    # such a site offers no 检测; its crawl records the check on every run.
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

    def __post_init__(self) -> None:
        if self.verify is None and self.verify_in_browser is None:
            msg = f'Login site {self.key} needs a way to verify a login'
            raise ValueError(msg)

    @property
    def checkable(self) -> bool:
        return self.verify is not None


class LoginBrowser(Protocol):
    """What a login needs from a browser. Implemented for real below, faked in tests."""

    @property
    def closed(self) -> bool: ...
    async def open(self, url: str) -> None: ...
    async def cookies(self, urls: tuple[str, ...]) -> list[dict[str, Any]]: ...
    async def evaluate(self, script: str) -> Any: ...
    async def reload(self) -> None: ...
    async def close(self) -> None: ...


class BrowserLauncher(Protocol):
    async def __call__(
        self,
        *,
        profile_dir: Path,
        proxy: str,
        display: int,
        vnc_port: int,
        prepare_page: Callable[[Any, Any], Awaitable[Any]] | None = None,
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
        sites: Mapping[str, LoginSite],
        *,
        launcher: BrowserLauncher | None = None,
        profile_root: Path = DEFAULT_PROFILE_ROOT,
        display: int = DEFAULT_DISPLAY,
        vnc_port: int = DEFAULT_VNC_PORT,
        timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
        poll_seconds: float = DEFAULT_POLL_SECONDS,
        busy_check: Callable[[], Awaitable[str]] | None = None,
    ) -> None:
        self.sites = dict(sites)
        self.vnc_port = vnc_port
        self._launcher = launcher or launch_vnc_chromium
        self._profile_root = profile_root
        self._display = display
        self._timeout = timedelta(seconds=timeout_seconds)
        self._poll_seconds = poll_seconds
        self._busy_check = busy_check
        self._lock = asyncio.Lock()
        self._attempt: LoginAttempt | None = None
        self._task: asyncio.Task[None] | None = None

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

    async def check_stored(self, site_key: str) -> LoginCheck | None:
        """Ask the site whether the stored session still works, and record the answer."""
        site = self.sites.get(site_key)
        if site is None:
            msg = f'Unknown login site: {site_key}'
            raise UnknownLoginSiteError(msg)
        if site.verify is None:
            msg = f'{site.label} can only be checked from its own browser; its next crawl records whether the login still works.'
            raise CheckUnsupportedError(msg)
        stored = await site_sessions.load(site.key)
        if stored is None or not stored.cookie(site.session_cookie):
            return None
        check = await site.verify(stored.cookies)
        await site_sessions.record_check(site.key, ok=check.ok, message=check.message)
        return check

    async def _run(self, site: LoginSite, attempt: LoginAttempt, lock: contextlib.AsyncExitStack) -> None:
        browser: LoginBrowser | None = None
        try:
            browser = await self._launcher(
                profile_dir=site.profile_dir() if site.profile_dir is not None else self._profile_root / site.key,
                proxy=site.proxy(),
                display=self._display,
                vnc_port=self.vnc_port,
                prepare_page=site.prepare_page,
            )
            attempt.state = STATE_WAITING
            await browser.open(site.login_url)
            await self._await_login(site, attempt, browser)
        except Exception as exc:
            log.exception('Browser login failed site=%s', site.key)
            attempt.state = STATE_FAILED
            attempt.message = f'{exc.__class__.__name__}: {exc}'[:_MESSAGE_MAX_CHARS]
        finally:
            if browser is not None:
                try:
                    await browser.close()
                except Exception:
                    log.exception('Closing the login browser failed site=%s', site.key)
            # Only once the browser is down: the lock is what keeps a crawl out of the profile.
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
                        site.key, cookies, account=check.account, expires_at=cookie_expiry(cookies, site.session_cookie)
                    )
                    attempt.state = STATE_SAVED
                    attempt.account = check.account
                    attempt.message = ''
                    return
                attempt.message = check.message[:_MESSAGE_MAX_CHARS]

            with contextlib.suppress(TimeoutError):
                await asyncio.wait_for(attempt.cancel.wait(), timeout=self._poll_seconds)

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
        try:
            await page.goto(url, wait_until='domcontentloaded', timeout=_GOTO_TIMEOUT_MS)
        except Exception as exc:  # noqa: BLE001 - a slow page is still usable by hand
            log.warning('Login page did not finish loading: %s', exc.__class__.__name__)

    async def evaluate(self, script: str) -> Any:
        return await self._page.evaluate(script)

    async def reload(self) -> None:
        await self._page.reload(wait_until='domcontentloaded', timeout=_GOTO_TIMEOUT_MS)

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
    await asyncio.to_thread(profile_dir.mkdir, parents=True, exist_ok=True)
    cleared = clear_stale_profile_locks(profile_dir)
    if cleared:
        log.info('Cleared stale profile locks %s', ', '.join(cleared))
    _reap_stale_display(display)

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
    playwright = None
    try:
        await _wait_for_port(vnc_port, xvnc, _XVNC_READY_TIMEOUT_SECONDS)
        playwright = await async_playwright().start()
        context = await playwright.chromium.launch_persistent_context(**options)
    except BaseException:
        if playwright is not None:
            await playwright.stop()
        await _stop_process(xvnc)
        raise
    return _VncChromium(xvnc, playwright, context, prepare_page)


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
        hint='用手机小红书扫画面里的二维码，或用手机号和验证码登录；之后多半还会弹一个安全验证码，同样用 App 扫。',  # noqa: RUF001
    ),
}


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

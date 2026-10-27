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
comes from the same "device" rather than a fresh one each time.
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
from src.web import pixiv
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
    verify: Callable[[tuple[StoredCookie, ...]], Awaitable[LoginCheck]]
    # The egress the source crawls through, so the login comes from the same place.
    proxy: Callable[[], str] = lambda: ''


class LoginBrowser(Protocol):
    """What a login needs from a browser. Implemented for real below, faked in tests."""

    @property
    def closed(self) -> bool: ...
    async def open(self, url: str) -> None: ...
    async def cookies(self, urls: tuple[str, ...]) -> list[dict[str, Any]]: ...
    async def close(self) -> None: ...


class BrowserLauncher(Protocol):
    async def __call__(self, *, profile_dir: Path, proxy: str, display: int, vnc_port: int) -> LoginBrowser: ...


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
            now = datetime.now(UTC)
            attempt = LoginAttempt(site=site.key, ticket=secrets.token_urlsafe(32), started_at=now, deadline=now + self._timeout)
            self._attempt = attempt
            self._task = asyncio.create_task(self._run(site, attempt), name=f'browser-login:{site.key}')
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

    async def check_stored(self, site_key: str) -> LoginCheck | None:
        """Ask the site whether the stored session still works, and record the answer."""
        site = self.sites.get(site_key)
        if site is None:
            msg = f'Unknown login site: {site_key}'
            raise UnknownLoginSiteError(msg)
        stored = await site_sessions.load(site.key)
        if stored is None or not stored.cookie(site.session_cookie):
            return None
        check = await site.verify(stored.cookies)
        await site_sessions.record_check(site.key, ok=check.ok, message=check.message)
        return check

    async def _run(self, site: LoginSite, attempt: LoginAttempt) -> None:
        browser: LoginBrowser | None = None
        try:
            browser = await self._launcher(
                profile_dir=self._profile_root / site.key,
                proxy=site.proxy(),
                display=self._display,
                vnc_port=self.vnc_port,
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
                check = await site.verify(cookies)
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
    def __init__(self, xvnc: asyncio.subprocess.Process, playwright: Any, context: Any) -> None:
        self._xvnc = xvnc
        self._playwright = playwright
        self._context = context
        self._closed = False
        context.on('close', lambda _context: self._mark_closed())

    def _mark_closed(self) -> None:
        self._closed = True

    @property
    def closed(self) -> bool:
        return self._closed or self._xvnc.returncode is not None

    async def open(self, url: str) -> None:
        page = self._context.pages[0] if self._context.pages else await self._context.new_page()
        try:
            await page.goto(url, wait_until='domcontentloaded', timeout=_GOTO_TIMEOUT_MS)
        except Exception as exc:  # noqa: BLE001 - a slow page is still usable by hand
            log.warning('Login page did not finish loading: %s', exc.__class__.__name__)

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


async def launch_vnc_chromium(*, profile_dir: Path, proxy: str, display: int, vnc_port: int) -> LoginBrowser:
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
    return _VncChromium(xvnc, playwright, context)


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

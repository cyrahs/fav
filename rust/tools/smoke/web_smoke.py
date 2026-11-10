"""Page smoke test of the unmodified React front end against the Rust API (WP3).

Starts `fav api` on a fresh temporary database with `web/dist` beside it, seeds a few
rows, signs in through the login screen and opens every page of the front end by its
deep link. It fails when a page throws, logs a console error, or an API call answers
an error, except the calls to routes later work packages own (`PENDING`), which must
answer 404.

    (cd web && npm ci && npm run build)
    (cd rust && cargo build -p fav)
    FAV_TEST_DATABASE_URL=postgresql://tests@127.0.0.1:5432/fav-tests \\
        uv run python rust/tools/smoke/web_smoke.py

`FAV_TEST_DATABASE_URL` names a database the tool may connect to and create databases
from; everything is written to a temporary database that is dropped afterwards.
Chromium comes from `--chromium`, else `FAV_TEST_CHROMIUM`, else Playwright's own.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
import uuid
from pathlib import Path
from typing import TYPE_CHECKING, Any
from urllib.parse import urlsplit, urlunsplit

import psycopg
from playwright.sync_api import sync_playwright

if TYPE_CHECKING:
    from collections.abc import Iterator

    from playwright.sync_api import ConsoleMessage, Page, Request, Response

HERE = Path(__file__).resolve().parent
RUST_DIR = HERE.parents[1]
REPO_ROOT = RUST_DIR.parent
TOKEN = 'token-for-smoke'  # noqa: S105

# Routes the pages call that later work packages port (rust/crates/fav-api/src/openapi.rs).
PENDING = {
    '/api/v2/services/tikhub/test': 'WP13',
}

# Each page by its deep link, and text it shows only once its data arrived.
PAGES = [
    ('/', '已归档'),
    ('/records', 'Smoke test video'),
    ('/records?source=telegram', 'Smoke channel'),
    ('/jobs', 'boom'),
    ('/logins', 'smoke-account'),
    ('/settings', 'Telegram'),
]

SEED_SQL = """
INSERT INTO bilibili (bvid, fav_id, title, upper, created_at) VALUES
    ('BV1smoke0001', 1, 'Smoke test video', 'Smoke uploader', '2026-03-01 10:00:00'),
    ('BV1smoke0002', 1, 'Second smoke video', 'Smoke uploader', '2026-03-01 11:00:00');
INSERT INTO telegram (account_name, message_id, channel_id, title, channel_name, media_type, created_at) VALUES
    ('main', 7, -1001, 'A channel post', 'Smoke channel', 'photo', '2026-03-01 12:00:00');
INSERT INTO control_requests (kind, target, status, requested_at, started_at, finished_at, result, error) VALUES
    ('trigger_job', 'bilibili', 'succeeded', '2026-03-01 10:00:00+00', '2026-03-01 10:00:01+00',
     '2026-03-01 10:00:09+00', 'done', ''),
    ('trigger_job', 'jandan', 'failed', '2026-03-01 11:00:00+00', '2026-03-01 11:00:01+00',
     '2026-03-01 11:00:02+00', '', 'boom');
INSERT INTO site_sessions (site, cookies, account, status, message, signed_in_at, checked_at) VALUES
    ('pixiv', '[]', 'smoke-account', 'valid', '', '2026-03-01 10:00:00+00', '2026-03-01 10:00:00+00');
"""


def _with_database(url: str, name: str) -> str:
    parts = urlsplit(url)
    return urlunsplit((parts.scheme, parts.netloc, f'/{name}', parts.query, parts.fragment))


@contextlib.contextmanager
def _temporary_database(admin_url: str) -> Iterator[str]:
    name = f'fav_smoke_{uuid.uuid4().hex[:12]}'
    with psycopg.connect(admin_url, autocommit=True) as conn:
        conn.execute(f'CREATE DATABASE "{name}"')
    try:
        yield _with_database(admin_url, name)
    finally:
        with psycopg.connect(admin_url, autocommit=True) as conn:
            conn.execute(f'DROP DATABASE IF EXISTS "{name}" WITH (FORCE)')


def _free_port() -> int:
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def _get(url: str, token: str | None = None) -> tuple[int, Any]:
    request = urllib.request.Request(url, headers={'Authorization': f'Bearer {token}'} if token else {})  # noqa: S310
    try:
        with urllib.request.urlopen(request, timeout=5) as response:  # noqa: S310
            return response.status, json.loads(response.read() or b'null')
    except urllib.error.HTTPError as error:
        return error.code, None


def _post(url: str, body: dict[str, Any]) -> int:
    request = urllib.request.Request(  # noqa: S310
        url,
        data=json.dumps(body).encode(),
        method='POST',
        headers={'Authorization': f'Bearer {TOKEN}', 'Content-Type': 'application/json'},
    )
    with urllib.request.urlopen(request, timeout=5) as response:  # noqa: S310
        return response.status


@contextlib.contextmanager
def _fav_api(binary: Path, dist: Path, dsn: str) -> Iterator[str]:
    """`fav api` in a scratch directory whose `web/dist` is the built front end."""
    port = _free_port()
    with tempfile.TemporaryDirectory() as workdir:
        (Path(workdir) / 'web').mkdir()
        (Path(workdir) / 'web' / 'dist').symlink_to(dist.resolve(), target_is_directory=True)
        env = {
            'PATH': os.environ.get('PATH', ''),
            'POSTGRES_DSN': dsn,
            'API_TOKEN': TOKEN,
            'API_BIND': '127.0.0.1',
            'API_PORT': str(port),
        }
        log_path = Path(workdir) / 'fav-api.out'
        with log_path.open('wb') as log:
            process = subprocess.Popen([str(binary.resolve()), 'api'], cwd=workdir, env=env, stdout=log, stderr=subprocess.STDOUT)  # noqa: S603
            base = f'http://127.0.0.1:{port}'
            try:
                deadline = time.monotonic() + 60
                while True:
                    if process.poll() is not None:
                        sys.exit(f'fav api exited with {process.returncode}:\n{log_path.read_text()}')
                    with contextlib.suppress(OSError):
                        if _get(f'{base}/healthz')[0] == 200:  # noqa: PLR2004
                            break
                    if time.monotonic() > deadline:
                        sys.exit(f'fav api did not answer /healthz:\n{log_path.read_text()}')
                    time.sleep(0.2)
                yield base
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                print(log_path.read_text(), end='')


class _Watcher:
    """What the browser saw go wrong: failed API calls, console errors, page errors."""

    def __init__(self, base: str) -> None:
        self.base = base
        self.problems: list[str] = []
        self.seen_pending: set[str] = set()

    def response(self, response: Response) -> None:
        path = urlsplit(response.url).path
        if not response.url.startswith(self.base) or response.status < 400:  # noqa: PLR2004
            return
        if path in PENDING:
            self.seen_pending.add(path)
            if response.status != 404:  # noqa: PLR2004
                self.problems.append(f'{response.request.method} {path}: pending route answered {response.status}')
            return
        self.problems.append(f'{response.request.method} {response.url}: {response.status}')

    def console(self, message: ConsoleMessage) -> None:
        # Failed requests are judged by `response` and `request_failed`; anything else logged as an error is a bug.
        if message.type == 'error' and not message.text.startswith('Failed to load resource'):
            self.problems.append(f'console error: {message.text}')

    def request_failed(self, request: Request) -> None:
        # A request that got no response at all. Leaving a page cancels its pending
        # fetches (`net::ERR_ABORTED`); that is navigation, not a failure.
        failure = request.failure or ''
        if request.url.startswith(self.base) and 'ERR_ABORTED' not in failure:
            self.problems.append(f'{request.method} {request.url}: {failure}')

    def page_error(self, error: Exception) -> None:
        self.problems.append(f'page error: {error}')


def _check_pages(base: str, chromium: str | None) -> list[str]:
    watcher = _Watcher(base)
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=chromium)
        page: Page = browser.new_page()
        page.on('response', watcher.response)
        page.on('console', watcher.console)
        page.on('pageerror', watcher.page_error)
        page.on('requestfailed', watcher.request_failed)

        page.goto(f'{base}/')
        page.fill('input[type=password]', TOKEN)
        page.click('button[type=submit]')
        page.wait_for_selector('nav a[href="/records"]')

        for path, text in PAGES:
            page.goto(f'{base}{path}')
            page.wait_for_load_state('networkidle')
            try:
                page.get_by_text(text, exact=False).first.wait_for(timeout=10_000)
            except Exception as error:  # noqa: BLE001
                watcher.problems.append(f'{path}: {text!r} never appeared ({error.__class__.__name__})')
            else:
                print(f'ok  {path}  ({text!r} shown)')
        browser.close()
    for path in sorted(watcher.seen_pending):
        print(f'ok  {path} is pending ({PENDING[path]}) and answered 404')
    return watcher.problems


def main() -> int | str:
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('--fav', type=Path, default=RUST_DIR / 'target' / 'debug' / 'fav', help='the fav binary')
    parser.add_argument('--dist', type=Path, default=REPO_ROOT / 'web' / 'dist', help='the built front end')
    parser.add_argument('--chromium', default=os.environ.get('FAV_TEST_CHROMIUM') or None, help='a Chromium executable')
    args = parser.parse_args()

    admin_url = os.environ.get('FAV_TEST_DATABASE_URL')
    if not admin_url:
        return 'FAV_TEST_DATABASE_URL is not set'
    if not (args.dist / 'index.html').is_file():
        return f'{args.dist}/index.html is missing; build the front end first'

    with _temporary_database(admin_url) as dsn, _fav_api(args.fav, args.dist, dsn) as base:
        with psycopg.connect(dsn, autocommit=True) as conn:
            conn.execute(SEED_SQL)
        _post(f'{base}/api/v2/job-requests', {'target': 'telegram'})
        problems = _check_pages(base, args.chromium)

    for problem in problems:
        print(f'FAIL {problem}')
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())

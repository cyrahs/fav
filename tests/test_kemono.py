# ruff: noqa: ANN001, ANN003, ARG001, INP001, PLR2004, S101, SLF001

import asyncio
import hashlib

import httpx
import pytest

import src.web.kemono as kemono_module
from src.core import settings
from src.core.settings import KemonoCreator
from src.web.kemono import (
    CrawlRunError,
    Kemono,
    attachment_filename,
    build_file_url,
    collect_new_posts,
    parse_post,
    retry_after_seconds,
    retry_delay_seconds,
    sha256_from_path,
    should_retry_status,
)


def _configure_kemono(**updates: object) -> None:
    """Mutate the pinned settings snapshot that Kemono() reads in __init__."""
    cfg = settings.load().web.kemono
    for key, value in updates.items():
        setattr(cfg, key, value)


def _kemono_job(handler) -> Kemono:
    job = Kemono.__new__(Kemono)
    job.cfg = settings.load().web.kemono
    job.client = httpx.AsyncClient(
        base_url=job.cfg.base_url,
        transport=httpx.MockTransport(handler),
    )
    return job


def _post_payload(post_id: str, *, path: str | None = None, name: str | None = None) -> dict:
    file_obj = {'name': name, 'path': path} if path else {}
    return {
        'id': post_id,
        'user': '123',
        'service': 'fanbox',
        'title': f'post {post_id}',
        'published': '2026-08-17T00:00:00',
        'file': file_obj,
        'attachments': [],
    }


def _sha256_path(content: bytes, ext: str = '.bin') -> str:
    digest = hashlib.sha256(content).hexdigest()
    return f'/{digest[:2]}/{digest[2:4]}/{digest}{ext}'


class _FakeDatabase:
    def __init__(self, select_rows: list[dict] | None = None) -> None:
        self.calls: list[tuple[str, tuple]] = []
        self.select_rows = select_rows or []

    async def query_db(self, sql: str, params: tuple = ()) -> list[dict]:
        self.calls.append((sql, params))
        if sql.lstrip().upper().startswith('SELECT'):
            return self.select_rows
        return []

    def inserted_ids(self) -> list[str]:
        return [params[0] for sql, params in self.calls if 'INSERT' in sql.upper()]


# --- pure functions ---


def test_parse_post_merges_file_and_attachments() -> None:
    data = {
        'id': 12441864,
        'user': 68870973,
        'service': 'fanbox',
        'title': '',
        'published': '2026-08-17T21:59:37',
        'file': {'name': 'main.jpeg', 'path': '/aa/bb/main-hash.jpeg'},
        'attachments': [
            {'name': None, 'path': '/cc/dd/second-hash.png'},
            {'name': 'skipped.zip', 'path': ''},
        ],
    }

    post = parse_post(data)

    assert post.id == '12441864'
    assert post.user == '68870973'
    assert post.title == 'untitled'
    assert [a.name for a in post.attachments] == ['main.jpeg', 'second-hash.png']
    assert [a.path for a in post.attachments] == ['/aa/bb/main-hash.jpeg', '/cc/dd/second-hash.png']


def test_sha256_from_path() -> None:
    digest = 'a' * 64
    assert sha256_from_path(f'/aa/aa/{digest}.jpeg') == digest
    assert sha256_from_path(f'/aa/aa/{digest.upper()}.jpeg') == digest
    assert sha256_from_path('/aa/aa/not-a-hash.jpeg') is None
    assert sha256_from_path(f'/aa/aa/{"a" * 63}.jpeg') is None


def test_attachment_filename() -> None:
    assert attachment_filename('cover.jpeg', 1) == 'cover.jpeg'
    assert attachment_filename('', 3) == 'file_3'
    long_name = 'x' * 300 + '.png'
    result = attachment_filename(long_name, 1)
    assert result.endswith('.png')
    assert len(result.encode()) <= 150 + len('.png')


def test_build_file_url() -> None:
    expected = 'https://file.pawchive.pw/data/aa/bb/hash.jpeg'
    assert build_file_url('https://file.pawchive.pw', '/aa/bb/hash.jpeg') == expected
    assert build_file_url('https://file.pawchive.pw/', '/aa/bb/hash.jpeg') == expected


def test_collect_new_posts() -> None:
    posts = [parse_post(_post_payload(str(i))) for i in (5, 4, 3)]

    fresh, hit_known = collect_new_posts(posts, set())
    assert [p.id for p in fresh] == ['5', '4', '3']
    assert hit_known is False

    fresh, hit_known = collect_new_posts(posts, {'4'})
    assert [p.id for p in fresh] == ['5']
    assert hit_known is True

    fresh, hit_known = collect_new_posts([], {'4'})
    assert fresh == []
    assert hit_known is False


def test_should_retry_status() -> None:
    for status in (429, 502, 503, 504):
        assert should_retry_status(status) is True
    # 403 means blocked on Cloudflare; retrying it is deliberately off the table.
    for status in (403, 404, 500):
        assert should_retry_status(status) is False


def test_retry_after_seconds() -> None:
    assert retry_after_seconds(None) is None
    assert retry_after_seconds(httpx.Response(429)) is None
    assert retry_after_seconds(httpx.Response(429, headers={'Retry-After': '17'})) == 17.0
    assert retry_after_seconds(httpx.Response(429, headers={'Retry-After': 'garbage'})) is None
    # A date in the past clamps to zero rather than going negative.
    past = httpx.Response(429, headers={'Retry-After': 'Mon, 17 Aug 2020 00:00:00 GMT'})
    assert retry_after_seconds(past) == 0.0


def test_retry_delay_seconds() -> None:
    assert retry_delay_seconds(1) == 2.0
    assert retry_delay_seconds(4) == 30.0
    assert retry_delay_seconds(99) == 30.0
    assert retry_delay_seconds(1, retry_after=17.0) == 17.0
    assert retry_delay_seconds(1, retry_after=9999.0) == 300.0


# --- API pagination ---


def test_fetch_new_posts_pages_documented_endpoint() -> None:
    requested: list[str] = []

    async def _handler(request: httpx.Request) -> httpx.Response:
        requested.append(f'{request.url.path}?{request.url.query.decode()}')
        assert request.url.path == '/api/v1/fanbox/user/123'
        offset = int(dict(request.url.params)['o'])
        if offset == 0:
            return httpx.Response(200, json=[_post_payload(str(i)) for i in range(100, 50, -1)])
        return httpx.Response(200, json=[_post_payload(str(i)) for i in range(50, 40, -1)])

    _configure_kemono(sleep_request_seconds=0.0)
    job = _kemono_job(_handler)
    try:
        posts = asyncio.run(job._fetch_new_posts('fanbox', '123', set()))
    finally:
        asyncio.run(job.client.aclose())

    assert len(requested) == 2
    assert posts is not None
    assert [p.id for p in posts] == [str(i) for i in range(100, 40, -1)]


def test_fetch_new_posts_stops_at_known_id() -> None:
    requested: list[str] = []

    async def _handler(request: httpx.Request) -> httpx.Response:
        requested.append(str(request.url))
        return httpx.Response(200, json=[_post_payload(str(i)) for i in range(100, 50, -1)])

    _configure_kemono(sleep_request_seconds=0.0)
    job = _kemono_job(_handler)
    try:
        posts = asyncio.run(job._fetch_new_posts('fanbox', '123', {'98'}))
    finally:
        asyncio.run(job.client.aclose())

    assert len(requested) == 1
    assert posts is not None
    assert [p.id for p in posts] == ['100', '99']


def test_fetch_new_posts_missing_creator() -> None:
    async def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(404, text='not found')

    _configure_kemono(sleep_request_seconds=0.0)
    job = _kemono_job(_handler)
    try:
        posts = asyncio.run(job._fetch_new_posts('fanbox', '123', set()))
    finally:
        asyncio.run(job.client.aclose())

    assert posts is None


# --- downloads ---


def test_download_attachment_verifies_sha256_and_promotes(tmp_path) -> None:
    content = b'attachment-bytes'
    path = _sha256_path(content)

    async def _handler(request: httpx.Request) -> httpx.Response:
        assert request.url.host == 'file.pawchive.pw'
        assert request.url.path == f'/data{path}'
        return httpx.Response(200, content=content)

    _configure_kemono(sleep_request_seconds=0.0)
    job = _kemono_job(_handler)
    attachment = kemono_module.Attachment(name='pic.bin', path=path)
    try:
        ok = asyncio.run(job._download_attachment(attachment, tmp_path, '42', 1))
    finally:
        asyncio.run(job.client.aclose())

    assert ok is True
    assert (tmp_path / 'pic.bin').read_bytes() == content
    assert [p.name for p in tmp_path.iterdir()] == ['pic.bin']


def test_download_attachment_rejects_corrupt_body(tmp_path, monkeypatch) -> None:
    path = _sha256_path(b'expected-bytes')

    async def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=b'corrupted-bytes')

    monkeypatch.setattr(kemono_module, '_API_RETRY_DELAYS_SECONDS', (0.0, 0.0))
    _configure_kemono(sleep_request_seconds=0.0)
    job = _kemono_job(_handler)
    attachment = kemono_module.Attachment(name='pic.bin', path=path)
    try:
        with pytest.raises(RuntimeError, match='sha256 mismatch'):
            asyncio.run(job._download_attachment(attachment, tmp_path, '42', 1))
    finally:
        asyncio.run(job.client.aclose())

    assert list(tmp_path.iterdir()) == []


def test_download_attachment_missing_upstream(tmp_path) -> None:
    async def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(404)

    _configure_kemono(sleep_request_seconds=0.0)
    job = _kemono_job(_handler)
    attachment = kemono_module.Attachment(name='pic.bin', path=_sha256_path(b'x'))
    try:
        ok = asyncio.run(job._download_attachment(attachment, tmp_path, '42', 1))
    finally:
        asyncio.run(job.client.aclose())

    assert ok is False
    assert list(tmp_path.iterdir()) == []


# --- update flow ---


def test_update_aborts_creator_on_transient_failure(tmp_path, monkeypatch) -> None:
    ok_content = b'older-post-bytes'
    ok_path = _sha256_path(ok_content)
    bad_path = '/ee/ff/' + 'e' * 64 + '.bin'

    async def _handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == 'file.pawchive.pw':
            if request.url.path == f'/data{ok_path}':
                return httpx.Response(200, content=ok_content)
            # The newer post's file 500s: not retryable, propagates immediately.
            return httpx.Response(500)
        return httpx.Response(
            200,
            json=[
                _post_payload('3', path=bad_path, name='newer.bin'),
                _post_payload('2', path=ok_path, name='older.bin'),
                _post_payload('1', path=ok_path, name='archived.bin'),
            ],
        )

    fake_db = _FakeDatabase(select_rows=[{'id': '1'}])
    monkeypatch.setattr(kemono_module.database, 'query_db', fake_db.query_db)
    notifications: list[dict] = []

    async def _fake_notify(**kwargs) -> None:
        notifications.append(kwargs)

    monkeypatch.setattr(kemono_module, 'enqueue_notification', _fake_notify)
    _configure_kemono(
        path=tmp_path,
        sleep_request_seconds=0.0,
        creators=[KemonoCreator(service='fanbox', id='123', name='tester')],
    )
    job = _kemono_job(_handler)
    try:
        with pytest.raises(CrawlRunError):
            asyncio.run(job.update())
    finally:
        asyncio.run(job.client.aclose())

    # Only the older post (downloaded before the failure) is recorded; the next
    # run resumes at post 3.
    assert fake_db.inserted_ids() == ['2']
    assert len(notifications) == 1
    assert notifications[0]['payload']['failed_creators'] == ['fanbox/123']


def test_update_reports_missing_creator(tmp_path, monkeypatch) -> None:
    async def _handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(404, text='not found')

    fake_db = _FakeDatabase()
    monkeypatch.setattr(kemono_module.database, 'query_db', fake_db.query_db)
    notifications: list[dict] = []

    async def _fake_notify(**kwargs) -> None:
        notifications.append(kwargs)

    monkeypatch.setattr(kemono_module, 'enqueue_notification', _fake_notify)
    _configure_kemono(
        path=tmp_path,
        sleep_request_seconds=0.0,
        creators=[KemonoCreator(service='fanbox', id='123', name='tester')],
    )
    job = _kemono_job(_handler)
    try:
        asyncio.run(job.update())
    finally:
        asyncio.run(job.client.aclose())

    assert fake_db.inserted_ids() == []
    assert len(notifications) == 1
    assert notifications[0]['payload']['missing_creators'] == ['fanbox/123']
    assert notifications[0]['payload']['failed_creators'] == []


def test_downloaded_post_survives_missing_attachment(tmp_path, monkeypatch) -> None:
    """A file-host 404 must not wedge the creator: the post is still recorded."""

    async def _handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == 'file.pawchive.pw':
            return httpx.Response(404)
        return httpx.Response(200, json=[_post_payload('7', path='/aa/bb/' + 'a' * 64 + '.bin', name='gone.bin')])

    fake_db = _FakeDatabase()
    monkeypatch.setattr(kemono_module.database, 'query_db', fake_db.query_db)
    notifications: list[dict] = []

    async def _fake_notify(**kwargs) -> None:
        notifications.append(kwargs)

    monkeypatch.setattr(kemono_module, 'enqueue_notification', _fake_notify)
    _configure_kemono(
        path=tmp_path,
        sleep_request_seconds=0.0,
        creators=[KemonoCreator(service='fanbox', id='123', name='tester')],
    )
    job = _kemono_job(_handler)
    try:
        asyncio.run(job.update())
    finally:
        asyncio.run(job.client.aclose())

    assert fake_db.inserted_ids() == ['7']
    assert notifications[0]['payload']['missing_files'] == 1

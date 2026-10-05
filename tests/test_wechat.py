# ruff: noqa: ANN001, ANN002, ANN003, ANN202, ARG001, EM101, INP001, S101, S105, S106, TRY003

import asyncio
import json
from pathlib import Path

import httpx
import pytest

import src.service.jobs as jobs_module
import src.web.wechat as wechat_module
from src.api.archive import ARCHIVE_SOURCES, _external_url
from src.api.schemas import JobRequestTarget
from src.api.settings_masking import MASK_SUFFIX, mask_section, unmask_section
from src.core import settings
from src.web.wechat import Wechat, WechatInboxError, file_extension, pending_images, relative_path

# 2025-10-05 03:13:20 UTC, in milliseconds as WeChat stores it.
_CREATE_TIME = 1_759_634_000_000
_JPEG = b'\xff\xd8\xff\xe0original-bytes'


def _configure_wechat(**updates: object) -> settings.Wechat:
    """Mutate the pinned settings snapshot that Wechat() reads in __init__."""
    cfg = settings.load().web.wechat
    for key, value in updates.items():
        setattr(cfg, key, value)
    return cfg


def _item(msg_id: int = 101, **updates: object) -> dict:
    item = {
        'msg_id': msg_id,
        'create_time': _CREATE_TIME + msg_id,
        'type': 'image',
        'talker': 'wxid_sender',
        'sender': 'wxid_sender',
        'md5': 'wechat-md5',
        'file': f'{msg_id}.jpg',
        'status': 'ok',
        'has_file': True,
    }
    item.update(updates)
    return item


class _FakeDatabase:
    """Records the SQL a run issues; a SELECT finds the keys in ``stored``."""

    def __init__(self, stored: set[tuple[int, int]] | None = None) -> None:
        self.calls: list[tuple[str, tuple]] = []
        self.stored = stored or set()

    async def query_db(self, query: str, params: tuple = ()) -> list[dict]:
        self.calls.append((query, params))
        if query.strip().upper().startswith('SELECT'):
            return [{'?column?': 1}] if tuple(params) in self.stored else []
        return []

    def inserts(self) -> list[tuple]:
        return [params for query, params in self.calls if 'INSERT INTO wechat_inbox' in query]


class _FakeInbox:
    """The FavInbox HTTP API, as an httpx handler that remembers what was asked."""

    def __init__(self, items: list, *, files: dict[int, bytes] | None = None, list_status: int = 200) -> None:
        self.items = items
        self.files = files if files is not None else {item['msg_id']: _JPEG for item in items if isinstance(item, dict)}
        self.list_status = list_status
        self.acked: list[int] = []
        self.auth_headers: set[str] = set()

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.auth_headers.add(request.headers.get('authorization', ''))
        path = request.url.path
        if path == '/items':
            return httpx.Response(self.list_status, json=self.items)
        if path.startswith('/file/'):
            msg_id = int(path.removeprefix('/file/'))
            if msg_id not in self.files:
                return httpx.Response(404, json={'error': 'not found'})
            return httpx.Response(200, content=self.files[msg_id], headers={'content-type': 'image/jpeg'})
        if path.startswith('/ack/') and request.method == 'POST':
            self.acked.append(int(path.removeprefix('/ack/')))
            return httpx.Response(200, json={'ok': True})
        return httpx.Response(404)


def _source(inbox: _FakeInbox) -> Wechat:
    source = Wechat()
    headers = source.client.headers
    asyncio.run(source.client.aclose())
    source.client = httpx.AsyncClient(transport=httpx.MockTransport(inbox), headers=headers)
    return source


@pytest.fixture
def fake_db(monkeypatch) -> _FakeDatabase:
    db = _FakeDatabase()
    monkeypatch.setattr(wechat_module, 'database', db)
    return db


@pytest.fixture(autouse=True)
def notifications(monkeypatch) -> list[dict]:
    sent: list[dict] = []

    async def record(**kwargs) -> None:
        sent.append(kwargs)

    monkeypatch.setattr(wechat_module, 'enqueue_notification', record)
    return sent


# ---------- configuration ----------


def test_a_source_without_a_token_is_not_runnable() -> None:
    assert _configure_wechat().validate_runnable() == ['token']


def test_a_configured_source_is_runnable_and_its_url_is_normalised() -> None:
    cfg = settings.Wechat(api_url=' http://favinbox-oracle.proxy.svc/ ', token=' secret ')

    assert cfg.validate_runnable() == []
    assert cfg.api_url == 'http://favinbox-oracle.proxy.svc'
    assert cfg.token == 'secret'


def test_the_token_is_masked_and_survives_a_save_of_the_masked_form() -> None:
    stored = {'api_url': 'http://favinbox-oracle.proxy.svc', 'token': 'abcdef123456'}

    masked = mask_section('web.wechat', stored)
    merged = unmask_section('web.wechat', masked, stored)

    assert masked['token'] == f'abcd{MASK_SUFFIX}'
    assert masked['api_url'] == stored['api_url']
    assert merged == stored


# ---------- selection and naming ----------


def test_only_saved_images_with_a_usable_key_are_downloaded_oldest_first() -> None:
    newer = _item(2, create_time=_CREATE_TIME + 50)
    older = _item(3, create_time=_CREATE_TIME)
    items = [
        newer,
        older,
        _item(4, type='video'),
        _item(5, has_file=False),
        _item(6, status='error: download failed'),
        {**_item(7), 'msg_id': '7'},
        'not an item',
    ]

    assert [item['msg_id'] for item in pending_images(items)] == [3, 2]


def test_a_backfilled_image_counts_as_saved() -> None:
    assert pending_images([_item(1, status='ok:backfill')]) != []


def test_the_extension_comes_from_the_plugin_and_falls_back_to_the_response() -> None:
    assert file_extension(_item(file='1.png'), 'image/jpeg') == 'png'
    assert file_extension(_item(file='1.wxgf'), '') == 'wxgf'
    assert file_extension(_item(file=''), 'image/webp; charset=binary') == 'webp'
    assert file_extension(_item(file='1.exe'), 'application/octet-stream') == 'bin'


def test_files_are_grouped_by_month_and_named_by_send_time_and_id() -> None:
    item = _item(42, create_time=_CREATE_TIME)

    assert relative_path(item, 'jpg') == Path('2025-10/2025-10-05 031320 [42].jpg')


# ---------- runs ----------


def test_a_run_stores_records_and_then_acknowledges_each_image(fake_db, notifications, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([_item(1), _item(2)])

    asyncio.run(_source(inbox).update())

    inserts = fake_db.inserts()
    assert [params[0] for params in inserts] == [1, 2]
    local_path = inserts[0][7]
    assert (tmp_path / local_path).read_bytes() == _JPEG
    assert inserts[0][6] == len(_JPEG)
    assert json.loads(inserts[0][8])['md5'] == 'wechat-md5'
    assert inbox.acked == [1, 2]
    assert inbox.auth_headers == {'Bearer secret'}
    assert not list(tmp_path.rglob('*.part'))
    assert notifications[0]['payload'] == {'downloaded': 2}


def test_an_image_stored_by_an_earlier_run_is_only_acknowledged(fake_db, notifications, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    item = _item(1)
    fake_db.stored.add((item['msg_id'], item['create_time']))
    inbox = _FakeInbox([item])

    asyncio.run(_source(inbox).update())

    assert fake_db.inserts() == []
    assert inbox.acked == [1]
    assert not any(tmp_path.rglob('*.jpg'))
    assert notifications == []


def test_a_failed_download_is_left_pending_and_reported_after_the_rest(fake_db, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([_item(1), _item(2)], files={2: _JPEG})

    with pytest.raises(WechatInboxError) as excinfo:
        asyncio.run(_source(inbox).update())

    assert excinfo.value.notification_dedupe_key == 'wechat:download'
    assert '1' in str(excinfo.value)
    assert [params[0] for params in fake_db.inserts()] == [2]
    assert inbox.acked == [2]


def test_a_rejected_token_fails_the_run_with_a_stable_dedupe_key(fake_db, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='wrong')
    inbox = _FakeInbox([], list_status=401)

    with pytest.raises(WechatInboxError) as excinfo:
        asyncio.run(_source(inbox).update())

    assert excinfo.value.notification_dedupe_key == 'wechat:auth'
    assert 'wrong' not in str(excinfo.value)


def test_a_failed_ack_does_not_fail_the_run(fake_db, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.startswith('/ack/'):
            return httpx.Response(500)
        return _FakeInbox([_item(1)])(request)

    source = Wechat()
    asyncio.run(source.client.aclose())
    source.client = httpx.AsyncClient(transport=httpx.MockTransport(handler))

    asyncio.run(source.update())

    assert [params[0] for params in fake_db.inserts()] == [1]


def test_an_unconfigured_source_does_nothing(monkeypatch) -> None:
    async def _explode(*_args, **_kwargs):
        raise AssertionError('should not query the database')

    monkeypatch.setattr(wechat_module.database, 'query_db', _explode)

    asyncio.run(Wechat().update())


# ---------- registration ----------


def test_the_job_is_registered_and_parked_until_configured() -> None:
    fake_config = settings.Settings()
    fake_config.web.wechat.enabled = True

    job = next(job for job in jobs_module.build_jobs(fake_config) if job.key == 'wechat')

    assert job.name == 'WeChat'
    assert job.section == 'web.wechat'
    assert job.required_commands == ()
    assert job.factory is jobs_module.Wechat
    assert job.enabled is False
    assert 'token' in job.missing_fields


def test_the_api_accepts_wechat_as_a_job_target() -> None:
    assert JobRequestTarget.WECHAT.value == 'wechat'


def test_the_archive_lists_wechat_rows_without_an_external_link() -> None:
    source = ARCHIVE_SOURCES['wechat']

    assert source.id_columns == ('msg_id', 'create_time')
    # The removed iLink source left a `wechat` table with another schema behind.
    assert source.table == 'wechat_inbox'
    assert _external_url(source, {'msg_id': 1, 'create_time': _CREATE_TIME}) is None

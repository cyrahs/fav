# ruff: noqa: ANN001, ANN002, ANN003, ANN202, ARG001, C901, EM101, INP001, PLR0911, PLR0913, PLR2004, S101, S105, S106, TRY003

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
from src.web.wechat import (
    Wechat,
    WechatInboxError,
    article_directory,
    file_extension,
    pending_images,
    queued_message,
    relative_path,
    sniff_extension,
    work_for,
)
from src.web.wechat_article import canonical_url, image_extension, original_image_url, parse_article_link, parse_article_page

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
    """Records the SQL a run issues and keeps just enough state to answer it.

    ``stored`` holds the ``wechat_inbox`` keys, ``articles`` the ``wechat_article``
    ones, ``state`` the ``wechat_state`` rows and ``queue`` the ``wechat_events``.
    """

    def __init__(self, stored: set[tuple[int, int]] | None = None) -> None:
        self.calls: list[tuple[str, tuple]] = []
        self.stored = stored or set()
        self.articles: set[tuple[int, int]] = set()
        self.article_urls: set[str] = set()
        self.state: dict[str, str] = {}
        self.queue: dict[tuple[int, int], dict] = {}

    async def query_db(self, query: str, params: tuple = ()) -> list[dict]:
        self.calls.append((query, params))
        sql = ' '.join(query.split())
        if sql.startswith('SELECT value FROM wechat_state'):
            return [{'value': self.state[params[0]]}] if params[0] in self.state else []
        if sql.startswith('INSERT INTO wechat_state'):
            self.state[params[0]] = params[1]
        elif sql.startswith('INSERT INTO wechat_events'):
            msg_id, create_time, work, seq, message = params
            self.queue.setdefault(
                (msg_id, create_time),
                {
                    'msg_id': msg_id,
                    'create_time': create_time,
                    'work': work,
                    'seq': seq,
                    'message': json.loads(message),
                    'status': 'pending',
                    'attempts': 0,
                    'last_error': '',
                },
            )
        elif sql.startswith('SELECT msg_id, create_time, work, message, attempts FROM wechat_events'):
            rows = sorted(self.queue.values(), key=lambda row: (row['create_time'], row['msg_id']))
            return [dict(row) for row in rows if row['status'] == 'pending']
        elif sql.startswith('UPDATE wechat_events'):
            status, attempts, last_error, msg_id, create_time = params
            self.queue[(msg_id, create_time)].update(status=status, attempts=attempts, last_error=last_error)
        elif sql.startswith('SELECT 1 FROM wechat_inbox'):
            return [{'?column?': 1}] if tuple(params) in self.stored else []
        elif sql.startswith('SELECT 1 FROM wechat_article'):
            msg_id, create_time, url = params
            return [{'?column?': 1}] if (msg_id, create_time) in self.articles or url in self.article_urls else []
        elif sql.startswith('INSERT INTO wechat_inbox'):
            self.stored.add((params[0], params[1]))
        elif sql.startswith('INSERT INTO wechat_article'):
            self.articles.add((params[0], params[1]))
            self.article_urls.add(params[5])
        return []

    def inserts(self, table: str = 'wechat_inbox') -> list[tuple]:
        return [params for query, params in self.calls if f'INSERT INTO {table} ' in query]


class _FakeInbox:
    """The FavInbox HTTP API with the bridge behind /wx/, as an httpx handler that remembers what was asked."""

    def __init__(
        self,
        items: list,
        *,
        files: dict[int, bytes] | None = None,
        list_status: int = 200,
        events: list[dict] | None = None,
        events_status: int = 200,
        images: dict[int, bytes] | None = None,
        post_page: httpx.Response | None = None,
    ) -> None:
        self.items = items
        self.post_page = post_page
        self.files = files if files is not None else {item['msg_id']: _JPEG for item in items if isinstance(item, dict)}
        self.list_status = list_status
        self.events = events or []
        self.last_seq: int | None = None
        self.events_status = events_status
        self.images = images or {}
        self.acked: list[int] = []
        self.auth_headers: set[str] = set()
        self.requests: list[httpx.Request] = []

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        self.auth_headers.add(request.headers.get('authorization', ''))
        path = request.url.path
        if path == '/wx/events':
            if self.events_status != 200:
                return httpx.Response(self.events_status, json={'error': 'bridge unreachable: connection refused'})
            after = int(request.url.params['after'])
            limit = int(request.url.params['limit'])
            last_seq = self.last_seq if self.last_seq is not None else max((e['seq'] for e in self.events), default=0)
            page = [event for event in self.events if event['seq'] > after][:limit]
            return httpx.Response(200, json={'ok': True, 'last_seq': last_seq, 'events': page})
        if path == '/wx/image':
            msg_id = int(request.url.params['msg_id'])
            if msg_id not in self.images:
                return httpx.Response(500, json={'error': 'download failed'})
            return httpx.Response(200, content=self.images[msg_id], headers={'content-type': 'application/octet-stream'})
        if path == '/post/page' and request.method == 'POST':
            return self.post_page or httpx.Response(404, json={'error': 'the post was not in the XWeb cache after 30s'})
        if path == '/items':
            return httpx.Response(self.list_status, json=self.items)
        if path.startswith('/file/'):
            msg_id = int(path.removeprefix('/file/'))
            if msg_id not in self.files:
                return httpx.Response(404, json={'error': 'not found'})
            return httpx.Response(200, content=self.files[msg_id], headers={'content-type': 'image/jpeg'})
        if path.startswith('/ack/') and request.method == 'POST':
            msg_id = int(path.removeprefix('/ack/'))
            if not any(isinstance(item, dict) and item.get('msg_id') == msg_id for item in self.items):
                return httpx.Response(404, json={'error': 'unknown msg_id'})
            self.acked.append(msg_id)
            return httpx.Response(200, json={'ok': True})
        return httpx.Response(404)

    def event_requests(self) -> list[int]:
        return [int(request.url.params['after']) for request in self.requests if request.url.path == '/wx/events']


class _FakeWeb:
    """mp.weixin.qq.com and mmbiz.qpic.cn: pages and images by URL, anything else 404."""

    def __init__(self, responses: dict[str, httpx.Response] | None = None) -> None:
        self.responses = responses or {}
        self.requests: list[httpx.Request] = []

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        # The fragment of a share link (#rd) never leaves the client.
        return self.responses.get(str(request.url.copy_with(fragment=None)), httpx.Response(404))


def _source(inbox, web: _FakeWeb | None = None) -> Wechat:
    source = Wechat()
    headers = source.client.headers
    web_headers = source.web.headers
    asyncio.run(source.aclose())
    source.client = httpx.AsyncClient(transport=httpx.MockTransport(inbox), headers=headers)
    source.web = httpx.AsyncClient(transport=httpx.MockTransport(web or _FakeWeb()), headers=web_headers, follow_redirects=True)
    return source


def _message(msg_id: int, **updates: object) -> dict:
    message = {
        'msg_id': msg_id,
        'type': 3,
        'kind': 'image',
        'chat': 'private',
        'create_time': _CREATE_TIME + msg_id,
        'talker': 'wxid_sender',
        'sender': 'wxid_sender',
        'is_send': False,
        'content': '<msg><img aeskey="secret-aes" cdnbigimgurl="cdn-url" /></msg>',
        'image': {'md5': 'wechat-md5', 'key': 'secret-aes', 'mid_img_url': 'cdn-url'},
    }
    message.update(updates)
    return message


def _event(seq: int, message: dict, kind: str = 'message') -> dict:
    return {'seq': seq, 'kind': kind, 'at': _CREATE_TIME, 'data': message}


_SHARE_URL = 'http://mp.weixin.qq.com/s?__biz=MzA5==&amp;mid=2247&amp;idx=1&amp;sn=abc123&amp;chksm=feed#rd'
_FETCH_URL = 'https://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123&chksm=feed'
_CANONICAL_URL = 'https://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123'
_IMAGE_A = 'https://mmbiz.qpic.cn/sz_mmbiz_jpg/AAA/640?wx_fmt=jpeg&from=appmsg'
_IMAGE_B = 'https://mmbiz.qpic.cn/sz_mmbiz_png/BBB/640?wx_fmt=png'
_PNG = b'\x89PNG\r\n\x1a\nfull-size'
_PAGE = f"""<html><head><meta property="og:title" content="og title" /></head>
<script>var msg_title = '每日Coser分享'.html(false); var nickname = htmlDecode("小叶菌ovo"); var ct = "1759600000";</script>
<img class="avatar" data-src="https://mmbiz.qpic.cn/mmbiz_png/AVATAR/0?wx_fmt=png" />
<div class="rich_media_content" id="js_content">
<p><img class="rich_pages" data-src="{_IMAGE_A.replace('&', '&amp;')}" /></p>
<p><img data-ratio="1" data-src="{_IMAGE_B}" /></p>
<p><img data-src="{_IMAGE_A.replace('&', '&amp;')}" /></p>
</div></html>"""


def _share(url: str = _SHARE_URL, kind: str = '5') -> str:
    return (
        '<msg><appmsg appid="" sdkver="0"><title>每日分享</title><des>今天的</des><action>view</action>'
        f'<type>{kind}</type><url>{url}</url><sourcedisplayname>小叶菌ovo</sourcedisplayname>'
        '<refermsg><type>1</type></refermsg></appmsg><fromusername>wxid_sender</fromusername></msg>'
    )


def _article_message(msg_id: int = 44, **updates: object) -> dict:
    return _message(msg_id, type=49, kind='app', content=_share(), image=None, **updates)


def _article_web(**overrides: httpx.Response) -> _FakeWeb:
    responses = {
        _FETCH_URL: httpx.Response(200, text=_PAGE, headers={'content-type': 'text/html; charset=utf-8'}),
        original_image_url(_IMAGE_A): httpx.Response(200, content=_JPEG, headers={'content-type': 'image/jpeg'}),
        original_image_url(_IMAGE_B): httpx.Response(200, content=_PNG, headers={'content-type': 'image/png'}),
    }
    responses.update(overrides)
    return _FakeWeb(responses)


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
    # One photo notification per image, pointing at the stored original.
    assert [n['kind'] for n in notifications] == ['download_completed', 'download_completed']
    assert notifications[0]['title'] == Path(local_path).name
    assert notifications[0]['payload']['image_path'] == str(tmp_path / local_path)
    assert notifications[0]['payload']['msg_id'] == 1


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


# ---------- bridge events ----------


def test_only_received_private_images_and_article_shares_are_work() -> None:
    assert work_for(_event(1, _message(1))) == 'image'
    assert work_for(_event(2, _article_message(2))) == 'article'
    assert work_for(_event(3, _message(3, is_send=True))) is None
    assert work_for(_event(4, _message(4, chat='group'))) is None
    assert work_for(_event(5, _message(5, chat='official'))) is None
    assert work_for(_event(6, _message(6, kind='text', content='hello'))) is None
    assert work_for(_event(7, _message(7, kind='app', content=_share(kind='6')))) is None
    assert work_for(_event(8, _message(8, create_time=None))) is None
    assert work_for(_event(9, {'wxid': 'wxid_new'}, kind='new_friend')) is None


def test_the_queue_keeps_no_image_key_or_cdn_address() -> None:
    kept = queued_message(_message(1), 'image')

    assert kept['md5'] == 'wechat-md5'
    assert 'secret-aes' not in json.dumps(kept)
    assert 'cdn-url' not in json.dumps(kept)
    assert queued_message(_article_message(2), 'article')['content'] == _share()


def test_a_bridge_image_is_saved_recorded_and_dropped_from_the_inbox(fake_db, notifications, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    # The plugin saved its own copy too; the inbox drain finds it already stored.
    inbox = _FakeInbox([_item(45)], events=[_event(7, _message(45))], images={45: _PNG})

    asyncio.run(_source(inbox).update())

    inserts = fake_db.inserts()
    assert len(inserts) == 1
    local_path = Path(inserts[0][7])
    assert local_path.suffix == '.png'
    assert (tmp_path / local_path).read_bytes() == _PNG
    assert json.loads(inserts[0][8])['md5'] == 'wechat-md5'
    assert inbox.acked == [45, 45]
    assert not [request for request in inbox.requests if request.url.path.startswith('/file/')]
    image_request = next(request for request in inbox.requests if request.url.path == '/wx/image')
    assert dict(image_request.url.params) == {'talker': 'wxid_sender', 'msg_id': '45', 'create_time': str(_CREATE_TIME + 45)}
    assert fake_db.state == {'events_after': '7'}
    assert fake_db.queue[(45, _CREATE_TIME + 45)]['status'] == 'done'
    assert [n['payload']['msg_id'] for n in notifications] == [45]


def test_an_image_the_bridge_cannot_fetch_comes_from_the_inbox(fake_db, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([_item(45)], events=[_event(1, _message(45))])

    asyncio.run(_source(inbox).update())

    assert [params[0] for params in fake_db.inserts()] == [45]
    assert [request.url.path for request in inbox.requests if request.url.path.startswith('/file/')] == ['/file/45']


def test_a_failing_item_is_retried_and_given_up_after_five_runs(fake_db, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([], events=[_event(1, _message(45))])
    key = (45, _CREATE_TIME + 45)

    for run in range(1, 6):
        with pytest.raises(WechatInboxError) as excinfo:
            asyncio.run(_source(inbox).update())
        assert excinfo.value.notification_dedupe_key == 'wechat:work'
        assert fake_db.queue[key]['attempts'] == run
    assert 'gave up' in str(excinfo.value)
    assert fake_db.queue[key]['status'] == 'failed'

    # Given up: the next run leaves it alone and succeeds.
    asyncio.run(_source(inbox).update())
    assert fake_db.queue[key]['attempts'] == 5


def test_the_cursor_moves_on_and_a_restarted_numbering_is_read_again(fake_db, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([], events=[_event(3, _message(1, kind='text')), _event(4, _message(2, kind='text'))])

    asyncio.run(_source(inbox).update())
    asyncio.run(_source(inbox).update())
    assert inbox.event_requests() == [0, 4]

    # The plugin lost its log and counts from 1 again.
    inbox.events = [_event(1, _message(45))]
    inbox.images = {45: _JPEG}
    asyncio.run(_source(inbox).update())

    assert inbox.event_requests()[2:] == [4, 0]
    assert fake_db.state['events_after'] == '1'
    assert [params[0] for params in fake_db.inserts()] == [45]


def test_a_bridge_that_is_down_still_lets_the_inbox_drain(fake_db, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([_item(1)], events_status=502)

    with pytest.raises(WechatInboxError) as excinfo:
        asyncio.run(_source(inbox).update())

    assert excinfo.value.notification_dedupe_key == 'wechat:bridge'
    assert 'connection refused' in str(excinfo.value)
    assert [params[0] for params in fake_db.inserts()] == [1]
    assert inbox.acked == [1]
    assert fake_db.state == {}


# ---------- articles ----------


def test_an_article_share_is_recognised_from_its_appmsg_xml() -> None:
    link = parse_article_link(_share())

    assert link is not None
    assert link.url == _FETCH_URL + '#rd'
    assert link.title == '每日分享'
    assert link.description == '今天的'
    assert link.account == '小叶菌ovo'
    assert canonical_url(link.url) == _CANONICAL_URL
    assert parse_article_link(_share(kind='6')) is None
    assert parse_article_link(_share(url='https://example.com/s?a=1')) is None
    assert parse_article_link('plain text') is None


def test_an_article_page_yields_its_body_images_once_each() -> None:
    page = parse_article_page(_PAGE)

    assert page.title == '每日Coser分享'
    assert page.account == '小叶菌ovo'
    assert page.published_at is not None
    assert page.published_at.year == 2025
    assert page.images == [_IMAGE_A, _IMAGE_B]


def test_an_image_post_takes_its_pictures_from_the_picture_list() -> None:
    page = parse_article_page(
        "var msg_title = 'pics'; picture_page_info_list = [{cdn_url: 'https://mmbiz.qpic.cn/mmbiz_jpg/P1/0?wx_fmt=jpeg'}];",
    )

    assert page.images == ['https://mmbiz.qpic.cn/mmbiz_jpg/P1/0?wx_fmt=jpeg']


def test_a_picture_list_yields_each_picture_and_not_its_nested_urls() -> None:
    page = parse_article_page(_PICTURE_PAGE)

    assert page.title == '九张图'
    assert page.images == _PICTURES
    assert parse_article_page('picture_page_info_list = []; <div id="js_content"></div>').images == []


def test_image_urls_ask_for_the_original_size() -> None:
    assert original_image_url(_IMAGE_A) == 'https://mmbiz.qpic.cn/sz_mmbiz_jpg/AAA/0?wx_fmt=jpeg&from=appmsg'
    assert original_image_url('https://mmbiz.qpic.cn/mmbiz_gif/C/640') == 'https://mmbiz.qpic.cn/mmbiz_gif/C/0'
    assert image_extension(_IMAGE_B, '') == 'png'
    assert image_extension(_IMAGE_A, 'image/webp') == 'webp'
    assert sniff_extension(_PNG) == 'png'
    assert sniff_extension(b'wxgf....') == 'wxgf'
    assert sniff_extension(b'????') is None


def test_an_article_is_saved_with_every_image_at_full_size(fake_db, notifications, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    message = _article_message(44)
    inbox = _FakeInbox([], events=[_event(5, message)])
    web = _article_web()

    asyncio.run(_source(inbox, web).update())

    folder = tmp_path / article_directory(message, '每日Coser分享')
    assert folder.name.endswith('每日Coser分享 [44]')
    assert sorted(path.name for path in folder.iterdir()) == ['01.jpg', '02.png', 'article.html', 'article.json']
    assert (folder / '01.jpg').read_bytes() == _JPEG
    assert (folder / '02.png').read_bytes() == _PNG
    summary = json.loads((folder / 'article.json').read_text(encoding='utf-8'))
    assert summary['url'] == _CANONICAL_URL
    assert summary['account'] == '小叶菌ovo'
    # Straight to the original size, and never with a Referer.
    assert [str(request.url.copy_with(fragment=None)) for request in web.requests] == [
        _FETCH_URL,
        original_image_url(_IMAGE_A),
        original_image_url(_IMAGE_B),
    ]
    assert not any('referer' in request.headers for request in web.requests)
    assert 'Mobile' in web.requests[0].headers['user-agent']

    (row,) = fake_db.inserts('wechat_article')
    assert row[:2] == (44, _CREATE_TIME + 44)
    assert row[5:10] == (_CANONICAL_URL, '每日Coser分享', '小叶菌ovo', '2025-10-04 17:46:40', 2)
    assert row[10] == str(folder.relative_to(tmp_path))
    assert fake_db.queue[(44, _CREATE_TIME + 44)]['status'] == 'done'

    (notification,) = notifications
    assert notification['title'] == '每日Coser分享'
    assert notification['link_url'] == _CANONICAL_URL
    assert notification['body'] == '小叶菌ovo | 2 images | From wxid_sender'
    assert notification['payload']['image_path'] == str(folder / '01.jpg')


def test_an_image_that_will_not_load_at_any_size_is_skipped(fake_db, notifications, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([], events=[_event(5, _article_message(44))])
    web = _article_web(**{original_image_url(_IMAGE_B): httpx.Response(404)})

    asyncio.run(_source(inbox, web).update())

    assert str(web.requests[-1].url) == _IMAGE_B
    (row,) = fake_db.inserts('wechat_article')
    assert row[9] == 1
    assert notifications[0]['body'] == '小叶菌ovo | 1 of 2 images | From wxid_sender'


def test_a_captcha_page_fails_the_article_without_writing_anything(fake_db, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([], events=[_event(5, _article_message(44))])
    captcha = 'https://mp.weixin.qq.com/mp/wappoc_appmsgcaptcha?poc_token=x'
    web = _FakeWeb({_FETCH_URL: httpx.Response(302, headers={'location': captcha}), captcha: httpx.Response(200, text='verify')})

    with pytest.raises(WechatInboxError) as excinfo:
        asyncio.run(_source(inbox, web).update())

    assert 'captcha' in str(excinfo.value)
    assert fake_db.inserts('wechat_article') == []
    assert fake_db.queue[(44, _CREATE_TIME + 44)]['status'] == 'pending'
    assert not any(tmp_path.rglob('*'))


_COVER = 'https://mmbiz.qpic.cn/mmbiz_jpg/COVER/0?wx_fmt=jpeg'
_IMAGE_POST_URL = 'https://mp.weixin.qq.com/s?__biz=MzA5==&amp;mid=2247&amp;idx=1&amp;sn=abc123&amp;scene=1&amp;from_masonry=1&amp;t=1#rd'
_IMAGE_POST_FETCH = 'https://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123&scene=1&from_masonry=1&t=1'


def _image_post(msg_id: int, pictures: int = 9) -> dict:
    share = _share(url=_IMAGE_POST_URL).replace(
        '</appmsg>',
        f'<mmreadershare><itemshowtype>8</itemshowtype><coverpicimageurl>{_COVER}</coverpicimageurl>'
        f'<piccount>{pictures}</piccount></mmreadershare></appmsg>',
    )
    return {**_article_message(msg_id), 'content': share}


def _picture_item(name: str) -> str:
    # As an image post's page writes it: the picture's own cdn_url, escaped for a
    # script string, beside nested objects with cdn_urls of their own.
    return (
        f"{{ width: '2560' * 1, height: '3413' * 1,"
        f" cdn_url: 'https:\\x2f\\x2fmmbiz.qpic.cn\\x2fsz_mmbiz_jpg\\x2f{name}\\x2f0?wx_fmt=jpeg\\x26amp;from=appmsg',"
        f" watermark_info: {{ cdn_url: 'https://mmbiz.qpic.cn/sz_mmbiz_jpg/W{name}/0?wx_fmt=jpeg', width: '0' * 1,"
        f" title: 'a }} [ \\' tricky' }}, live_photo: [], }}"
    )


_PICTURES = [f'https://mmbiz.qpic.cn/sz_mmbiz_jpg/P{n}/0?wx_fmt=jpeg&from=appmsg' for n in range(3)]
_PICTURE_PAGE = (
    '<html><script>var msg_title = \'九张图\';\nvar nickname = htmlDecode("某号");\nvar ct = "1760000000";\n'
    'window.picture_page_info_list = [' + ','.join(_picture_item(f'P{n}') for n in range(3)) + '];\n'
    'if (window.picture_page_info_list.length) {}</script><div id="js_content">caption</div></html>'
)


def _captcha_web(**images: httpx.Response) -> _FakeWeb:
    captcha = 'https://mp.weixin.qq.com/mp/wappoc_appmsgcaptcha?poc_token=x'
    responses = {
        _IMAGE_POST_FETCH: httpx.Response(302, headers={'location': captcha}),
        captcha: httpx.Response(200, text='<script src="https://captcha.gtimg.com/TCaptcha.js"></script>'),
    }
    responses.update(images)
    return _FakeWeb(responses)


def test_an_image_post_share_names_its_cover_and_picture_count() -> None:
    link = parse_article_link(_image_post(46)['content'])

    assert link is not None
    assert (link.show_type, link.picture_count, link.cover_url) == ('8', 9, _COVER)
    assert parse_article_link(_share()).cover_url == ''


def test_an_image_post_behind_the_captcha_is_read_from_wechat(fake_db, notifications, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([], events=[_event(5, _image_post(47, pictures=3))], post_page=httpx.Response(200, text=_PICTURE_PAGE))
    web = _captcha_web(**{url: httpx.Response(200, content=_JPEG, headers={'content-type': 'image/jpeg'}) for url in _PICTURES})

    asyncio.run(_source(inbox, web).update())

    (opened,) = [request for request in inbox.requests if request.url.path == '/post/page']
    assert json.loads(opened.content) == {'url': _IMAGE_POST_FETCH + '#rd', 'wait': 30}
    (row,) = fake_db.inserts('wechat_article')
    assert row[8:10] == ('2025-10-09 08:53:20', 3)
    folder = tmp_path / row[10]
    assert sorted(path.name for path in folder.iterdir()) == ['01.jpg', '02.jpg', '03.jpg', 'article.html', 'article.json']
    assert (folder / 'article.html').read_text(encoding='utf-8') == _PICTURE_PAGE
    assert fake_db.queue[(47, _CREATE_TIME + 47)]['status'] == 'done'
    (notification,) = notifications
    assert notification['title'] == '九张图'
    assert notification['body'] == '某号 | 3 images | From wxid_sender'


def test_an_image_post_wechat_cannot_open_is_retried_before_keeping_its_first_picture(fake_db, notifications, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([], events=[_event(5, _image_post(47))])
    web = _captcha_web(**{_COVER: httpx.Response(200, content=_JPEG, headers={'content-type': 'image/jpeg'})})
    source = _source(inbox, web)

    with pytest.raises(WechatInboxError) as excinfo:
        asyncio.run(source.update())

    assert 'not in the XWeb cache' in str(excinfo.value)
    assert fake_db.inserts('wechat_article') == []
    assert fake_db.queue[(47, _CREATE_TIME + 47)]['status'] == 'pending'

    fake_db.queue[(47, _CREATE_TIME + 47)]['attempts'] = 4
    asyncio.run(source.update())

    (row,) = fake_db.inserts('wechat_article')
    assert row[9] == 1
    folder = tmp_path / row[10]
    assert sorted(path.name for path in folder.iterdir()) == ['01.jpg', 'article.json']
    assert (folder / '01.jpg').read_bytes() == _JPEG
    assert json.loads((folder / 'article.json').read_text(encoding='utf-8'))['picture_count'] == 9
    assert fake_db.queue[(47, _CREATE_TIME + 47)]['status'] == 'done'
    (notification,) = notifications
    assert notification['title'] == '每日分享'
    assert notification['body'] == '小叶菌ovo | 1 of 9 images (WeChat could not open the rest) | From wxid_sender'


def test_a_rejected_token_is_never_settled_with_the_first_picture(fake_db, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([], events=[_event(5, _image_post(47))], post_page=httpx.Response(401, json={'error': 'unauthorized'}))
    web = _captcha_web(**{_COVER: httpx.Response(200, content=_JPEG, headers={'content-type': 'image/jpeg'})})
    source = _source(inbox, web)
    with pytest.raises(WechatInboxError):
        asyncio.run(source.update())
    fake_db.queue[(47, _CREATE_TIME + 47)]['attempts'] = 4

    with pytest.raises(WechatInboxError):
        asyncio.run(source.update())

    assert fake_db.inserts('wechat_article') == []


def test_a_single_picture_post_is_saved_whole(fake_db, notifications, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    page = _PICTURE_PAGE.replace('picture_page_info_list = [', 'picture_page_info_list = [' + _picture_item('ONLY') + '];//', 1)
    inbox = _FakeInbox([], events=[_event(5, _image_post(46, pictures=1))], post_page=httpx.Response(200, text=page))
    only = 'https://mmbiz.qpic.cn/sz_mmbiz_jpg/ONLY/0?wx_fmt=jpeg&from=appmsg'
    web = _captcha_web(**{only: httpx.Response(200, content=_JPEG, headers={'content-type': 'image/jpeg'})})

    asyncio.run(_source(inbox, web).update())

    assert notifications[0]['body'] == '某号 | 1 image | From wxid_sender'


def test_a_second_forward_of_a_saved_article_is_not_saved_again(fake_db, notifications, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    inbox = _FakeInbox([], events=[_event(5, _article_message(44)), _event(6, _article_message(52))])
    web = _article_web()

    asyncio.run(_source(inbox, web).update())

    assert [row[0] for row in fake_db.inserts('wechat_article')] == [44]
    assert fake_db.queue[(52, _CREATE_TIME + 52)]['status'] == 'done'
    assert len(notifications) == 1


def test_an_article_saved_earlier_is_not_fetched_again(fake_db, tmp_path) -> None:
    _configure_wechat(path=tmp_path, token='secret')
    fake_db.articles.add((44, _CREATE_TIME + 44))
    inbox = _FakeInbox([], events=[_event(5, _article_message(44))])
    web = _article_web()

    asyncio.run(_source(inbox, web).update())

    assert web.requests == []
    assert fake_db.queue[(44, _CREATE_TIME + 44)]['status'] == 'done'


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


def test_the_archive_links_articles_to_their_canonical_url() -> None:
    source = ARCHIVE_SOURCES['wechat_article']

    assert source.table == 'wechat_article'
    assert source.id_columns == ('msg_id', 'create_time')
    assert _external_url(source, {'msg_id': 44, 'create_time': _CREATE_TIME, 'url': _CANONICAL_URL}) == _CANONICAL_URL

# ruff: noqa: INP001, S101, S106, ANN001, ANN002, ANN003, ANN202, ARG001, EM101, FBT003, PLR2004, SLF001, TRY003

import asyncio
from contextlib import asynccontextmanager
from pathlib import Path
from types import SimpleNamespace

import pytest
from telethon import events
from telethon.errors import FloodWaitError
from telethon.tl.types import PeerChannel

import src.web.telegram as telegram_module
from src.tool.telegram_queue import TelegramMediaJob
from src.web.telegram import Telegram, TelegramMediaEntry, TelegramSessionUnauthorizedError


class _DummyClient:
    async def get_entity(self, _peer: object) -> SimpleNamespace:
        return SimpleNamespace(username='demo_channel')


class _IterClient(_DummyClient):
    def __init__(self, messages: list[SimpleNamespace]) -> None:
        self._messages = messages

    def iter_messages(
        self,
        _channel: object,
        *,
        limit: int | None = None,
        min_id: int = 0,
        reverse: bool = False,
        wait_time: float | None = None,  # noqa: ARG002
    ):
        messages = [msg for msg in self._messages if int(getattr(msg, 'id', 0) or 0) > min_id]
        if not reverse:
            messages.reverse()
        if limit is not None:
            messages = messages[:limit]

        async def _iter_messages():
            for msg in messages:
                yield msg

        return _iter_messages()


def _make_telegram() -> Telegram:
    tg = Telegram()
    tg.client = _DummyClient()
    return tg


def _account(
    name: str = 'default',
    channel_path: Path | None = None,
    *,
    channels: list[telegram_module.TelegramChannel] | None = None,
    media_types: list[telegram_module.TelegramMediaType] | None = None,
) -> telegram_module.TelegramAccount:
    return telegram_module.TelegramAccount(
        name=name,
        channels=channels
        or [
            telegram_module.TelegramChannel(
                id=123,
                path=channel_path or Path('./collection/telegram/demo_channel'),
                media_types=media_types or ['video'],
            ),
        ],
        api_id=1,
        api_hash='hash',
        session_path=Path('./session'),
    )


def _message(message_id: int, **updates: object) -> SimpleNamespace:
    data: dict[str, object] = {
        'message': '',
        'grouped_id': None,
        'media': None,
        'video': False,
        'photo': False,
        'document': None,
        'sticker': False,
        'peer_id': PeerChannel(123),
    }
    data.update(updates)
    return SimpleNamespace(id=message_id, **data)


def _document_media(mime_type: str, attributes: list[object] | None = None) -> telegram_module.MessageMediaDocument:
    document = SimpleNamespace(mime_type=mime_type, attributes=attributes or [])
    return telegram_module.MessageMediaDocument(document=document)


def _entry(
    message: object,
    title: str,
    media_type: telegram_module.TelegramMediaType = 'video',
) -> TelegramMediaEntry:
    return TelegramMediaEntry(msg=message, filename=title, media_type=media_type)


def _job(
    *,
    message_id: int = 10,
    media_type: telegram_module.TelegramMediaType = 'image',
    attempt_count: int = 1,
) -> TelegramMediaJob:
    return TelegramMediaJob(
        account_name='default',
        channel_id=123,
        message_id=message_id,
        grouped_id=None,
        media_type=media_type,
        title='Title',
        source='event',
        priority=100,
        attempt_count=attempt_count,
    )


def test_get_media_uses_configured_media_types() -> None:
    tg = _make_telegram()
    tg.client = _IterClient(
        [
            _message(1, message='Video Caption', video=True),
            _message(2, message='Image Caption', media=telegram_module.MessageMediaPhoto(photo=object())),
        ],
    )

    video_only = asyncio.run(tg.get_media(SimpleNamespace(), ['video']))
    video_and_image = asyncio.run(tg.get_media(SimpleNamespace(), ['video', 'image']))

    assert [(item.msg.id, item.filename, item.media_type) for item in video_only] == [(1, 'Video Caption', 'video')]
    assert [(item.msg.id, item.filename, item.media_type) for item in video_and_image] == [
        (1, 'Video Caption', 'video'),
        (2, 'Image Caption', 'image'),
    ]


def test_get_media_supports_image_documents_and_skips_stickers_and_previews() -> None:
    sticker_attr = type('DocumentAttributeSticker', (), {})()
    tg = _make_telegram()
    tg.client = _IterClient(
        [
            _message(1, media=_document_media('image/png')),
            _message(2, media=_document_media('image/webp', [sticker_attr])),
            _message(3, message='Link preview', photo=object()),
            _message(4, message='Link preview', document=SimpleNamespace(mime_type='image/png', attributes=[])),
        ],
    )

    media = asyncio.run(tg.get_media(SimpleNamespace(), ['image']))

    assert [(item.msg.id, item.filename, item.media_type) for item in media] == [(1, 'image_1', 'image')]


def test_album_uses_caption_and_indexes_mixed_media() -> None:
    tg = _make_telegram()
    scan = tg._build_scan_from_messages(
        [
            _message(1, message='Album Caption', grouped_id=99, media=telegram_module.MessageMediaPhoto(photo=object())),
            _message(2, grouped_id=99, video=True),
            _message(3, grouped_id=99, media=_document_media('application/octet-stream')),
        ],
        ['image', 'video'],
    )

    assert [(item.msg.id, item.filename, item.media_type) for item in scan.media] == [
        (1, 'Album Caption-1', 'image'),
        (2, 'Album Caption-2', 'video'),
    ]


def test_image_notification_includes_local_attachment(monkeypatch, tmp_path) -> None:
    tg = _make_telegram()
    notifications: list[dict[str, object]] = []

    async def _fake_enqueue_notification(**kwargs) -> None:
        notifications.append(kwargs)

    monkeypatch.setattr(telegram_module, 'enqueue_notification', _fake_enqueue_notification)
    image_path = tmp_path / 'image.jpg'
    asyncio.run(
        tg._notify_download(
            account_name='default',
            channel_name='demo',
            message_id=1,
            title='Image',
            media_type='image',
            saved_path=image_path,
        ),
    )
    asyncio.run(
        tg._notify_download(
            account_name='default',
            channel_name='demo',
            message_id=2,
            title='Video',
            media_type='video',
            saved_path=tmp_path / 'video.mp4',
        ),
    )

    assert notifications[0]['payload']['image_path'] == str(image_path)
    assert 'image_path' not in notifications[1]['payload']


def test_initialize_tables_creates_media_backfill_state(monkeypatch) -> None:
    tg = _make_telegram()
    statements: list[str] = []

    async def _fake_query_db_multi(sql: str) -> list[dict[str, object]]:
        statements.append(sql)
        return []

    monkeypatch.setattr(telegram_module.database, 'query_db_multi', _fake_query_db_multi)
    monkeypatch.setattr(telegram_module, 'ensure_telegram_media_queue_table', lambda: asyncio.sleep(0))

    asyncio.run(tg._initialize_tables())

    assert 'CREATE TABLE IF NOT EXISTS telegram_channel_media_backfill' in statements[0]
    assert 'PRIMARY KEY (account_name, channel_id, media_type)' in statements[0]


def test_split_routes_reconcile_channel_once_and_stop_cursor_at_limit(monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(
        channels=[
            telegram_module.TelegramChannel(id=123, path=Path('./collection/image'), media_types=['image']),
            telegram_module.TelegramChannel(id=123, path=Path('./collection/video'), media_types=['video']),
        ],
    )
    queued: list[tuple[int, str]] = []
    cursors: list[int] = []
    scan_calls: list[dict[str, object]] = []
    monkeypatch.setattr(
        telegram_module,
        'cfg',
        telegram_module.cfg.model_copy(update={'scan_limit': 10, 'download_limit_per_channel': 2, 'history_wait_seconds': 0.5}),
    )

    async def _fake_state(*_args) -> telegram_module.TelegramChannelState:
        return telegram_module.TelegramChannelState(last_scanned_message_id=100, has_scan_state=True)

    async def _fake_scan(_channel, media_types, **kwargs) -> telegram_module.TelegramChannelScan:
        scan_calls.append({'media_types': media_types, **kwargs})
        return telegram_module.TelegramChannelScan(
            media=[
                _entry(_message(101), 'One', 'video'),
                _entry(_message(102), 'Two', 'image'),
                _entry(_message(103), 'Three', 'video'),
            ],
            max_message_id=104,
            scanned_message_count=4,
        )

    async def _fake_downloaded(*_args, **_kwargs) -> list[int]:
        return []

    async def _fake_enqueue(**kwargs) -> bool:
        queued.append((kwargs['message_id'], kwargs['source']))
        return True

    async def _fake_cursor(_account_name: str, _channel_id: int, message_id: int) -> None:
        cursors.append(message_id)

    monkeypatch.setattr(tg, 'get_channel_state', _fake_state)
    monkeypatch.setattr(tg, 'get_backfilled_media_types', lambda *_args: asyncio.sleep(0, result={'image', 'video'}))
    monkeypatch.setattr(tg, 'scan_media', _fake_scan)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_downloaded)
    monkeypatch.setattr(tg, 'set_channel_last_scanned_message_id', _fake_cursor)
    monkeypatch.setattr(telegram_module, 'enqueue_telegram_media_job', _fake_enqueue)

    asyncio.run(tg._reconcile_account(account, tg.client))

    assert queued == [(101, 'reconciliation'), (102, 'reconciliation')]
    assert cursors == [102]
    assert scan_calls == [
        {
            'media_types': ['image', 'video'],
            'client': tg.client,
            'min_message_id': 100,
            'limit': 10,
            'wait_time': 0.5,
            'newest_window': False,
        },
    ]


def test_reconciliation_enqueues_whole_album_before_advancing_cursor(monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(media_types=['image'])
    queued: list[int] = []
    cursors: list[int] = []
    monkeypatch.setattr(
        telegram_module,
        'cfg',
        telegram_module.cfg.model_copy(update={'download_limit_per_channel': 1}),
    )

    async def _fake_state(*_args) -> telegram_module.TelegramChannelState:
        return telegram_module.TelegramChannelState(last_scanned_message_id=10, has_scan_state=True)

    async def _fake_scan(*_args, **_kwargs) -> telegram_module.TelegramChannelScan:
        messages = [
            _message(11, grouped_id=50, media=telegram_module.MessageMediaPhoto(photo=object())),
            _message(12, grouped_id=50, media=telegram_module.MessageMediaPhoto(photo=object())),
        ]
        return telegram_module.TelegramChannelScan(
            media=[_entry(messages[0], 'Album-1', 'image'), _entry(messages[1], 'Album-2', 'image')],
            max_message_id=12,
            scanned_message_count=2,
        )

    async def _fake_enqueue(**kwargs) -> bool:
        queued.append(kwargs['message_id'])
        return True

    async def _fake_cursor(_account_name: str, _channel_id: int, message_id: int) -> None:
        cursors.append(message_id)

    monkeypatch.setattr(tg, 'get_channel_state', _fake_state)
    monkeypatch.setattr(tg, 'get_backfilled_media_types', lambda *_args: asyncio.sleep(0, result={'image'}))
    monkeypatch.setattr(tg, 'scan_media', _fake_scan)
    monkeypatch.setattr(tg, 'get_downloaded_ids', lambda *_args, **_kwargs: asyncio.sleep(0, result=[]))
    monkeypatch.setattr(tg, 'set_channel_last_scanned_message_id', _fake_cursor)
    monkeypatch.setattr(telegram_module, 'enqueue_telegram_media_job', _fake_enqueue)

    asyncio.run(tg.update_channel(123, account))

    assert queued == [11, 12]
    assert cursors == [12]


def test_reconciliation_does_not_advance_cursor_when_enqueue_fails(monkeypatch) -> None:
    tg = _make_telegram()
    account = _account()
    cursors: list[int] = []

    monkeypatch.setattr(
        tg,
        'get_channel_state',
        lambda *_args: asyncio.sleep(0, result=telegram_module.TelegramChannelState(last_scanned_message_id=10, has_scan_state=True)),
    )
    monkeypatch.setattr(tg, 'get_backfilled_media_types', lambda *_args: asyncio.sleep(0, result={'video'}))
    monkeypatch.setattr(
        tg,
        'scan_media',
        lambda *_args, **_kwargs: asyncio.sleep(
            0,
            result=telegram_module.TelegramChannelScan(
                media=[_entry(_message(11), 'Video')],
                max_message_id=11,
                scanned_message_count=1,
            ),
        ),
    )
    monkeypatch.setattr(tg, 'get_downloaded_ids', lambda *_args, **_kwargs: asyncio.sleep(0, result=[]))

    async def _failing_enqueue(**_kwargs) -> bool:
        raise RuntimeError('database unavailable')

    async def _fake_cursor(_account_name: str, _channel_id: int, message_id: int) -> None:
        cursors.append(message_id)

    monkeypatch.setattr(telegram_module, 'enqueue_telegram_media_job', _failing_enqueue)
    monkeypatch.setattr(tg, 'set_channel_last_scanned_message_id', _fake_cursor)

    with pytest.raises(RuntimeError, match='database unavailable'):
        asyncio.run(tg.update_channel(123, account))
    assert cursors == []


def test_new_media_type_backfills_full_recent_window(monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(
        channels=[
            telegram_module.TelegramChannel(id=123, path=Path('./collection/image'), media_types=['image']),
            telegram_module.TelegramChannel(id=123, path=Path('./collection/video'), media_types=['video']),
        ],
    )
    queued: list[int] = []
    marked: list[list[telegram_module.TelegramMediaType]] = []
    scans: list[tuple[list[telegram_module.TelegramMediaType], bool]] = []
    monkeypatch.setattr(
        telegram_module,
        'cfg',
        telegram_module.cfg.model_copy(update={'scan_limit': 5, 'download_limit_per_channel': 1}),
    )
    monkeypatch.setattr(
        tg,
        'get_channel_state',
        lambda *_args: asyncio.sleep(0, result=telegram_module.TelegramChannelState(last_scanned_message_id=100, has_scan_state=True)),
    )
    monkeypatch.setattr(tg, 'get_backfilled_media_types', lambda *_args: asyncio.sleep(0, result={'image'}))

    async def _fake_scan(_channel, media_types, **kwargs) -> telegram_module.TelegramChannelScan:
        scans.append((media_types, kwargs['newest_window']))
        if kwargs['newest_window']:
            return telegram_module.TelegramChannelScan(
                media=[
                    _entry(_message(91), 'Downloaded', 'video'),
                    _entry(_message(92), 'Two', 'video'),
                    _entry(_message(93), 'Three', 'video'),
                ],
                max_message_id=100,
                scanned_message_count=5,
            )
        return telegram_module.TelegramChannelScan(media=[], max_message_id=100, scanned_message_count=0)

    async def _fake_downloaded(*_args, **_kwargs) -> list[int]:
        return [91]

    async def _fake_enqueue(**kwargs) -> bool:
        queued.append(kwargs['message_id'])
        return True

    async def _fake_mark(_account_name: str, _channel_id: int, media_types: list[telegram_module.TelegramMediaType]) -> None:
        marked.append(media_types)

    monkeypatch.setattr(tg, 'scan_media', _fake_scan)
    monkeypatch.setattr(tg, 'get_downloaded_ids', _fake_downloaded)
    monkeypatch.setattr(tg, 'mark_media_types_backfilled', _fake_mark)
    monkeypatch.setattr(telegram_module, 'enqueue_telegram_media_job', _fake_enqueue)

    asyncio.run(tg.update_channel(123, account))

    assert queued == [92, 93]
    assert marked == [['video']]
    assert scans == [(['video'], True), (['image', 'video'], False)]


def test_backfill_failure_does_not_mark_complete(monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(media_types=['video'])
    marked: list[list[telegram_module.TelegramMediaType]] = []
    monkeypatch.setattr(
        tg,
        'get_channel_state',
        lambda *_args: asyncio.sleep(0, result=telegram_module.TelegramChannelState(last_scanned_message_id=100, has_scan_state=True)),
    )
    monkeypatch.setattr(tg, 'get_backfilled_media_types', lambda *_args: asyncio.sleep(0, result=set()))
    monkeypatch.setattr(
        tg,
        'scan_media',
        lambda *_args, **_kwargs: asyncio.sleep(
            0,
            result=telegram_module.TelegramChannelScan(
                media=[_entry(_message(90), 'Video', 'video')],
                max_message_id=100,
                scanned_message_count=1,
            ),
        ),
    )
    monkeypatch.setattr(tg, 'get_downloaded_ids', lambda *_args, **_kwargs: asyncio.sleep(0, result=[]))

    async def _failing_enqueue(**_kwargs) -> bool:
        raise RuntimeError('database unavailable')

    async def _fake_mark(_account_name: str, _channel_id: int, media_types: list[telegram_module.TelegramMediaType]) -> None:
        marked.append(media_types)

    monkeypatch.setattr(telegram_module, 'enqueue_telegram_media_job', _failing_enqueue)
    monkeypatch.setattr(tg, 'mark_media_types_backfilled', _fake_mark)

    with pytest.raises(RuntimeError, match='database unavailable'):
        asyncio.run(tg.update_channel(123, account))

    assert marked == []


def test_single_event_and_album_are_persisted_with_event_priority(monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(
        channels=[
            telegram_module.TelegramChannel(id=123, path=Path('./collection/image'), media_types=['image']),
            telegram_module.TelegramChannel(id=123, path=Path('./collection/video'), media_types=['video']),
        ],
    )
    queued: list[dict[str, object]] = []

    async def _fake_enqueue(**kwargs) -> bool:
        queued.append(kwargs)
        return True

    monkeypatch.setattr(telegram_module, 'enqueue_telegram_media_job', _fake_enqueue)

    asyncio.run(
        tg._enqueue_event_messages(
            account=account,
            messages=[_message(1, message='Single', media=telegram_module.MessageMediaPhoto(photo=object()))],
            album=False,
        ),
    )
    asyncio.run(
        tg._enqueue_event_messages(
            account=account,
            messages=[
                _message(2, message='Album', grouped_id=99, media=telegram_module.MessageMediaPhoto(photo=object())),
                _message(3, grouped_id=99, video=True),
            ],
            album=True,
        ),
    )

    assert [(item['message_id'], item['title'], item['source']) for item in queued] == [
        (1, 'Single', 'event'),
        (2, 'Album-1', 'event'),
        (3, 'Album-2', 'event'),
    ]
    assert queued[0]['available_delay_seconds'] == 0
    assert queued[1]['available_delay_seconds'] == telegram_module._ALBUM_SETTLE_SECONDS
    assert queued[2]['available_delay_seconds'] == telegram_module._ALBUM_SETTLE_SECONDS


def test_event_handlers_route_grouped_messages_only_through_album(monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(media_types=['image'])
    handlers: list[tuple[object, object]] = []
    calls: list[tuple[list[int], bool]] = []

    class _Client:
        def add_event_handler(self, callback, builder) -> None:
            handlers.append((callback, builder))

    async def _fake_enqueue_event_messages(*, account, messages, album) -> None:
        calls.append(([message.id for message in messages], album))

    monkeypatch.setattr(tg, '_enqueue_event_messages', _fake_enqueue_event_messages)
    tg._register_event_handlers(account, _Client())
    new_handler = next(callback for callback, builder in handlers if isinstance(builder, events.NewMessage))
    album_handler = next(callback for callback, builder in handlers if isinstance(builder, events.Album))

    asyncio.run(new_handler(SimpleNamespace(message=_message(1))))
    asyncio.run(new_handler(SimpleNamespace(message=_message(2, grouped_id=99))))
    asyncio.run(album_handler(SimpleNamespace(messages=[_message(2, grouped_id=99), _message(3, grouped_id=99)])))

    assert calls == [([1], False), ([2, 3], True)]


def test_existing_archive_completes_queue_without_fetching_message(monkeypatch) -> None:
    tg = _make_telegram()
    completed: list[int] = []

    class _UnexpectedClient:
        async def get_messages(self, *_args, **_kwargs):
            raise AssertionError('Already archived jobs must not refetch Telegram')

    monkeypatch.setattr(tg, 'is_downloaded', lambda *_args: asyncio.sleep(0, result=True))

    async def _complete(job: TelegramMediaJob, _owner_token: str) -> bool:
        completed.append(job.message_id)
        return True

    monkeypatch.setattr(telegram_module, 'mark_telegram_media_job_completed', _complete)

    downloaded = asyncio.run(
        tg._process_job(
            account=_account(media_types=['image']),
            client=_UnexpectedClient(),
            job=_job(),
            owner_token='owner',
        ),
    )

    assert downloaded is False
    assert completed == [10]


def test_split_route_worker_selects_media_destination(monkeypatch, tmp_path) -> None:
    tg = _make_telegram()
    image_path = tmp_path / 'image'
    video_path = tmp_path / 'video'
    account = _account(
        channels=[
            telegram_module.TelegramChannel(id=123, path=image_path, media_types=['image']),
            telegram_module.TelegramChannel(id=123, path=video_path, media_types=['video']),
        ],
    )
    messages = {
        10: _message(10, media=telegram_module.MessageMediaPhoto(photo=object())),
        11: _message(11, video=True),
    }
    destinations: list[Path] = []

    class _Client(_DummyClient):
        async def get_messages(self, *_args, ids: int):
            return messages[ids]

    async def _archive(**kwargs) -> Path:
        destination = kwargs['channel_cfg'].path
        destinations.append(destination)
        return destination / f'{kwargs["job"].message_id}.media'

    monkeypatch.setattr(tg, 'is_downloaded', lambda *_args: asyncio.sleep(0, result=False))
    monkeypatch.setattr(
        tg,
        'get_channel_state',
        lambda *_args: asyncio.sleep(0, result=telegram_module.TelegramChannelState(0, True)),
    )
    monkeypatch.setattr(tg, '_archive_job', _archive)
    monkeypatch.setattr(telegram_module, 'mark_telegram_media_job_completed', lambda *_args: asyncio.sleep(0, result=True))

    image_downloaded = asyncio.run(
        tg._process_job(
            account=account,
            client=_Client(),
            job=_job(message_id=10, media_type='image'),
            owner_token='owner',
        ),
    )
    video_downloaded = asyncio.run(
        tg._process_job(
            account=account,
            client=_Client(),
            job=_job(message_id=11, media_type='video'),
            owner_token='owner',
        ),
    )

    assert image_downloaded is True
    assert video_downloaded is True
    assert destinations == [image_path, video_path]


def test_queue_job_is_discarded_when_media_route_was_removed(monkeypatch) -> None:
    tg = _make_telegram()
    discarded: list[str] = []

    async def _discard(_job: TelegramMediaJob, _owner_token: str, *, error: str) -> bool:
        discarded.append(error)
        return True

    monkeypatch.setattr(telegram_module, 'mark_telegram_media_job_discarded', _discard)

    downloaded = asyncio.run(
        tg._process_job(
            account=_account(media_types=['image']),
            client=_DummyClient(),
            job=_job(media_type='video'),
            owner_token='owner',
        ),
    )

    assert downloaded is False
    assert discarded == ['Media route is no longer configured']


def test_missing_message_is_permanently_discarded(monkeypatch) -> None:
    tg = _make_telegram()
    discarded: list[str] = []

    class _Client(_DummyClient):
        async def get_messages(self, *_args, **_kwargs):
            return None

    monkeypatch.setattr(tg, 'is_downloaded', lambda *_args: asyncio.sleep(0, result=False))
    monkeypatch.setattr(
        tg,
        'get_channel_state',
        lambda *_args: asyncio.sleep(0, result=telegram_module.TelegramChannelState(0, True)),
    )

    async def _discard(_job: TelegramMediaJob, _owner_token: str, *, error: str) -> bool:
        discarded.append(error)
        return True

    monkeypatch.setattr(telegram_module, 'mark_telegram_media_job_discarded', _discard)

    downloaded = asyncio.run(
        tg._process_job(
            account=_account(media_types=['image']),
            client=_Client(),
            job=_job(),
            owner_token='owner',
        ),
    )

    assert downloaded is False
    assert discarded == ['Message was deleted or is unavailable']


def test_successful_image_job_archives_notifies_and_does_not_set_cooldown(monkeypatch, tmp_path) -> None:
    tg = _make_telegram()
    message = _message(10, media=telegram_module.MessageMediaPhoto(photo=object()))
    queries: list[str] = []
    notifications: list[dict[str, object]] = []
    completed: list[int] = []

    class _Client(_DummyClient):
        async def get_messages(self, *_args, **_kwargs):
            return message

    monkeypatch.setattr(tg, 'is_downloaded', lambda *_args: asyncio.sleep(0, result=False))
    monkeypatch.setattr(
        tg,
        'get_channel_state',
        lambda *_args: asyncio.sleep(0, result=telegram_module.TelegramChannelState(0, True)),
    )
    monkeypatch.setattr(tg, 'download', lambda *_args, **_kwargs: asyncio.sleep(0, result=tmp_path / 'image.jpg'))

    async def _query(sql: str, _params=None) -> list[dict[str, object]]:
        queries.append(sql)
        return []

    async def _notify(**kwargs) -> None:
        notifications.append(kwargs)

    async def _complete(job: TelegramMediaJob, _owner_token: str) -> bool:
        completed.append(job.message_id)
        return True

    async def _unexpected_cooldown(*_args, **_kwargs) -> None:
        raise AssertionError('Successful downloads must not activate channel cooldown')

    monkeypatch.setattr(telegram_module.database, 'query_db', _query)
    monkeypatch.setattr(tg, '_notify_download', _notify)
    monkeypatch.setattr(tg, 'set_channel_cooldown', _unexpected_cooldown)
    monkeypatch.setattr(telegram_module, 'mark_telegram_media_job_completed', _complete)

    downloaded = asyncio.run(
        tg._process_job(
            account=_account(channel_path=tmp_path, media_types=['image']),
            client=_Client(),
            job=_job(),
            owner_token='owner',
        ),
    )

    assert downloaded is True
    assert completed == [10]
    assert any('INSERT INTO telegram ' in sql for sql in queries)
    assert notifications[0]['media_type'] == 'image'
    assert notifications[0]['saved_path'] == tmp_path / 'image.jpg'


def test_download_creates_missing_destination_directory(tmp_path) -> None:
    tg = _make_telegram()
    destination = tmp_path / 'missing' / 'video'

    class _Message:
        id = 10

        async def download_media(self, *, file: str, progress_callback) -> str:
            downloaded = Path(file).with_suffix('.mp4')
            downloaded.write_bytes(b'video')
            progress_callback(5, 5)
            return str(downloaded)

    saved_path = asyncio.run(
        tg.download(
            _Message(),
            destination,
            'Video',
            media_type='video',
            account_name='default',
            channel_id=123,
        ),
    )

    assert saved_path is not None
    assert saved_path.parent == destination
    assert saved_path.read_bytes() == b'video'


def test_flood_wait_cools_down_account_and_retries_job(monkeypatch) -> None:
    tg = _make_telegram()
    cooldowns: list[float] = []
    retries: list[float] = []

    class _Client:
        async def get_entity(self, *_args):
            raise FloodWaitError(None, capture=12)

    monkeypatch.setattr(tg, 'is_downloaded', lambda *_args: asyncio.sleep(0, result=False))
    monkeypatch.setattr(
        tg,
        'get_channel_state',
        lambda *_args: asyncio.sleep(
            0,
            result=telegram_module.TelegramChannelState(last_scanned_message_id=0, has_scan_state=True),
        ),
    )
    monkeypatch.setattr(
        telegram_module,
        'cfg',
        telegram_module.cfg.model_copy(update={'channel_cooldown_seconds': 30}),
    )

    async def _cooldown(_account, seconds: float, *, error: str = '') -> None:
        assert 'FloodWaitError' in error
        cooldowns.append(seconds)

    async def _retry(_job, _owner_token, *, error: str, delay_seconds: float) -> bool:
        assert 'FloodWaitError' in error
        retries.append(delay_seconds)
        return True

    monkeypatch.setattr(tg, 'set_account_cooldown', _cooldown)
    monkeypatch.setattr(telegram_module, 'mark_telegram_media_job_retry', _retry)

    downloaded = asyncio.run(
        tg._process_job(
            account=_account(media_types=['image']),
            client=_Client(),
            job=_job(),
            owner_token='owner',
        ),
    )

    assert downloaded is False
    assert cooldowns == [30]
    assert retries == [30]


def test_account_cooldown_updates_split_channel_once(monkeypatch) -> None:
    tg = _make_telegram()
    account = _account(
        channels=[
            telegram_module.TelegramChannel(id=123, path=Path('./collection/image'), media_types=['image']),
            telegram_module.TelegramChannel(id=123, path=Path('./collection/video'), media_types=['video']),
        ],
    )
    cooldowns: list[tuple[str, int, float, str]] = []

    async def _set_cooldown(account_name: str, channel_id: int, seconds: float, *, error: str = '') -> None:
        cooldowns.append((account_name, channel_id, seconds, error))

    monkeypatch.setattr(tg, 'set_channel_cooldown', _set_cooldown)

    asyncio.run(tg.set_account_cooldown(account, 30, error='flood'))

    assert cooldowns == [('default', 123, 30, 'flood')]


def test_account_worker_is_serial_and_delays_between_successes(monkeypatch) -> None:
    tg = _make_telegram()
    jobs = [_job(message_id=1), _job(message_id=2), None]
    processed: list[int] = []
    sleeps: list[float] = []

    async def _claim(_account_name: str, _owner_token: str) -> TelegramMediaJob | None:
        return jobs.pop(0)

    async def _process_job(*, job: TelegramMediaJob, **_kwargs) -> bool:
        processed.append(job.message_id)
        return True

    async def _sleep(seconds: float) -> None:
        sleeps.append(seconds)

    monkeypatch.setattr(telegram_module, 'claim_next_telegram_media_job', _claim)
    monkeypatch.setattr(tg, '_process_job', _process_job)
    monkeypatch.setattr(tg, '_sleep', _sleep)
    monkeypatch.setattr(
        telegram_module,
        'cfg',
        telegram_module.cfg.model_copy(update={'download_delay_seconds': 7}),
    )

    asyncio.run(
        tg._consume_account_queue(
            account=_account(),
            client=_DummyClient(),
            owner_token='owner',
            stop_event=asyncio.Event(),
            drain_only=True,
        ),
    )

    assert processed == [1, 2]
    assert sleeps == [7]


def test_queue_worker_failure_propagates_to_account_runtime() -> None:
    tg = Telegram()

    async def _run() -> None:
        disconnected = asyncio.get_running_loop().create_future()
        client = SimpleNamespace(disconnected=disconnected)

        async def _fail() -> None:
            raise RuntimeError('worker failed')

        worker_task = asyncio.create_task(_fail())
        with pytest.raises(RuntimeError, match='worker failed'):
            await tg._wait_for_disconnect_or_stop(client, asyncio.Event(), worker_task)

    asyncio.run(_run())


def test_persistent_account_client_enables_updates_and_catch_up(monkeypatch) -> None:
    tg = Telegram()
    account = _account()
    stop_event = asyncio.Event()
    client_kwargs: list[dict[str, object]] = []

    class _Client:
        def __init__(self, *_args, **kwargs) -> None:
            client_kwargs.append(kwargs)
            self.disconnected = asyncio.get_running_loop().create_future()

        def add_event_handler(self, *_args) -> None:
            return None

        async def connect(self) -> None:
            return None

        async def is_user_authorized(self) -> bool:
            return True

        async def disconnect(self) -> None:
            if not self.disconnected.done():
                self.disconnected.set_result(None)

    async def _consume(**_kwargs) -> None:
        await stop_event.wait()

    monkeypatch.setattr(telegram_module, 'TelegramClient', _Client)
    monkeypatch.setattr(telegram_module, 'reset_processing_telegram_media_jobs', lambda *_args: asyncio.sleep(0, result=2))
    monkeypatch.setattr(tg, '_consume_account_queue', _consume)

    async def _run() -> None:
        task = asyncio.create_task(tg._run_account_once(account, stop_event))
        await tg._account_ready_events.setdefault(account.name, asyncio.Event()).wait()
        stop_event.set()
        await task

    asyncio.run(_run())

    assert client_kwargs == [
        {
            'flood_sleep_threshold': telegram_module.cfg.flood_sleep_threshold_seconds,
            'receive_updates': True,
            'catch_up': True,
        },
    ]


def test_accounts_run_in_parallel(monkeypatch) -> None:
    tg = Telegram()
    accounts = [_account('one'), _account('two')]
    started: list[str] = []
    both_started = asyncio.Event()
    stop_event = asyncio.Event()
    monkeypatch.setattr(telegram_module, 'cfg', telegram_module.cfg.model_copy(update={'accounts': accounts}))
    monkeypatch.setattr(tg, '_initialize_tables', lambda: asyncio.sleep(0))

    async def _fake_run_account(account: telegram_module.TelegramAccount, _stop_event: asyncio.Event) -> None:
        started.append(account.name)
        if len(started) == 2:
            both_started.set()
        await stop_event.wait()

    monkeypatch.setattr(tg, '_run_account_forever', _fake_run_account)

    async def _run() -> None:
        task = asyncio.create_task(tg.run(stop_event))
        await asyncio.wait_for(both_started.wait(), timeout=1)
        stop_event.set()
        await task

    asyncio.run(_run())
    assert set(started) == {'one', 'two'}


def test_one_shot_uses_session_lock_and_rejects_unauthorized_session(monkeypatch) -> None:
    tg = Telegram()
    account = _account()
    calls: list[str] = []

    class _Client:
        def __init__(self, *_args, **kwargs) -> None:
            assert kwargs['receive_updates'] is False

        async def connect(self) -> None:
            calls.append('connect')

        async def is_user_authorized(self) -> bool:
            return False

        async def disconnect(self) -> None:
            calls.append('disconnect')

    @asynccontextmanager
    async def _lock(name: str):
        calls.append(name)
        yield True

    monkeypatch.setattr(telegram_module, 'TelegramClient', _Client)
    monkeypatch.setattr(telegram_module.database, 'advisory_lock', _lock)
    monkeypatch.setattr(tg, '_initialize_tables', lambda: asyncio.sleep(0))

    with pytest.raises(TelegramSessionUnauthorizedError, match='account=default'):
        asyncio.run(tg.update_account(account))

    assert calls[0].startswith('telegram-session:')
    assert calls[-2:] == ['connect', 'disconnect']


def test_account_lock_name_uses_effective_telethon_session_file(tmp_path) -> None:
    plain_account = _account('default').model_copy(update={'session_path': tmp_path / 'shared'})
    suffixed_account = _account('other').model_copy(update={'session_path': tmp_path / 'shared.session'})

    assert Telegram._account_lock_name(plain_account) == Telegram._account_lock_name(suffixed_account)
    assert Telegram._account_lock_name(plain_account) == f'telegram-session:{(tmp_path / "shared.session").resolve()}'

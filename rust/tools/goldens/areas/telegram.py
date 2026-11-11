"""Telegram (WP15): what `src/web/telegram.py` decides about a message, and what Telethon does.

- `classification.json`: `_message_media_type` for several route sets and `_unsupported_reason`,
  over real Telethon messages built from a small spec (the Rust test builds the same message
  from the same spec).
- `downloads.json`: the file `message.download_media(file=<cache>/<name>)` writes, through
  Telethon 1.43's own `download_media` (its `_download_file` stubbed): the size it picks
  (`_get_thumb`), the name and extension (`_get_proper_filename`, `utils.get_extension`),
  the bytes of an inline size (`stripped_photo_to_jpg`), with some names already taken.
- `mimetypes_defaults.json`: Python 3.12's built-in `mimetypes` table, which the Rust
  extension table starts from.
- `session.json`: a `.session` file as Telethon's `SQLiteSession` writes it: the schema
  and the rows after a login's worth of calls.
- `errors.json`: the class and text of `rpc_message_to_error` for the errors the source
  tells apart, and for the names grammers takes a number out of.
- `rpc_errors.json`: Telethon's whole error table (`rpc_errors_dict`, `rpc_errors_re`,
  `base_errors`): each class, its text, and whether `_call` retries it as a server error.

`mimetypes` reads the machine's `mime.types` files; the downloads are made with the
built-in table and Telethon's additions only, so they do not depend on the machine, and
the module's state is put back afterwards.
"""

from __future__ import annotations

import asyncio
import datetime
import importlib
import mimetypes
import sqlite3
import tempfile
from pathlib import Path
from typing import Any

from src.web.telegram import Telegram
from telethon import TelegramClient, errors, utils
from telethon.crypto import AuthKey
from telethon.errors import rpcbaseerrors, rpcerrorlist
from telethon.sessions import MemorySession, SQLiteSession
from telethon.tl import types
from telethon.tl.functions import InvokeWithoutUpdatesRequest
from telethon.tl.functions.channels import GetChannelsRequest, GetMessagesRequest
from telethon.tl.functions.messages import GetHistoryRequest

CHANNEL = 123
ZHIHU_SHARE = (
    '高跟鞋配白丝真的很难看吗？ - 知乎用户的回答 - 知乎\nhttps://www.zhihu.com/question/1981023166554989468/answer/2100311203096573240'  # noqa: RUF001 - the share text as Zhihu writes it
)
STRIPPED = bytes([1, 40, 30, 0x11, 0x22, 0x33, 0x44, 0x55])


# ---------- messages from a spec ----------


def _attribute(spec: dict[str, Any]) -> object:
    kind = spec['kind']
    if kind == 'video':
        return types.DocumentAttributeVideo(duration=1, w=1, h=1, round_message=spec.get('round', False))
    if kind == 'sticker':
        return types.DocumentAttributeSticker(alt='', stickerset=types.InputStickerSetEmpty())
    if kind == 'custom_emoji':
        return types.DocumentAttributeCustomEmoji(alt='', stickerset=types.InputStickerSetEmpty())
    if kind == 'filename':
        return types.DocumentAttributeFilename(file_name=spec['name'])
    if kind == 'audio':
        return types.DocumentAttributeAudio(
            duration=1,
            voice=spec.get('voice', False),
            performer=spec.get('performer'),
            title=spec.get('title'),
        )
    if kind == 'animated':
        return types.DocumentAttributeAnimated()
    raise ValueError(kind)


def _size(spec: dict[str, Any]) -> object:  # noqa: PLR0911 - one return per photo size kind
    kind = spec['kind']
    if kind == 'size':
        return types.PhotoSize(type=spec['type'], w=1, h=1, size=spec['size'])
    if kind == 'cached':
        return types.PhotoCachedSize(type=spec['type'], w=1, h=1, bytes=bytes.fromhex(spec['bytes']))
    if kind == 'stripped':
        return types.PhotoStrippedSize(type=spec['type'], bytes=bytes.fromhex(spec['bytes']))
    if kind == 'progressive':
        return types.PhotoSizeProgressive(type=spec['type'], w=1, h=1, sizes=spec['sizes'])
    if kind == 'path':
        return types.PhotoPathSize(type=spec['type'], bytes=b'\x01')
    if kind == 'empty':
        return types.PhotoSizeEmpty(type=spec['type'])
    if kind == 'video':
        return types.VideoSize(type=spec['type'], w=1, h=1, size=spec['size'])
    raise ValueError(kind)


def _photo(spec: dict[str, Any]) -> object:
    return types.Photo(
        id=7,
        access_hash=8,
        file_reference=b'ref',
        date=None,
        sizes=[_size(size) for size in spec.get('sizes', [{'kind': 'size', 'type': 'y', 'size': 14}])],
        dc_id=2,
        video_sizes=[_size(size) for size in spec.get('video_sizes', [])] or None,
    )


def _document(spec: dict[str, Any]) -> object:
    return types.Document(
        id=9,
        access_hash=10,
        file_reference=b'ref',
        date=None,
        mime_type=spec.get('mime', ''),
        size=spec.get('size', 14),
        dc_id=2,
        attributes=[_attribute(attr) for attr in spec.get('attributes', [])],
    )


def _media(spec: dict[str, Any] | None) -> object:  # noqa: C901, PLR0911 - one branch per media kind
    if spec is None:
        return None
    kind = spec['kind']
    if kind == 'photo':
        return types.MessageMediaPhoto(photo=_photo(spec))
    if kind == 'photo_empty':
        return types.MessageMediaPhoto(photo=types.PhotoEmpty(id=1))
    if kind == 'document':
        return types.MessageMediaDocument(document=_document(spec))
    if kind == 'document_empty':
        return types.MessageMediaDocument(document=types.DocumentEmpty(id=1))
    if kind == 'webpage':
        if not spec.get('loaded', True):
            return types.MessageMediaWebPage(webpage=types.WebPageEmpty(id=1, url=spec.get('url')))
        return types.MessageMediaWebPage(
            webpage=types.WebPage(
                id=1,
                url=spec['url'],
                display_url=spec['url'],
                hash=0,
                photo=_photo(spec['photo']) if 'photo' in spec else None,
                document=_document(spec['document']) if 'document' in spec else None,
            ),
        )
    if kind == 'geo':
        return types.MessageMediaGeo(geo=types.GeoPointEmpty())
    if kind == 'dice':
        return types.MessageMediaDice(value=1, emoticon='🎲')
    if kind == 'unsupported':
        return types.MessageMediaUnsupported()
    raise ValueError(kind)


def _message(spec: dict[str, Any]) -> object:
    if spec.get('action'):
        return types.MessageService(id=spec['id'], peer_id=types.PeerChannel(CHANNEL), date=None, action=types.MessageActionPinMessage())
    return types.Message(
        id=spec['id'],
        peer_id=types.PeerChannel(CHANNEL),
        date=None,
        message=spec.get('text', ''),
        media=_media(spec.get('media')),
        grouped_id=spec.get('grouped_id'),
    )


VIDEO = {'kind': 'document', 'mime': 'video/mp4', 'attributes': [{'kind': 'video'}]}

MESSAGES: list[dict[str, Any]] = [
    {'id': 1, 'text': 'Caption', 'media': {'kind': 'photo'}},
    {'id': 2, 'media': {'kind': 'photo_empty'}},
    {'id': 3, 'media': VIDEO},
    {'id': 4, 'media': {'kind': 'document', 'mime': 'video/mp4', 'attributes': [{'kind': 'video', 'round': True}]}},
    {'id': 5, 'media': {'kind': 'document', 'mime': 'image/png'}},
    {'id': 6, 'media': {'kind': 'document', 'mime': 'image/webp', 'attributes': [{'kind': 'sticker'}]}},
    {'id': 7, 'media': {'kind': 'document', 'mime': 'image/webp', 'attributes': [{'kind': 'custom_emoji'}]}},
    {'id': 8, 'media': {'kind': 'document', 'mime': 'video/mp4', 'attributes': [{'kind': 'animated'}, {'kind': 'video'}]}},
    {'id': 9, 'media': {'kind': 'document', 'mime': 'application/octet-stream', 'attributes': [{'kind': 'filename', 'name': 'clip.mkv'}]}},
    {'id': 10, 'media': {'kind': 'document', 'mime': 'application/pdf'}},
    {'id': 11, 'media': {'kind': 'document'}},
    {'id': 12, 'media': {'kind': 'document_empty'}},
    {'id': 13, 'text': 'see', 'media': {'kind': 'webpage', 'url': 'https://example.com/a', 'photo': {}}},
    {'id': 14, 'media': {'kind': 'webpage', 'url': 'https://example.com/b', 'document': VIDEO}},
    {'id': 15, 'media': {'kind': 'webpage', 'url': 'https://tieba.baidu.com/p/9876543210?share=1'}},
    {'id': 16, 'media': {'kind': 'webpage', 'loaded': False, 'url': 'https://bbs.nga.cn/read.php?tid=1'}},
    {'id': 17, 'text': ZHIHU_SHARE},
    {'id': 18, 'text': 'just a note'},
    {'id': 19, 'text': '  '},
    {'id': 20, 'text': 'https://example.com/post/1'},
    {'id': 21, 'media': {'kind': 'geo'}},
    {'id': 22, 'media': {'kind': 'dice'}},
    {'id': 23, 'media': {'kind': 'unsupported'}},
    {'id': 24, 'action': True},
    {'id': 25, 'text': ZHIHU_SHARE, 'media': {'kind': 'photo'}},
    {'id': 26, 'media': {'kind': 'document', 'mime': 'audio/ogg', 'attributes': [{'kind': 'audio', 'voice': True}]}},
    {'id': 27, 'text': 'x' * 150},
    {
        'id': 28,
        'text': '看看 https://mp.weixin.qq.com/s/abc',
        'media': {'kind': 'document', 'mime': 'image/gif', 'attributes': [{'kind': 'sticker'}]},
    },
]

TYPE_SETS = [
    ['image'],
    ['video'],
    ['image', 'video', 'link'],
    ['video', 'link', 'unsupported'],
    ['image', 'video', 'link', 'unsupported'],
]


def _classification() -> list[object]:
    cases = []
    for spec in MESSAGES:
        message = _message(spec)
        cases.append(
            {
                'message': spec,
                'media_type': {','.join(kinds): Telegram._message_media_type(message, set(kinds)) for kinds in TYPE_SETS},  # noqa: SLF001 - the decision under test
                'reason': Telegram._unsupported_reason(message),  # noqa: SLF001 - the decision under test
            },
        )
    return cases


# ---------- downloads ----------


def _default_mimetypes() -> None:
    """The built-in table and what Telethon adds on import, without the machine's files."""
    mimetypes.init(files=[])
    importlib.reload(utils)


DOWNLOADS: list[dict[str, Any]] = [
    {'media': {'kind': 'photo'}},
    {'media': {'kind': 'photo'}, 'existing': ['file.jpg']},
    {'media': {'kind': 'photo'}, 'existing': ['file.jpg', 'file (1).jpg']},
    {
        'media': {
            'kind': 'photo',
            'sizes': [
                {'kind': 'stripped', 'type': 'i', 'bytes': STRIPPED.hex()},
                {'kind': 'size', 'type': 'm', 'size': 300},
                {'kind': 'progressive', 'type': 'y', 'sizes': [100, 900, 500]},
                {'kind': 'size', 'type': 'x', 'size': 800},
                {'kind': 'path', 'type': 'j'},
            ],
        },
    },
    {
        'media': {
            'kind': 'photo',
            'sizes': [{'kind': 'size', 'type': 'x', 'size': 800}],
            'video_sizes': [{'kind': 'video', 'type': 'u', 'size': 5}],
        }
    },
    {'media': {'kind': 'photo', 'sizes': [{'kind': 'stripped', 'type': 'i', 'bytes': STRIPPED.hex()}]}},
    {'media': {'kind': 'photo', 'sizes': [{'kind': 'cached', 'type': 's', 'bytes': 'ffd8ffe0cafe'}]}},
    {'media': {'kind': 'photo', 'sizes': [{'kind': 'empty', 'type': 's'}]}},
    {'media': {'kind': 'photo', 'sizes': [{'kind': 'path', 'type': 'j'}, {'kind': 'size', 'type': 'a', 'size': 3}]}},
    {'media': {'kind': 'photo_empty'}},
    {'media': VIDEO},
    {'media': {'kind': 'document', 'mime': 'video/quicktime', 'size': 99, 'attributes': [{'kind': 'video'}]}},
    {'media': {'kind': 'document', 'mime': 'image/jpeg'}},
    {'media': {'kind': 'document', 'mime': 'image/png'}},
    {'media': {'kind': 'document', 'mime': 'image/webp'}},
    {'media': {'kind': 'document', 'mime': 'image/gif'}},
    {'media': {'kind': 'document', 'mime': 'IMAGE/PNG'}},
    {'media': {'kind': 'document', 'mime': 'audio/ogg'}},
    {'media': {'kind': 'document', 'mime': 'audio/mpeg'}},
    {'media': {'kind': 'document', 'mime': 'application/x-tgsticker'}},
    {'media': {'kind': 'document', 'mime': 'application/pdf'}},
    {'media': {'kind': 'document', 'mime': 'application/octet-stream', 'attributes': [{'kind': 'filename', 'name': 'clip.mkv'}]}},
    {'media': {'kind': 'document', 'mime': 'application/x-unknown'}},
    {'media': {'kind': 'document', 'mime': ''}},
    {'media': VIDEO, 'existing': ['file.mp4']},
    {'media': {'kind': 'document_empty'}},
    {'media': {'kind': 'webpage', 'url': 'https://example.com/a', 'photo': {}}},
    {'media': {'kind': 'webpage', 'url': 'https://example.com/b', 'document': VIDEO, 'photo': {}}},
    {'media': {'kind': 'webpage', 'url': 'https://example.com/c'}},
    {'media': {'kind': 'webpage', 'loaded': False, 'url': 'https://example.com/d'}},
    {'media': {'kind': 'geo'}},
    {'media': None},
]


async def _download(case: dict[str, Any], folder: Path) -> dict[str, Any]:
    client = TelegramClient(MemorySession(), 1, 'hash')
    requested: dict[str, Any] = {}

    async def _download_file(input_location: object, file: object = None, **kwargs: Any) -> object:
        requested['location'] = type(input_location).__name__
        requested['thumb'] = getattr(input_location, 'thumb_size', None)
        requested['size'] = kwargs.get('file_size')
        Path(file).write_bytes(b'downloaded')  # noqa: ASYNC240 - a stub inside a test download
        return file

    client._download_file = _download_file  # noqa: SLF001 - the network is the one thing stubbed
    for name in case.get('existing', []):
        (folder / name).write_bytes(b'')
    message = _message({'id': 10, 'media': case['media']})
    result = await client.download_media(message, file=str(folder / 'file'))
    out: dict[str, Any] = {'case': case, 'name': None}
    if result is not None:
        path = Path(result)
        out['name'] = path.name
        out['request'] = requested or None
        if not requested:
            out['inline'] = path.read_bytes().hex()  # noqa: ASYNC240 - a small temporary file
    return out


def _downloads() -> list[object]:
    saved = {name: getattr(mimetypes, name) for name in ('inited', '_db', 'encodings_map', 'suffix_map', 'types_map', 'common_types')}
    try:
        _default_mimetypes()
        cases = []
        for case in DOWNLOADS:
            with tempfile.TemporaryDirectory() as folder:
                cases.append(asyncio.run(_download(case, Path(folder))))
        extensions = {
            mime: utils.get_extension(_document({'mime': mime}))
            for mime in [
                'image/jpeg',
                'image/png',
                'image/x-tga',
                'image/vnd.adobe.photoshop',
                'video/avi',
                'audio/m4a',
                'audio/flac',
                'text/plain',
            ]
        }
        return [{'downloads': cases, 'extensions': extensions}]
    finally:
        for name, value in saved.items():
            setattr(mimetypes, name, value)
        importlib.reload(utils)


def _mimetypes_defaults() -> list[object]:
    return [[ext, kind] for ext, kind in mimetypes._types_map_default.items()]  # noqa: SLF001 - the table itself is the golden


# ---------- the session file ----------


def _rows(connection: sqlite3.Connection, table: str) -> list[list[object]]:
    rows = connection.execute(f'select * from {table} order by 1').fetchall()  # noqa: S608 - fixed table names
    return [[value.hex() if isinstance(value, bytes) else value for value in row] for row in rows]


def _session() -> list[object]:
    with tempfile.TemporaryDirectory() as folder:
        session = SQLiteSession(str(Path(folder) / 'account'))
        session.set_dc(4, '149.154.167.91', 443)
        session.auth_key = AuthKey(bytes(range(256)))
        session.process_entities(
            types.contacts.ResolvedPeer(
                peer=types.PeerUser(42),
                chats=[
                    types.Channel(
                        id=CHANNEL, title='Demo', photo=types.ChatPhotoEmpty(), date=None, access_hash=555, username='Demo_Channel'
                    ),
                    types.Chat(id=77, title='Group', photo=types.ChatPhotoEmpty(), participants_count=2, date=None, version=1),
                ],
                users=[types.User(id=42, access_hash=999, first_name='Max', username='Someone', is_self=True)],
            ),
        )
        # `_save_states_and_entities` on disconnect.
        session.process_entities(types.contacts.ResolvedPeer(None, [types.InputPeerUser(0, 42)], []))
        date = datetime.datetime(2026, 11, 10, 12, 0, tzinfo=datetime.UTC)
        session.set_update_state(0, types.updates.State(pts=100, qts=3, date=date, seq=7, unread_count=0))
        session.set_update_state(CHANNEL, types.updates.State(pts=55, qts=0, date=date, seq=0, unread_count=0))
        session.save()
        session.close()
        connection = sqlite3.connect(Path(folder) / 'account.session')
        try:
            schema = [row[0] for row in connection.execute("select sql from sqlite_master where type = 'table' order by rowid")]
            tables = {table: _rows(connection, table) for table in ('version', 'sessions', 'entities', 'sent_files', 'update_state')}
        finally:
            connection.close()
    # The entity rows carry the time they were written; that column is not compared.
    for row in tables['entities']:
        row[5] = None
    return [{'schema': schema, 'tables': tables}]


# ---------- errors ----------


def _errors() -> list[object]:
    history = GetHistoryRequest(types.InputPeerEmpty(), 0, None, 0, 0, 0, 0, 0)
    cases = []
    for code, message, request in [
        (420, 'FLOOD_WAIT_12', GetChannelsRequest([])),
        (420, 'FLOOD_WAIT_0', GetChannelsRequest([])),
        (420, 'FLOOD_WAIT_05', GetChannelsRequest([])),
        (420, 'FLOOD_PREMIUM_WAIT_5', GetChannelsRequest([])),
        (420, 'FLOOD_TEST_PHONE_WAIT_7', None),
        (420, 'SLOWMODE_WAIT_9', None),
        (420, '2FA_CONFIRM_WAIT_86400', None),
        (400, 'MESSAGE_ID_INVALID', GetMessagesRequest(types.InputChannelEmpty(), [])),
        (400, 'CHANNEL_INVALID', None),
        (400, 'CHANNEL_PRIVATE', GetChannelsRequest([])),
        (400, 'FILE_REFERENCE_EXPIRED', history),
        (400, 'FILE_PART_3_MISSING', None),
        (400, 'SOMETHING_NEW_AND_UNKNOWN', None),
        (400, 'SOMETHING_NEW_42', None),
        (406, 'PREVIOUS_CHAT_IMPORT_ACTIVE_WAIT_5MIN', None),
        (500, 'SOMETHING_ON_FIRE', None),
        (500, 'INTERDC_2_CALL_ERROR', None),
        (500, 'INTERDC_4_CALL_RICH_ERROR', None),
        (-500, 'No workers running', None),
        (-503, 'Timeout', None),
        (500, 'RPC_CALL_FAIL', None),
        (303, 'FILE_MIGRATE_5', None),
        (303, 'SOMEWHERE_ELSE', None),
        (401, 'AUTH_KEY_UNREGISTERED', None),
        (401, 'auth_key_unregistered', None),
        (403, 'CHAT_WRITE_FORBIDDEN', InvokeWithoutUpdatesRequest(history)),
        (420, 'FLOOD_WAIT_30', InvokeWithoutUpdatesRequest(history)),
        (418, 'I_AM_A_TEAPOT', None),
    ]:
        error = errors.rpc_message_to_error(types.RpcError(code, message), request)
        cases.append(
            {
                'code': code,
                'message': message,
                'request': type(request).__name__ if request else None,
                'inner': type(request.query).__name__ if isinstance(request, InvokeWithoutUpdatesRequest) else None,
                'class': type(error).__name__,
                'text': str(error),
                'seconds': getattr(error, 'seconds', None),
                'server': _server(type(error)),
            },
        )
    return cases


_CAUSE = ' (caused by NoneType)'
_CAPTURE = 987654321


def _server(cls: type) -> bool:
    """Whether `_call` sleeps two seconds and sends the request again after this error."""
    return issubclass(
        cls,
        (
            errors.ServerError,
            errors.RpcCallFailError,
            errors.RpcMcgetFailError,
            errors.InterdcCallErrorError,
            errors.TimedOutError,
            errors.InterdcCallRichErrorError,
        ),
    )


def _text(error: Exception) -> str:
    text = str(error)
    assert text.endswith(_CAUSE), text  # noqa: S101 - a golden that does not hold must stop the generator
    return text[: -len(_CAUSE)]


def _rpc_errors() -> list[object]:
    cases: list[object] = []
    for name, cls in rpcerrorlist.rpc_errors_dict.items():
        cases.append({'kind': 'exact', 'name': name, 'class': cls.__name__, 'text': _text(cls(request=None)), 'server': _server(cls)})
    for regex, cls in rpcerrorlist.rpc_errors_re:
        text = _text(cls(request=None, capture=_CAPTURE))
        assert text.count(str(_CAPTURE)) == 1, text  # noqa: S101 - a golden that does not hold must stop the generator
        cases.append(
            {
                'kind': 'pattern',
                'regex': regex,
                'class': cls.__name__,
                'text': text.replace(str(_CAPTURE), '{}'),
                'server': _server(cls),
            },
        )
    for code, cls in sorted(rpcbaseerrors.base_errors.items()):
        cases.append({'kind': 'base', 'code': code, 'class': cls.__name__, 'server': _server(cls)})
    return cases


GOLDENS = {
    'telegram/classification.json': _classification,
    'telegram/downloads.json': _downloads,
    'telegram/mimetypes_defaults.json': _mimetypes_defaults,
    'telegram/session.json': _session,
    'telegram/errors.json': _errors,
    'telegram/rpc_errors.json': _rpc_errors,
}

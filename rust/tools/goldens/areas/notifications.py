"""The notification outbox and its Telegram Bot delivery (WP2).

`render.json` is `_notification_delivery_fields`, the template every message follows
(`src/tool/notifications.py`): the markdown, and whether the preview, the sound and the pin
are on. `text.json` holds the escaping helpers and the dedupe keys. `send_message.json`
runs the real `_send_message` against a fake Bot API and records the JSON it posts, which
covers the plain-text fallback for a message longer than Telegram allows. `deliver.json`
runs the real `deliver()` through scripted Bot API answers and records every request and
the result or the error, so the photo fallbacks, the pin bookkeeping and the error
messages are Python's.
"""

from __future__ import annotations

import asyncio
import json
import mimetypes
import tempfile
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import httpx
from src.tool import notifications, telegram_bot
from src.tool.notifications import NotificationRecord

_KINDS = ['job_failed', 'unsupported_message', 'unsupported_resolved', 'job_recovered', 'download_completed']
_ACTIONS = ['send', 'upsert', 'resolve']
_OCCURRENCES = [1, 2, 7]
# Each entry is a header, title, body, link and image URL.
_TEXTS: list[tuple[str, str, str, str, str]] = [
    ('', '', '', '', ''),
    ('Bilibili', 'Job failed', 'RuntimeError: boom', '', ''),
    ('Bilibili (toview)', '- Video [01]', 'Uploader | 1080P | 12.3 MiB | 2026-03-01', 'https://www.bilibili.com/video/BV1TEST', ''),
    ('', 'Title', '', 'https://example.com/a_(b)?q=1.2\\x', ''),
    ('', '', 'Body', 'https://example.com/video', ''),
    ('Stella Sora', 'Image', 'disc/Foo.png', '', 'https://example.com/Foo.png'),
    ('  X  ', '  padded title \u3000', '\n body with\nlines \t', '  https://x.com/i/1  ', ''),
    ('Telegram', 'fav #130', 'Link not supported: https://example.com/x', 'https://t.me/c/1/130', ''),
    ('Kemono', '_*[]()~`>#+-=|{}.!\\', '\\_*[]()~`>#+-=|{}.!', '', ''),
    ('微信', '标题: 测试 🎉', '正文\u2028第二行 😀', 'https://mp.weixin.qq.com/s/abc', ''),
    ('\x1c', '\x1d', '\x1e\x1f\x85\xa0', '\u2003', ''),
    ('Nikke', 'a' * 20, 'b' * 30, '', ''),
]

_ESCAPE_INPUTS = [
    '',
    'plain',
    '_*[]()~`>#+-=|{}.!\\',
    'https://example.com/a_(b)?q=1.2\\x',
    'already \\_ escaped',
    '中文 · 🎉 · e\u0301',
    'a\nb\tc',
    '100% done_ok',
    'C:\\path\\to_file',
]

# File names a source might store as `image_path`, for `mimetypes.guess_type` (the local image path).
_IMAGE_NAMES = [
    'cover.png',
    'photo.JPG',
    'sticker.gif',
    'big.webp',
    'scan.TIFF',
    'icon.svgz',
    'archive.png.gz',
    'archive.PNG.GZ',
    'drawing.svg.Z',
    'notes.txt',
    '.png',
    '..png',
    'a..png',
    'no_extension',
    'Re: hello.png',
    'Note: #1 cover.png',
    'Note: cover.png?x=1',
    'http://host/a.png;params',
    'http://host/dir;x/a.png',
    'ftp:a.jpg;type=i',
    'x:y.png',
    'c:/path/z.jpeg',
    'data:image/png;base64,AAAA',
    'data:image/svg+xml,<svg/>',
    'data:text/plain,hi.png',
    'data:nocomma.png',
    '  sp:ace.heic',
    '[uploader]title [id].avif',
    '中文.JPEG',
]

_JOB_KEYS = ['bilibili', 'Bilibili', '  Twitter  ', '', '   ', 'İstanbul', 'job_failed']
_FAILURE_KEYS = ['run', 'bilibili:download:BV1TEST', '  spaced  ', '', '  ', 'x']


def _render() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for kind in _KINDS:
        for action in _ACTIONS:
            for occurrences in _OCCURRENCES:
                for header, title, body, link_url, image_url in _TEXTS:
                    markdown, disable_preview, disable_notification, pin = notifications._notification_delivery_fields(  # noqa: SLF001
                        kind=kind,
                        header=header,
                        title=title,
                        body=body,
                        link_url=link_url,
                        image_url=image_url,
                        webhook_action=action,
                        occurrence_count=occurrences,
                    )
                    cases.append(
                        {
                            'kind': kind,
                            'webhook_action': action,
                            'occurrence_count': occurrences,
                            'header': header,
                            'title': title,
                            'body': body,
                            'link_url': link_url,
                            'image_url': image_url,
                            'markdown': markdown,
                            'disable_web_page_preview': disable_preview,
                            'disable_notification': disable_notification,
                            'pin': pin,
                        },
                    )
    return cases


def _text() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = [
        {
            'function': 'escape',
            'input': value,
            'escape_markdown_v2': telegram_bot.escape_markdown_v2(value),
            'escape_markdown_v2_url': notifications._escape_markdown_v2_url(value),  # noqa: SLF001
            'format_header_line': telegram_bot.format_header_line(value),
            'like_literal': notifications._like_literal(value),  # noqa: SLF001
        }
        for value in _ESCAPE_INPUTS
    ]
    cases.extend(
        {
            'function': 'format_job_failure_dedupe_key',
            'job_key': job_key,
            'failure_key': failure_key,
            'result': notifications.format_job_failure_dedupe_key(job_key=job_key, failure_key=failure_key),
        }
        for job_key in _JOB_KEYS
        for failure_key in _FAILURE_KEYS
    )
    cases.extend(
        {
            'function': 'format_job_run_failure_dedupe_key',
            'job_key': job_key,
            'result': notifications.format_job_run_failure_dedupe_key(job_key),
        }
        for job_key in _JOB_KEYS
    )
    defaults = mimetypes.MimeTypes()
    cases.extend({'function': 'guess_type', 'name': name, 'result': defaults.guess_type(name)[0]} for name in _IMAGE_NAMES)
    cases.extend(
        {'function': 'retry_delay_seconds', 'attempt': attempt, 'result': notifications.retry_delay_seconds(attempt)}
        for attempt in range(-1, 13)
    )
    return cases


# A fake token: requests only ever reach httpx.MockTransport.
_TOKEN = '123:secret-token'  # noqa: S105


def _config(thread: int | None = 42) -> telegram_bot.TelegramBotConfig:
    return telegram_bot.TelegramBotConfig(bot_token=_TOKEN, chat_id='-100123', message_thread_id=thread)


def _send_message_cases() -> list[dict[str, Any]]:
    limit = telegram_bot.MAX_MESSAGE_LENGTH
    inputs: list[tuple[str, str, bool, bool, int | None]] = [
        # Each entry: a label, the markdown, then disable_notification, disable_web_page_preview and the topic.
        ('short', '*Job failed*\nboom', False, True, 42),
        ('empty', '', True, True, None),
        ('blank', ' \n\t ', True, False, None),
        ('exactly the limit', 'x' * limit, True, True, 42),
        ('one over the limit', 'x' * (limit + 1), True, True, 42),
        ('long with title', f'*Title*\n{"x" * limit}', True, True, 42),
        ('long with escapes', '\\_\\*\\[x\\]\\. ' * 700, False, True, None),
        ('long with a lone backslash', 'a\\' * 2100 + '\\', True, True, None),
        ('long with backslash before a plain char', '\\a\\b' * 1100, True, True, None),
        ('long emoji', '😀' * (limit + 10), True, True, None),
        ('long stars', '*' * 5000 + 'tail', True, True, None),
        ('surrounding space', f'  {"y" * limit}  ', True, True, None),
    ]
    cases: list[dict[str, Any]] = []
    for label, markdown, disable_notification, disable_preview, thread in inputs:
        posted: list[dict[str, Any]] = []

        def _handler(request: httpx.Request, posted: list[dict[str, Any]] = posted) -> httpx.Response:
            posted.append(json.loads(request.content))
            return httpx.Response(200, json={'ok': True, 'result': {'message_id': 1}})

        async def _run(
            markdown: str = markdown,
            disable_notification: bool = disable_notification,  # noqa: FBT001
            disable_preview: bool = disable_preview,  # noqa: FBT001
            thread: int | None = thread,
            handler: Any = _handler,
        ) -> int | None:
            async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
                return await telegram_bot._send_message(  # noqa: SLF001
                    client=client,
                    config=_config(thread),
                    markdown=markdown,
                    disable_notification=disable_notification,
                    disable_web_page_preview=disable_preview,
                )

        asyncio.run(_run())
        cases.append(
            {
                'label': label,
                'markdown': markdown,
                'disable_notification': disable_notification,
                'disable_web_page_preview': disable_preview,
                'message_thread_id': thread,
                'payload': posted[0],
            },
        )
    return cases


_NOW = datetime(2026, 3, 2, tzinfo=UTC)

_OK_MESSAGE = {'status': 200, 'json': {'ok': True, 'result': {'message_id': 99}}}
_OK_TRUE = {'status': 200, 'json': {'ok': True, 'result': True}}

# Each scenario: notification overrides, the local image to create (name, size) if any, and
# the scripted answer per Bot API method (a list is consumed in order; the last repeats).
_SCENARIOS: list[dict[str, Any]] = [
    {'label': 'text message to a topic', 'notification': {}, 'responses': {}},
    {'label': 'no topic', 'thread': None, 'notification': {}, 'responses': {}},
    {
        'label': 'remote photo with caption',
        'notification': {'image_url': ' https://images.example/test.png ', 'disable_web_page_preview': False},
        'responses': {},
    },
    {
        'label': 'remote photo rejected falls back to text',
        'notification': {'image_url': 'https://images.example/test.png'},
        'responses': {'sendPhoto': [{'status': 200, 'json': {'ok': False, 'error_code': 400, 'description': 'wrong file identifier'}}]},
    },
    {
        'label': 'remote photo 413 and 415 fall back',
        'notification': {'image_url': 'https://images.example/test.png'},
        'responses': {'sendPhoto': [{'status': 413, 'json': {'ok': False, 'error_code': 413, 'description': 'too big'}}]},
    },
    {
        'label': 'remote photo server error is not a fallback',
        'notification': {'image_url': 'https://images.example/test.png'},
        'responses': {'sendPhoto': [{'status': 502, 'text': 'Bad Gateway'}]},
    },
    {
        'label': 'remote photo forbidden is not a fallback',
        'notification': {'image_url': 'https://images.example/test.png'},
        'responses': {'sendPhoto': [{'status': 200, 'json': {'ok': False, 'error_code': 403, 'description': 'bot was blocked'}}]},
    },
    {
        'label': 'local png then remote',
        'image': ('cover.png', 64),
        'notification': {'image_url': 'https://images.example/test.png'},
        'responses': {},
    },
    {
        'label': 'local png rejected then remote accepted',
        'image': ('cover.png', 64),
        'notification': {'image_url': 'https://images.example/test.png'},
        'responses': {
            'sendPhoto': [
                {'status': 400, 'json': {'ok': False, 'error_code': 400, 'description': 'IMAGE_PROCESS_FAILED'}},
                _OK_MESSAGE,
            ],
        },
    },
    {'label': 'local gif as animation', 'image': ('sticker.gif', 13), 'notification': {}, 'responses': {}},
    {'label': 'local jpeg uppercase extension', 'image': ('photo.JPG', 10), 'notification': {}, 'responses': {}},
    {'label': 'local file that is not an image', 'image': ('notes.txt', 10), 'notification': {}, 'responses': {}},
    {'label': 'local image too large', 'image': ('huge.png', telegram_bot.MAX_IMAGE_BYTES + 1), 'notification': {}, 'responses': {}},
    {'label': 'local image of a type the defaults do not know', 'image': ('photo.webp', 10), 'notification': {}, 'responses': {}},
    {'label': 'local image at the size limit', 'image': ('big.png', telegram_bot.MAX_IMAGE_BYTES), 'notification': {}, 'responses': {}},
    {'label': 'missing local image', 'missing_image': 'gone.png', 'notification': {}, 'responses': {}},
    {
        'label': 'caption too long sends photo then text',
        'notification': {'image_url': 'https://images.example/test.png', 'markdown': 'c' * 1025},
        'responses': {},
    },
    {
        'label': 'caption at the limit',
        'notification': {'image_url': 'https://images.example/test.png', 'markdown': 'c' * 1024},
        'responses': {},
    },
    {
        'label': 'rate limited with retry_after and the token in the description',
        'notification': {},
        'responses': {
            'sendMessage': [
                {
                    'status': 429,
                    'json': {
                        'ok': False,
                        'error_code': 429,
                        'description': 'Too Many Requests: retry for 123:secret-token',
                        'parameters': {'retry_after': 17},
                    },
                },
            ],
        },
    },
    {
        'label': 'error without json',
        'notification': {},
        'responses': {'sendMessage': [{'status': 500, 'text': '<html>oops</html>'}]},
    },
    {
        'label': 'ok false without error_code keeps the HTTP status',
        'notification': {},
        'responses': {'sendMessage': [{'status': 200, 'json': {'ok': False, 'description': 'x' * 300}}]},
    },
    {
        'label': 'conflict is retryable',
        'notification': {},
        'responses': {'sendMessage': [{'status': 409, 'json': {'ok': False, 'error_code': 409, 'description': 'Conflict'}}]},
    },
    {
        'label': 'bad request is permanent',
        'notification': {},
        'responses': {'sendMessage': [{'status': 400, 'json': {'ok': False, 'error_code': 400, 'description': "can't parse entities"}}]},
    },
    {
        'label': 'result that is not an object',
        'notification': {},
        'responses': {'sendMessage': [{'status': 200, 'json': {'ok': True, 'result': [1, 2]}}]},
    },
    {
        'label': 'message id that is not an integer',
        'notification': {},
        'responses': {'sendMessage': [{'status': 200, 'json': {'ok': True, 'result': {'message_id': '99'}}}]},
    },
    {
        'label': 'retry_after that is not an integer',
        'notification': {},
        'responses': {
            'sendMessage': [
                {'status': 429, 'json': {'ok': False, 'error_code': 429, 'description': 'slow', 'parameters': {'retry_after': '5'}}},
            ],
        },
    },
    {'label': 'pin', 'notification': {'pin': True}, 'responses': {}},
    {
        'label': 'pin forbidden',
        'notification': {'pin': True},
        'responses': {'pinChatMessage': [{'status': 200, 'json': {'ok': False, 'error_code': 403, 'description': 'not enough rights'}}]},
    },
    {'label': 'pin replaces the previous pin', 'notification': {'pin': True, 'telegram_pinned_message_id': 55}, 'responses': {}},
    {'label': 'pin of the same message', 'notification': {'pin': True, 'telegram_pinned_message_id': 99}, 'responses': {}},
    {
        'label': 'resolve unpins',
        'notification': {'webhook_action': 'resolve', 'telegram_pinned_message_id': 55},
        'responses': {},
    },
    {
        'label': 'unpin of a message that is gone',
        'notification': {'webhook_action': 'resolve', 'telegram_pinned_message_id': 55},
        'responses': {
            'unpinChatMessage': [{'status': 400, 'json': {'ok': False, 'error_code': 400, 'description': 'message to unpin not found'}}],
        },
    },
    {
        'label': 'unpin forbidden keeps the old pin',
        'notification': {'webhook_action': 'resolve', 'telegram_pinned_message_id': 55},
        'responses': {'unpinChatMessage': [{'status': 200, 'json': {'ok': False, 'error_code': 403, 'description': 'not enough rights'}}]},
    },
    {
        'label': 'unpin forbidden after a new pin',
        'notification': {'pin': True, 'telegram_pinned_message_id': 55},
        'responses': {'unpinChatMessage': [{'status': 200, 'json': {'ok': False, 'error_code': 403, 'description': 'not enough rights'}}]},
    },
    {
        'label': 'unpin server error after a new pin failed',
        'notification': {'pin': True, 'telegram_pinned_message_id': 55},
        'responses': {
            'pinChatMessage': [{'status': 200, 'json': {'ok': False, 'error_code': 400, 'description': 'message not found'}}],
            'unpinChatMessage': [{'status': 503, 'json': {'ok': False, 'error_code': 503, 'description': 'busy'}}],
        },
    },
    {
        'label': 'no message id means nothing to pin',
        'notification': {'pin': True, 'telegram_pinned_message_id': 55},
        'responses': {'sendMessage': [{'status': 200, 'json': {'ok': True, 'result': {}}}]},
    },
    {
        'label': 'photo pinned',
        'notification': {'pin': True, 'image_url': 'https://images.example/test.png'},
        'responses': {'sendPhoto': [{'status': 200, 'json': {'ok': True, 'result': {'message_id': 7}}}]},
    },
]


def _notification(overrides: dict[str, Any], image_path: str | None) -> NotificationRecord:
    values: dict[str, Any] = {
        'notification_id': 7,
        'kind': 'job_failed',
        'source': 'worker',
        'title': 'Job failed',
        'body': 'boom',
        'link_url': '',
        'image_url': '',
        'payload': json.dumps({'image_path': image_path}) if image_path else '{}',
        'dedupe_key': 'job_failed:test:boom',
        'status': 'unread',
        'markdown': 'FAV · Test\n*Job failed*\nboom',
        'disable_web_page_preview': True,
        'disable_notification': False,
        'webhook_action': 'upsert',
        'occurrence_count': 1,
        'event_version': 3,
        'delivery_status': 'sending',
        'attempt_count': 0,
        'next_attempt_at': _NOW,
        'created_at': _NOW,
        'pin': False,
    }
    values.update(overrides)
    return NotificationRecord(**values)


def _recorded_request(request: httpx.Request) -> dict[str, Any]:
    method = request.url.path.rsplit('/', 1)[-1]
    content_type = request.headers.get('content-type', '')
    entry: dict[str, Any] = {'method': method, 'path': request.url.path}
    if content_type.startswith('application/json'):
        entry['json'] = json.loads(request.content)
        return entry
    # Multipart: record the form fields and the file part's field, name, type and size.
    boundary = content_type.split('boundary=', 1)[1].encode()
    fields: dict[str, str] = {}
    files: list[dict[str, Any]] = []
    for part in request.content.split(b'--' + boundary)[1:-1]:
        head, _, body = part.strip(b'\r\n').partition(b'\r\n\r\n')
        disposition = next(line for line in head.split(b'\r\n') if line.lower().startswith(b'content-disposition'))
        name = disposition.split(b'name="', 1)[1].split(b'"', 1)[0].decode()
        if b'filename="' in disposition:
            type_lines = [line for line in head.split(b'\r\n') if line.lower().startswith(b'content-type')]
            part_type = type_lines[0].split(b':', 1)[1].strip().decode() if type_lines else ''
            filename = disposition.split(b'filename="', 1)[1].split(b'"', 1)[0].decode()
            files.append({'field': name, 'filename': filename, 'content_type': part_type, 'size': len(body)})
        else:
            fields[name] = body.decode()
    entry['form'] = fields
    entry['files'] = files
    return entry


def _scripted_handler(scenario: dict[str, Any], requests: list[dict[str, Any]]) -> Any:
    programmes = {method: list(answers) for method, answers in scenario['responses'].items()}

    def _handler(request: httpx.Request) -> httpx.Response:
        requests.append(_recorded_request(request))
        method = request.url.path.rsplit('/', 1)[-1]
        answers = programmes.get(method)
        if answers:
            answer = answers.pop(0) if len(answers) > 1 else answers[0]
        elif method in {'pinChatMessage', 'unpinChatMessage'}:
            answer = _OK_TRUE
        else:
            answer = _OK_MESSAGE
        if 'json' in answer:
            return httpx.Response(answer['status'], json=answer['json'])
        return httpx.Response(answer['status'], text=answer['text'])

    return _handler


def _image_path(scenario: dict[str, Any], directory: Path) -> str | None:
    if 'image' in scenario:
        name, size = scenario['image']
        path = directory / name
        path.write_bytes(b'GIF89a' + b'\0' * (size - 6) if name.endswith('.gif') else b'\x89' * size)
        return str(path)
    if 'missing_image' in scenario:
        return str(directory / scenario['missing_image'])
    return None


async def _deliver(notification: NotificationRecord, handler: Any, config: telegram_bot.TelegramBotConfig) -> Any:
    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        return await telegram_bot.deliver(notification=notification, client=client, config=config)


def _deliver_cases() -> list[dict[str, Any]]:
    # The built-in table only: the system's /etc/mime.types differs between machines.
    mimetypes._db = mimetypes.MimeTypes()  # noqa: SLF001
    cases: list[dict[str, Any]] = []
    for scenario in _SCENARIOS:
        with tempfile.TemporaryDirectory() as raw_directory:
            requests: list[dict[str, Any]] = []
            notification = _notification(scenario['notification'], _image_path(scenario, Path(raw_directory)))
            handler = _scripted_handler(scenario, requests)
            config = _config(scenario.get('thread', 42))
            case: dict[str, Any] = {
                'label': scenario['label'],
                'thread': scenario.get('thread', 42),
                'image': list(scenario['image']) if 'image' in scenario else None,
                'missing_image': scenario.get('missing_image'),
                'notification': scenario['notification'],
                'responses': scenario['responses'],
            }
            try:
                result = asyncio.run(_deliver(notification, handler, config))
            except telegram_bot.TelegramDeliveryError as exc:
                case['error'] = {'message': str(exc), 'retryable': exc.retryable, 'retry_after_seconds': exc.retry_after_seconds}
            else:
                case['result'] = {
                    'message_id': result.message_id,
                    'media_status': result.media_status,
                    'warnings': list(result.warnings),
                    'pinned_message_id': result.pinned_message_id,
                }
            case['requests'] = requests
            cases.append(case)
    return cases


GOLDENS = {
    'notifications/render.json': _render,
    'notifications/text.json': _text,
    'notifications/send_message.json': _send_message_cases,
    'notifications/deliver.json': _deliver_cases,
}

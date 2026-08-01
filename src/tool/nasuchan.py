"""Nasuchan notification delivery.

This is deliberately Nasuchan-specific rather than a generic webhook layer: it
targets Nasuchan's v3 notification endpoint and its multipart image upload path.
"""

from __future__ import annotations

import asyncio
import json
import mimetypes
from dataclasses import dataclass
from typing import TYPE_CHECKING

import httpx

from src.core import logger, settings

if TYPE_CHECKING:
    from pathlib import Path

log = logger.get('nasuchan')

WEBHOOK_PATH = '/api/v3/notifications/webhook'
MAX_IMAGE_BYTES = 9_500_000

CONNECT_TIMEOUT_SECONDS = 5.0
WRITE_TIMEOUT_SECONDS = 30.0
READ_TIMEOUT_SECONDS = 90.0

_HTTP_STATUS_SUCCESS_MIN = 200
_HTTP_STATUS_SUCCESS_MAX = 299
_HTTP_STATUS_SERVER_ERROR_MIN = 500
_HTTP_STATUS_SERVER_ERROR_MAX = 599
# Nasuchan answers 413/415 when the multipart image is too large or unsupported;
# the notification itself is still deliverable without the upload.
IMAGE_FALLBACK_STATUS_CODES = frozenset({413, 415})
RETRYABLE_STATUS_CODES = frozenset({408, 409, 425, 429})


@dataclass(frozen=True, slots=True)
class NasuchanConfig:
    webhook_url: str
    token: str


class NasuchanNotConfiguredError(RuntimeError):
    pass


def load_config() -> NasuchanConfig | None:
    """Return the delivery config, or None when Nasuchan has not been set up yet."""
    cfg = settings.load().nasuchan
    if not cfg.configured:
        return None
    return NasuchanConfig(webhook_url=f'{cfg.base_url}{WEBHOOK_PATH}', token=cfg.token)


def build_client() -> httpx.AsyncClient:
    return httpx.AsyncClient(
        timeout=httpx.Timeout(
            connect=CONNECT_TIMEOUT_SECONDS,
            write=WRITE_TIMEOUT_SECONDS,
            read=READ_TIMEOUT_SECONDS,
            pool=CONNECT_TIMEOUT_SECONDS,
        ),
    )


def request_headers(token: str, *, idempotency_key: str | None = None) -> dict[str, str]:
    headers = {'Authorization': f'Bearer {token}'}
    if idempotency_key is not None:
        headers['Idempotency-Key'] = idempotency_key
    return headers


def error_message(*, status_code: int, response_text: str) -> str:
    detail = response_text.strip()
    if detail:
        return f'Nasuchan responded with HTTP {status_code}: {detail[:200]}'
    return f'Nasuchan responded with HTTP {status_code}'


def is_success_status_code(status_code: int) -> bool:
    return _HTTP_STATUS_SUCCESS_MIN <= status_code <= _HTTP_STATUS_SUCCESS_MAX


def is_retryable_status_code(status_code: int) -> bool:
    return status_code in RETRYABLE_STATUS_CODES or _HTTP_STATUS_SERVER_ERROR_MIN <= status_code <= _HTTP_STATUS_SERVER_ERROR_MAX


def read_bounded_image(image_path: Path) -> tuple[str, bytes, str] | None:
    content_type = mimetypes.guess_type(image_path.name)[0] or ''
    if not content_type.startswith('image/'):
        log.warning('Notification attachment %s is not a recognized image; using URL fallback', image_path)
        return None
    try:
        with image_path.open('rb') as handle:
            image_bytes = handle.read(MAX_IMAGE_BYTES + 1)
    except OSError as exc:
        log.warning('Failed to read notification image %s; using URL fallback: %s', image_path, exc)
        return None
    if len(image_bytes) > MAX_IMAGE_BYTES:
        log.warning(
            'Notification image %s exceeds upload limit of %s bytes; using URL fallback',
            image_path,
            MAX_IMAGE_BYTES,
        )
        return None
    return image_path.name, image_bytes, content_type


async def post_notification(
    *,
    notification,  # noqa: ANN001 - NotificationRecord, imported lazily to avoid a cycle
    client: httpx.AsyncClient,
    config: NasuchanConfig,
    include_local_image: bool,
) -> httpx.Response:
    payload = notification.webhook_v3_payload
    request_kwargs: dict[str, object] = {'json': payload}
    image_path = notification.local_image_path
    if include_local_image and image_path is not None:
        image_attachment = await asyncio.to_thread(read_bounded_image, image_path)
        if image_attachment is not None:
            filename, image_bytes, content_type = image_attachment
            request_kwargs = {
                'data': {'payload': json.dumps(payload, separators=(',', ':'))},
                'files': {'image': (filename, image_bytes, content_type)},
            }
    return await client.post(
        config.webhook_url,
        headers=request_headers(
            config.token,
            idempotency_key=f'fav:{notification.notification_id}:{notification.event_version}',
        ),
        **request_kwargs,
    )


async def deliver(
    *,
    notification,  # noqa: ANN001 - NotificationRecord
    client: httpx.AsyncClient,
    config: NasuchanConfig,
) -> httpx.Response:
    """POST a notification, retrying once without the image when Nasuchan rejects it."""
    response = await post_notification(
        notification=notification,
        client=client,
        config=config,
        include_local_image=True,
    )
    if response.status_code in IMAGE_FALLBACK_STATUS_CODES and notification.local_image_path is not None:
        log.warning(
            'Nasuchan rejected notification image %s with HTTP %s; retrying without upload',
            notification.notification_id,
            response.status_code,
        )
        response = await post_notification(
            notification=notification,
            client=client,
            config=config,
            include_local_image=False,
        )
    return response

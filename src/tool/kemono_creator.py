"""Kemono creator resolution.

Turns a pasted creator page URL (or a bare fanbox user id) into the
``{service, id, name}`` triple stored in ``web.kemono.creators``. The name comes
from the site's profile API, so adding a creator only needs the URL. The
worker-side crawl lives in ``src.web.kemono``.
"""

from __future__ import annotations

import re
from typing import TYPE_CHECKING, Any
from urllib.parse import urlsplit

import httpx

from src.core import settings

if TYPE_CHECKING:
    from collections.abc import Callable

# Same browser UA as the crawler: pawchive fronts the API with Cloudflare, which
# gates on IP reputation, but a browser UA stays off the obvious bot heuristics.
USER_AGENT = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36'

# Bare numeric input defaults to fanbox; other services need the full URL.
DEFAULT_SERVICE = 'fanbox'

_SERVICE_RE = re.compile(r'^[a-z0-9]+$')
_CREATOR_ID_RE = re.compile(r'^[A-Za-z0-9_-]+$')
_HTTP_OK = 200
_HTTP_NOT_FOUND = 404


def extract_kemono_creator(raw: str) -> tuple[str, str] | None:
    """Parse ``(service, id)`` from a creator page URL or a bare numeric id.

    Accepts any host (kemono-style domains change often); only the
    ``/{service}/user/{id}`` path shape matters.
    """
    text = raw.strip()
    if not text:
        return None
    if text.isdecimal():
        return (DEFAULT_SERVICE, text) if int(text) > 0 else None

    segments = [segment for segment in urlsplit(text).path.split('/') if segment]
    for index, segment in enumerate(segments):
        if segment != 'user' or index == 0 or index + 1 >= len(segments):
            continue
        service = segments[index - 1].lower()
        creator_id = segments[index + 1]
        if _SERVICE_RE.fullmatch(service) and _CREATOR_ID_RE.fullmatch(creator_id):
            return service, creator_id
    return None


class KemonoCreatorResolver:
    """Synchronous resolver the API uses; reads base_url per call so a settings
    edit takes effect without a restart."""

    def __init__(
        self,
        *,
        base_url_provider: Callable[[], str] | None = None,
        client: httpx.Client | None = None,
    ) -> None:
        self._base_url_provider = base_url_provider or (lambda: settings.load().web.kemono.base_url)
        self._owns_client = client is None
        self._client = client or httpx.Client(
            timeout=30,
            follow_redirects=True,
            headers={'User-Agent': USER_AGENT, 'Accept': 'application/json'},
        )

    def resolve(self, raw_creator: str) -> dict[str, str]:
        parsed = extract_kemono_creator(raw_creator)
        if parsed is None:
            msg = 'invalid_creator'
            raise ValueError(msg)
        service, creator_id = parsed

        base_url = self._base_url_provider().rstrip('/')
        url = f'{base_url}/api/v1/{service}/user/{creator_id}/profile'
        try:
            response = self._client.get(url)
        except Exception as exc:
            msg = 'creator_resolve_failed'
            raise ConnectionError(msg) from exc

        if response.status_code == _HTTP_NOT_FOUND:
            msg = 'creator_not_found'
            raise LookupError(msg)
        if response.status_code != _HTTP_OK:
            msg = 'creator_resolve_failed'
            raise ConnectionError(msg)

        try:
            profile: Any = response.json()
        except ValueError as exc:
            msg = 'creator_resolve_failed'
            raise ConnectionError(msg) from exc
        name = str(profile.get('name') or '').strip() if isinstance(profile, dict) else ''
        return {'service': service, 'id': creator_id, 'name': name or creator_id}

    def close(self) -> None:
        if self._owns_client:
            self._client.close()

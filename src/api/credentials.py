"""Signed-in sessions, served read-only to other services on the cluster.

fav is the one place a login is captured (the 登录态 page) and kept alive (the
crawlers renew what they can, see ``src/web/bilibili_session.py``). A service
that needs the same login -- RSSHub's Bilibili proxy -- reads it from here instead
of from a browser extension's sync, and never writes it back: a second writer
could refresh a session and retire the pair fav holds.

Each client has its own bearer token and may read only the sites it is
configured for (``CREDENTIAL_CLIENTS``). fav keeps the SHA-256 of each token, not
the token. These responses carry cookie values, so they are never logged and are
marked no-store.
"""

from __future__ import annotations

import hashlib
import re
import secrets
from dataclasses import dataclass
from typing import Annotated

from fastapi import APIRouter, Path, Request, Response
from pydantic import BaseModel

from src.core import logger
from src.tool import site_sessions

from .constants import API_V2_PREFIX, AUTH_PREFIX, HEADER_AUTHORIZATION, WWW_AUTHENTICATE_BEARER
from .errors import ApiError

log = logger.get('fav-api')

TAG_CREDENTIALS = 'credentials'
_SHA256_HEX = re.compile(r'^[0-9a-f]{64}$')
_NAME = re.compile(r'^[A-Za-z0-9_-]+$')


@dataclass(frozen=True)
class CredentialClient:
    name: str
    token_sha256: str
    sites: frozenset[str]


def parse_clients(raw: str) -> tuple[CredentialClient, ...]:
    """``name:sha256:site,site`` entries, separated by whitespace or ';'."""
    clients: list[CredentialClient] = []
    for entry in re.split(r'[\s;]+', raw.strip()):
        if not entry:
            continue
        parts = entry.split(':')
        if len(parts) != 3:  # noqa: PLR2004 - name, hash, sites
            msg = 'CREDENTIAL_CLIENTS entries must be name:sha256:site[,site...]'
            raise ValueError(msg)
        name, digest, sites = parts[0], parts[1].lower(), [s for s in parts[2].split(',') if s]
        if not _NAME.match(name) or not _SHA256_HEX.match(digest) or not sites or not all(_NAME.match(s) for s in sites):
            msg = f'CREDENTIAL_CLIENTS entry for {name or "?"} is malformed'
            raise ValueError(msg)
        clients.append(CredentialClient(name=name, token_sha256=digest, sites=frozenset(sites)))
    return tuple(clients)


class CredentialCookie(BaseModel):
    name: str
    value: str
    domain: str
    path: str
    # Unix seconds; null for a session cookie.
    expires: float | None


class SiteCredential(BaseModel):
    site: str
    account: str
    # 'valid' or 'invalid', as the last crawl or check found it.
    status: str
    signed_in_at: str | None
    checked_at: str | None
    cookies: list[CredentialCookie]


_TOKEN_REQUIRED = 'A Bearer token is required.'  # noqa: S105 - a message


def _unauthorized(*, code: str) -> ApiError:
    return ApiError(status_code=401, code=code, message=_TOKEN_REQUIRED, headers={'WWW-Authenticate': WWW_AUTHENTICATE_BEARER})


def authenticate_client(clients: tuple[CredentialClient, ...], authorization: str | None) -> CredentialClient:
    if not authorization or not authorization.startswith(AUTH_PREFIX):
        raise _unauthorized(code='missing_authorization')
    token = authorization[len(AUTH_PREFIX) :].strip()
    if not token:
        raise _unauthorized(code='missing_bearer_token')
    digest = hashlib.sha256(token.encode()).hexdigest()
    # Every entry is compared, so the time taken does not say which one matched.
    matched = [client for client in clients if secrets.compare_digest(digest, client.token_sha256)]
    if not matched:
        raise ApiError(status_code=403, code='invalid_token', message='Bearer token is not valid.')
    return matched[0]


router = APIRouter(prefix=API_V2_PREFIX, tags=[TAG_CREDENTIALS])


@router.get('/credentials/{site}', operation_id='getSiteCredential', response_model=SiteCredential)
async def get_site_credential(
    site: Annotated[str, Path(min_length=1, max_length=96, pattern=r'^[A-Za-z0-9_-]+$')],
    request: Request,
    response: Response,
) -> SiteCredential:
    clients: tuple[CredentialClient, ...] = getattr(request.app.state, 'credential_clients', ())
    client = authenticate_client(clients, request.headers.get(HEADER_AUTHORIZATION))
    if site not in client.sites:
        raise ApiError(status_code=403, code='site_not_allowed', message=f'This client may not read {site}.')
    stored = await site_sessions.load(site)
    cookies = [c for c in stored.cookies if c.value and c.value != site_sessions.VALUE_IN_PROFILE] if stored else []
    if stored is None or not cookies:
        raise ApiError(status_code=404, code='not_signed_in', message=f'{site} has no stored session.')
    response.headers['Cache-Control'] = 'no-store'
    log.info('Served the %s session to %s', site, client.name)
    return SiteCredential(
        site=site,
        account=stored.account,
        status=stored.status,
        signed_in_at=stored.signed_in_at.isoformat() if stored.signed_in_at else None,
        checked_at=stored.checked_at.isoformat() if stored.checked_at else None,
        cookies=[CredentialCookie(name=c.name, value=c.value, domain=c.domain, path=c.path, expires=c.expires) for c in cookies],
    )

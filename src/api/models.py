from __future__ import annotations

from dataclasses import dataclass
from http import HTTPStatus

from .constants import _DEFAULT_BIND, _DEFAULT_PORT


@dataclass(frozen=True, slots=True)
class ApiConfig:
    dsn: str
    token: str
    bind: str = _DEFAULT_BIND
    port: int = _DEFAULT_PORT


@dataclass(frozen=True, slots=True)
class ApiResponse:
    status: HTTPStatus
    headers: dict[str, str]
    body: bytes | None

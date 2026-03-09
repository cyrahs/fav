from __future__ import annotations

from dataclasses import dataclass

from .constants import DEFAULT_BIND, DEFAULT_PORT


@dataclass(frozen=True, slots=True)
class ApiConfig:
    dsn: str
    token: str
    bind: str = DEFAULT_BIND
    port: int = DEFAULT_PORT

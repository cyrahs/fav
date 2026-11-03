# ruff: noqa: INP001, S101, S105

from __future__ import annotations

from typing import Any, Self

import pytest

from src.core import settings


class _Cursor:
    def __init__(self, executed: list[tuple[str, Any]]) -> None:
        self.executed = executed

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_exc: object) -> None:
        return None

    def execute(self, query: str, params: Any = None) -> None:
        self.executed.append((' '.join(query.split()), params))


class _Connection:
    def __init__(self, executed: list[tuple[str, Any]]) -> None:
        self.executed = executed

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_exc: object) -> None:
        return None

    def cursor(self) -> _Cursor:
        return _Cursor(self.executed)


@pytest.fixture
def executed(monkeypatch: pytest.MonkeyPatch) -> list[tuple[str, Any]]:
    statements: list[tuple[str, Any]] = []
    monkeypatch.setattr(settings, '_connect', lambda: _Connection(statements))
    monkeypatch.setattr(settings, '_table_ready', False)
    return statements


def test_the_retired_cookiecloud_section_is_deleted_with_its_vault_passwords(executed: list[tuple[str, Any]]) -> None:
    settings.ensure_table_sync()

    assert ('DELETE FROM app_settings WHERE section = ANY(%s);', (['cookiecloud'],)) in executed


def test_a_row_left_by_the_retired_cookiecloud_section_is_not_read() -> None:
    loaded = settings.build_settings(
        {
            'cookiecloud': {'configs': [{'name': 'main', 'server_url': 'https://cc', 'uuid': 'u', 'password': 'p'}]},
            'web.wechat': {'token': 't', 'nga_cookiecloud': 'main'},
        }
    )

    assert not hasattr(loaded, 'cookiecloud')
    assert 'nga_cookiecloud' not in loaded.web.wechat.model_dump()
    assert loaded.web.wechat.token == 't'

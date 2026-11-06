# ruff: noqa: INP001, S101, S105

from __future__ import annotations

import json
from contextlib import nullcontext
from typing import Any, Self

import pytest

from src.core import settings


class _Cursor:
    def __init__(self, executed: list[tuple[str, Any]], rows: list[dict[str, Any]] | None = None) -> None:
        self.executed = executed
        self.rows = rows or []

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_exc: object) -> None:
        return None

    def execute(self, query: str, params: Any = None) -> None:
        self.executed.append((' '.join(query.split()), params))

    def fetchall(self) -> list[dict[str, Any]]:
        return self.rows


class _Connection:
    def __init__(self, executed: list[tuple[str, Any]], rows: list[dict[str, Any]] | None = None) -> None:
        self.executed = executed
        self.rows = rows

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_exc: object) -> None:
        return None

    def cursor(self) -> _Cursor:
        return _Cursor(self.executed, self.rows)

    def transaction(self) -> nullcontext[None]:
        return nullcontext()


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


def test_the_tikhub_key_moves_out_of_the_wechat_section_once() -> None:
    rows = {'web.wechat': {'token': 't', 'tikhub_token': 'key', 'tikhub_api_url': 'https://api.tikhub.dev'}}

    changed = settings.moved_fields(rows)

    assert changed == {
        'web.wechat': {'token': 't'},
        'services.tikhub': {'token': 'key', 'api_url': 'https://api.tikhub.dev'},
    }
    # Nothing left to move once the old fields are gone.
    assert settings.moved_fields({**rows, **changed}) == {}


def test_a_tikhub_key_already_set_in_its_own_section_wins() -> None:
    rows = {'web.wechat': {'token': 't', 'tikhub_token': 'old'}, 'services.tikhub': {'token': 'new'}}

    assert settings.moved_fields(rows) == {'web.wechat': {'token': 't'}}


def test_ensure_table_writes_the_moved_fields(monkeypatch: pytest.MonkeyPatch) -> None:
    statements: list[tuple[str, Any]] = []
    stored = [{'section': 'web.wechat', 'value': {'token': 't', 'tikhub_token': 'key'}}]
    monkeypatch.setattr(settings, '_connect', lambda: _Connection(statements, stored))
    monkeypatch.setattr(settings, '_table_ready', False)

    settings.ensure_table_sync()

    written = {params[0]: json.loads(params[1]) for query, params in statements if query.startswith('INSERT INTO app_settings')}
    assert written == {'web.wechat': {'token': 't'}, 'services.tikhub': {'token': 'key'}}

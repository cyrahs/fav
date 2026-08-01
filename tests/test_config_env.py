# ruff: noqa: INP001, PLR2004, S101, S104, S105, S106

import pytest
from pydantic import ValidationError

from src.core.env import Env


def _env(monkeypatch, **overrides: str) -> Env:  # noqa: ANN001
    # Env reads a .env file by default; clear the real values so these tests do
    # not depend on the developer's local bootstrap file.
    for name in (
        'POSTGRES_DSN',
        'API_TOKEN',
        'API_BIND',
        'API_PORT',
        'API_CORS_ORIGINS',
        'API_CORS_ALLOW_CREDENTIALS',
    ):
        monkeypatch.delenv(name, raising=False)
    for name, value in overrides.items():
        monkeypatch.setenv(name, value)
    return Env(_env_file=None)


def test_env_reads_required_bootstrap_values(monkeypatch) -> None:  # noqa: ANN001
    env = _env(monkeypatch, POSTGRES_DSN='postgresql://user:pass@db.local/fav', API_TOKEN='abc123')

    assert env.postgres_dsn == 'postgresql://user:pass@db.local/fav'
    assert env.api_token == 'abc123'
    assert env.api_bind == '0.0.0.0'
    assert env.api_port == 8091


def test_env_overrides_bind_and_port(monkeypatch) -> None:  # noqa: ANN001
    env = _env(
        monkeypatch,
        POSTGRES_DSN='postgresql://db.local/fav',
        API_TOKEN='abc123',
        API_BIND='127.0.0.1',
        API_PORT='9000',
    )

    assert env.api_bind == '127.0.0.1'
    assert env.api_port == 9000


def test_env_rejects_missing_postgres_dsn(monkeypatch) -> None:  # noqa: ANN001
    with pytest.raises(ValidationError):
        _env(monkeypatch, API_TOKEN='abc123')


def test_env_rejects_empty_api_token(monkeypatch) -> None:  # noqa: ANN001
    # An empty token would leave the settings API world-writable on a fresh
    # deployment, so this must fail loudly at startup.
    with pytest.raises(ValidationError):
        _env(monkeypatch, POSTGRES_DSN='postgresql://db.local/fav', API_TOKEN='   ')


def test_env_rejects_out_of_range_port(monkeypatch) -> None:  # noqa: ANN001
    with pytest.raises(ValidationError):
        _env(monkeypatch, POSTGRES_DSN='postgresql://db.local/fav', API_TOKEN='abc123', API_PORT='70000')


def test_cors_origins_are_split_trimmed_and_deduped(monkeypatch) -> None:  # noqa: ANN001
    env = _env(
        monkeypatch,
        POSTGRES_DSN='postgresql://db.local/fav',
        API_TOKEN='abc123',
        API_CORS_ORIGINS=' https://a.example/, https://b.example ,, https://a.example ',
    )

    assert env.cors_origins == ('https://a.example', 'https://b.example')


def test_cors_origins_default_to_empty(monkeypatch) -> None:  # noqa: ANN001
    env = _env(monkeypatch, POSTGRES_DSN='postgresql://db.local/fav', API_TOKEN='abc123')

    assert env.cors_origins == ()

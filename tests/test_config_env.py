# ruff: noqa: INP001, S101, S105

from pathlib import Path

from src.core.config import Config


def test_env_nested_override_web_bilibili_path(monkeypatch, tmp_path) -> None:  # noqa: ANN001
    overridden_path = tmp_path / 'bilibili-from-env'
    monkeypatch.setenv('WEB__BILIBILI__PATH', str(overridden_path))

    cfg = Config()

    assert cfg.web.bilibili.path == Path(overridden_path)


def test_env_nested_override_api_token(monkeypatch) -> None:  # noqa: ANN001
    monkeypatch.setenv('API__TOKEN', 'token-from-env')

    cfg = Config()

    assert cfg.api.token == 'token-from-env'


def test_env_nested_override_notifications_settings(monkeypatch) -> None:  # noqa: ANN001
    monkeypatch.setenv('NOTIFICATIONS__WEBHOOK_BASE_URL', 'https://hooks.example.com/base/')
    monkeypatch.setenv('NOTIFICATIONS__WEBHOOK_TOKEN', 'webhook-token')

    cfg = Config()

    assert cfg.notifications.webhook_base_url == 'https://hooks.example.com/base'
    assert cfg.notifications.webhook_token == 'webhook-token'

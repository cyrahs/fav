# ruff: noqa: INP001, S101

from pathlib import Path

from src.core.config import Config


def test_env_nested_override_web_bilibili_path(monkeypatch, tmp_path) -> None:  # noqa: ANN001
    overridden_path = tmp_path / 'bilibili-from-env'
    monkeypatch.setenv('WEB__BILIBILI__PATH', str(overridden_path))

    cfg = Config()

    assert cfg.web.bilibili.path == Path(overridden_path)

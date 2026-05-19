# ruff: noqa: INP001, S101

import asyncio
from pathlib import Path
from types import SimpleNamespace

import pytest

import src.service.jobs as jobs_module
from src.api.schemas import JobRequestTarget
from src.core.config import AzurLane as AzurLaneConfig
from src.web import AzurLane


def _job_cfg(*, enabled: bool = True) -> SimpleNamespace:
    return SimpleNamespace(cron='0 */6 * * *', enabled=enabled, run_on_start=False)


def test_azurlane_config_defaults_to_disabled_collection_path() -> None:
    cfg = AzurLaneConfig()

    assert cfg.enabled is False
    assert cfg.cron == '0 */6 * * *'
    assert cfg.run_on_start is False
    assert cfg.path == Path('./collection/azurlane')


def test_scheduler_registration_includes_azurlane(monkeypatch: pytest.MonkeyPatch) -> None:
    fake_config = SimpleNamespace(
        web=SimpleNamespace(
            azurlane=_job_cfg(enabled=False),
            bd2=_job_cfg(),
            bilibili=_job_cfg(),
            hanime1=_job_cfg(),
            jandan=_job_cfg(),
            nikke=_job_cfg(),
            stellasora=_job_cfg(),
            telegram=_job_cfg(),
        ),
    )
    monkeypatch.setattr(jobs_module, 'config', fake_config)

    jobs = jobs_module.build_jobs()
    azurlane_job = next(job for job in jobs if job.key == 'azurlane')

    assert azurlane_job.name == 'Azur Lane'
    assert azurlane_job.enabled is False
    assert azurlane_job.required_commands == ()
    assert azurlane_job.factory is jobs_module.AzurLane


def test_api_job_enum_includes_azurlane() -> None:
    assert JobRequestTarget.AZURLANE.value == 'azurlane'


def test_azurlane_placeholder_update_does_not_write_collection_path(tmp_path: Path) -> None:
    collection_path = tmp_path / 'azurlane'
    crawler = AzurLane(path=collection_path)

    asyncio.run(crawler.update())

    assert not collection_path.exists()

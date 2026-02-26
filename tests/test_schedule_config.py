# ruff: noqa: INP001, S101

import pytest
from pydantic import ValidationError

from src.core.config import ScheduleJob


def test_schedule_job_accepts_five_field_cron() -> None:
    job = ScheduleJob(cron='*/5 * * * *')

    assert job.cron == '*/5 * * * *'
    assert job.enabled is True
    assert job.run_on_start is True


def test_schedule_job_rejects_non_five_field_cron() -> None:
    with pytest.raises(ValidationError):
        ScheduleJob(cron='*/5 * * *')

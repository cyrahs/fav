# ruff: noqa: INP001, S101

import pytest
from pydantic import ValidationError

from src.core.config import Hanime1Ranking, ScheduleJob


def test_schedule_job_accepts_five_field_cron() -> None:
    job = ScheduleJob(cron='*/5 * * * *')

    assert job.cron == '*/5 * * * *'
    assert job.enabled is True
    assert job.run_on_start is False


def test_schedule_job_rejects_non_five_field_cron() -> None:
    with pytest.raises(ValidationError):
        ScheduleJob(cron='*/5 * * *')


def test_hanime1_ranking_dedupes_periods() -> None:
    ranking = Hanime1Ranking(enabled=True, periods=['weekly', 'weekly', 'monthly'], pages=1)

    assert ranking.enabled is True
    assert ranking.periods == ['weekly', 'monthly']
    assert ranking.pages == 1


def test_hanime1_ranking_rejects_empty_periods() -> None:
    with pytest.raises(ValidationError):
        Hanime1Ranking(periods=[])


def test_hanime1_ranking_rejects_non_positive_pages() -> None:
    with pytest.raises(ValidationError):
        Hanime1Ranking(pages=0)

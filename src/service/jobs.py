from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

from src.core.config import config
from src.web import BD2, Bilibili, Hanime1, Jandan, Nikke, StellaSora, Telegram

if TYPE_CHECKING:
    from collections.abc import Callable


@dataclass(frozen=True, slots=True)
class ScheduledJob:
    key: str
    name: str
    cron: str
    enabled: bool
    run_on_start: bool
    required_commands: tuple[str, ...]
    factory: Callable[[], object]

    def as_public_dict(self) -> dict[str, str | bool]:
        return {
            'key': self.key,
            'name': self.name,
            'enabled': self.enabled,
            'run_on_start': self.run_on_start,
        }


def build_jobs() -> list[ScheduledJob]:
    bilibili_cfg = config.web.bilibili
    jandan_cfg = config.web.jandan
    telegram_cfg = config.web.telegram
    stellasora_cfg = config.web.stellasora
    hanime1_cfg = config.web.hanime1
    nikke_cfg = config.web.nikke
    bd2_cfg = config.web.bd2
    return [
        ScheduledJob(
            key='bilibili',
            name='Bilibili',
            cron=bilibili_cfg.cron,
            enabled=bilibili_cfg.enabled,
            run_on_start=bilibili_cfg.run_on_start,
            required_commands=('yt-dlp',),
            factory=Bilibili,
        ),
        ScheduledJob(
            key='hanime1',
            name='Hanime1',
            cron=hanime1_cfg.cron,
            enabled=hanime1_cfg.enabled,
            run_on_start=hanime1_cfg.run_on_start,
            required_commands=('yt-dlp',),
            factory=Hanime1,
        ),
        ScheduledJob(
            key='jandan',
            name='Jandan',
            cron=jandan_cfg.cron,
            enabled=jandan_cfg.enabled,
            run_on_start=jandan_cfg.run_on_start,
            required_commands=(),
            factory=Jandan,
        ),
        ScheduledJob(
            key='nikke',
            name='Nikke',
            cron=nikke_cfg.cron,
            enabled=nikke_cfg.enabled,
            run_on_start=nikke_cfg.run_on_start,
            required_commands=(),
            factory=Nikke,
        ),
        ScheduledJob(
            key='bd2',
            name='BD2',
            cron=bd2_cfg.cron,
            enabled=bd2_cfg.enabled,
            run_on_start=bd2_cfg.run_on_start,
            required_commands=(),
            factory=BD2,
        ),
        ScheduledJob(
            key='telegram',
            name='Telegram',
            cron=telegram_cfg.cron,
            enabled=telegram_cfg.enabled,
            run_on_start=telegram_cfg.run_on_start,
            required_commands=(),
            factory=Telegram,
        ),
        ScheduledJob(
            key='stellasora',
            name='StellaSora',
            cron=stellasora_cfg.cron,
            enabled=stellasora_cfg.enabled,
            run_on_start=stellasora_cfg.run_on_start,
            required_commands=(),
            factory=StellaSora,
        ),
    ]


def resolve_trigger_jobs(trigger_target: str, all_jobs: list[ScheduledJob]) -> list[ScheduledJob]:
    normalized = trigger_target.strip().lower()
    if not normalized:
        msg = 'Trigger target cannot be empty'
        raise SystemExit(msg)

    if normalized == 'all':
        selected_jobs = [job for job in all_jobs if job.enabled]
        if selected_jobs:
            return selected_jobs
        msg = 'No enabled jobs available for --trigger all'
        raise SystemExit(msg)

    job_by_key = {job.key: job for job in all_jobs}
    selected_job = job_by_key.get(normalized)
    if selected_job is not None:
        return [selected_job]

    valid_targets = ', '.join(['all', *sorted(job_by_key)])
    msg = f'Unknown --trigger target: {trigger_target!r}. Valid targets: {valid_targets}'
    raise SystemExit(msg)

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from src.core import settings
from src.web import BD2, AzurLane, Bilibili, Hanime1, Jandan, Kemono, Nikke, RedNote, StellaSora, Telegram, Twitter

if TYPE_CHECKING:
    from collections.abc import Callable


@dataclass(frozen=True, slots=True)
class ScheduledJob:
    key: str
    name: str
    cron: str
    enabled: bool
    required_commands: tuple[str, ...]
    factory: Callable[[], object]
    section: str = ''
    missing_fields: tuple[str, ...] = field(default=())

    def as_public_dict(self) -> dict[str, Any]:
        return {
            'key': self.key,
            'name': self.name,
            'cron': self.cron,
            'enabled': self.enabled,
            'section': self.section,
            'missing_fields': list(self.missing_fields),
        }


@dataclass(frozen=True, slots=True)
class JobSpec:
    key: str
    name: str
    attr: str
    required_commands: tuple[str, ...]
    factory: Callable[[], object]


JOB_SPECS: tuple[JobSpec, ...] = (
    JobSpec(key='bilibili', name='Bilibili', attr='bilibili', required_commands=('yt-dlp',), factory=Bilibili),
    JobSpec(key='hanime1', name='Hanime1', attr='hanime1', required_commands=('yt-dlp',), factory=Hanime1),
    JobSpec(key='jandan', name='Jandan', attr='jandan', required_commands=(), factory=Jandan),
    JobSpec(key='kemono', name='Kemono', attr='kemono', required_commands=(), factory=Kemono),
    JobSpec(key='nikke', name='Nikke', attr='nikke', required_commands=(), factory=Nikke),
    JobSpec(key='bd2', name='BD2', attr='bd2', required_commands=(), factory=BD2),
    JobSpec(key='azurlane', name='Azur Lane', attr='azurlane', required_commands=(), factory=AzurLane),
    JobSpec(key='telegram', name='Telegram', attr='telegram', required_commands=(), factory=Telegram),
    JobSpec(key='stellasora', name='Stella Sora', attr='stellasora', required_commands=(), factory=StellaSora),
    JobSpec(key='twitter', name='X', attr='twitter', required_commands=('gallery-dl',), factory=Twitter),
    JobSpec(key='rednote', name='RedNote', attr='rednote', required_commands=(), factory=RedNote),
)

JOB_KEYS: tuple[str, ...] = tuple(spec.key for spec in JOB_SPECS)


def build_jobs(current: settings.Settings | None = None) -> list[ScheduledJob]:
    resolved = current if current is not None else settings.load()
    jobs: list[ScheduledJob] = []
    for spec in JOB_SPECS:
        job_cfg = getattr(resolved.web, spec.attr)
        missing = tuple(job_cfg.validate_runnable())
        jobs.append(
            ScheduledJob(
                key=spec.key,
                name=spec.name,
                cron=job_cfg.cron,
                # A source with an incomplete configuration stays out of the
                # scheduler even when the UI toggle says enabled.
                enabled=job_cfg.enabled and not missing,
                required_commands=spec.required_commands,
                factory=spec.factory,
                section=f'web.{spec.attr}',
                missing_fields=missing,
            ),
        )
    return jobs


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

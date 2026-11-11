"""`src/service/jobs.py`: the source registry, the jobs built from default settings, and `fav trigger` targets (WP2)."""

from __future__ import annotations

from typing import Any

from src.core import settings
from src.service.jobs import JOB_SPECS, build_jobs, resolve_trigger_jobs


def _specs() -> list[dict[str, Any]]:
    return [
        {'key': spec.key, 'name': spec.name, 'attr': spec.attr, 'required_commands': list(spec.required_commands)} for spec in JOB_SPECS
    ]


def _default_jobs() -> list[dict[str, Any]]:
    return [job.as_public_dict() for job in build_jobs()]


_TARGETS = ['all', 'ALL', ' Jandan ', 'jandan', 'kemono', 'twitter', '', '   ', 'nope', 'Azur Lane', 'azurlane']


def _trigger_targets() -> list[dict[str, Any]]:
    """`resolve_trigger_jobs` (`fav trigger`) with nothing enabled, then with two runnable sources and a parked one on."""
    cases: list[dict[str, Any]] = []
    for enabled in ([], ['hanime1', 'nikke', 'jandan']):
        current = settings.Settings()
        for key in enabled:
            getattr(current.web, key).enabled = True
        jobs = build_jobs(current)
        for target in _TARGETS:
            case: dict[str, Any] = {'enabled': enabled, 'target': target}
            try:
                case['selected'] = [job.key for job in resolve_trigger_jobs(target, jobs)]
            except SystemExit as exc:
                case['exit'] = str(exc)
            cases.append(case)
    return cases


GOLDENS = {
    'jobs/trigger_targets.json': _trigger_targets,
    'jobs/job_specs.json': _specs,
    'jobs/default_jobs.json': _default_jobs,
}

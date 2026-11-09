"""`src/service/jobs.py`: the source registry and the jobs built from default settings (WP2)."""

from __future__ import annotations

from typing import Any

from src.service.jobs import JOB_SPECS, build_jobs


def _specs() -> list[dict[str, Any]]:
    return [
        {'key': spec.key, 'name': spec.name, 'attr': spec.attr, 'required_commands': list(spec.required_commands)} for spec in JOB_SPECS
    ]


def _default_jobs() -> list[dict[str, Any]]:
    return [job.as_public_dict() for job in build_jobs()]


GOLDENS = {
    'jobs/job_specs.json': _specs,
    'jobs/default_jobs.json': _default_jobs,
}

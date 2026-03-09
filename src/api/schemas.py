from __future__ import annotations

from enum import StrEnum
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field


class ApiSchema(BaseModel):
    model_config = ConfigDict(extra='forbid')


class ErrorDetail(ApiSchema):
    code: str
    message: str
    details: Any | None = None


class ErrorResponse(ApiSchema):
    error: ErrorDetail


class HealthResponse(ApiSchema):
    status: Literal['ok']
    generated_at: str


class JobRequestTarget(StrEnum):
    ALL = 'all'
    BILIBILI = 'bilibili'
    HANIME1 = 'hanime1'
    JANDAN = 'jandan'
    STELLASORA = 'stellasora'
    TELEGRAM = 'telegram'


class JobRequestStatus(StrEnum):
    FAILED = 'failed'
    PENDING = 'pending'
    REJECTED = 'rejected'
    RUNNING = 'running'
    SUCCEEDED = 'succeeded'


class JobSummary(ApiSchema):
    key: str
    name: str
    enabled: bool
    run_on_start: bool
    cron: str


class JobListResponse(ApiSchema):
    items: list[JobSummary]
    total: int


class JobRequestCreate(ApiSchema):
    target: JobRequestTarget


class JobRequest(ApiSchema):
    id: int
    target: JobRequestTarget
    status: JobRequestStatus
    requested_at: str
    started_at: str | None = None
    finished_at: str | None = None
    result: str = ''
    error: str = ''


class Hanime1Seed(ApiSchema):
    video_id: str
    title: str
    label: str


class Hanime1Video(ApiSchema):
    video_id: str
    title: str
    downloaded: bool
    uploader: str | None = None
    release_date: str | None = None
    plot: str | None = None
    watch_url: str


class Hanime1ListResponse(ApiSchema):
    items: list[Hanime1Video]
    total: int


class Hanime1SeedListResponse(ApiSchema):
    items: list[Hanime1Seed]
    total: int


class Hanime1SeedCreate(ApiSchema):
    seed: str = Field(min_length=1)

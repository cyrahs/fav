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
    BD2 = 'bd2'
    BILIBILI = 'bilibili'
    HANIME1 = 'hanime1'
    JANDAN = 'jandan'
    NIKKE = 'nikke'
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


class Hanime1SeedCreate(ApiSchema):
    seed: str = Field(min_length=1)


class NikkeAsset(ApiSchema):
    kind: str
    path: str
    url: str
    content_type: str = ''
    size: int = 0
    sha256: str = ''
    status: str = ''
    label: str = ''
    field: str = ''
    skin_index: int | None = None
    live2d_key: str = ''
    available: bool = False
    contexts: list[dict[str, Any]] = Field(default_factory=list)


class NikkeProfileItem(ApiSchema):
    key: str
    label: str
    value: str = ''
    asset: NikkeAsset | None = None


class NikkeCharacterSummary(ApiSchema):
    content_id: int
    title: str
    directory_name: str = ''
    source_url: str = ''
    updated_at: int | str | None = None
    fetched_at: str | None = None
    asset_counts: dict[str, int] = Field(default_factory=dict)
    tags: dict[str, str] = Field(default_factory=dict)
    profile: list[NikkeProfileItem] = Field(default_factory=list)
    icon: NikkeAsset | None = None
    portrait: NikkeAsset | None = None
    skin_count: int = 0
    live2d_model_count: int = 0


class NikkeCharacterListResponse(ApiSchema):
    items: list[NikkeCharacterSummary]
    total: int
    limit: int | None = None
    offset: int = 0


class NikkeSidebarIcon(ApiSchema):
    url: str
    available: bool
    sha256: str
    content_type: str


class NikkeSidebarCharacter(ApiSchema):
    content_id: int
    title: str
    icon: NikkeSidebarIcon
    implemented_at: str | None = None
    updated_at: int | str | None = None
    fetched_at: str | None = None


class NikkeSidebarCharacterListResponse(ApiSchema):
    items: list[NikkeSidebarCharacter]
    total: int


class NikkeLive2DModel(ApiSchema):
    label: str = ''
    section: str = ''
    row_index: int | None = None
    skin_index: int | None = None
    skin_name: str = ''
    skin_title: str = ''
    skin_series: str = ''
    skin_obtain: str = ''
    is_collection_skin: bool = False
    key: str = ''
    stable_id: str = ''
    live2d_key: str = ''
    animation: str = ''
    skin: str = ''
    limit_age: bool = False
    position: dict[str, Any] = Field(default_factory=dict)
    bg_position: dict[str, Any] = Field(default_factory=dict)
    source_urls: dict[str, Any] = Field(default_factory=dict)
    assets: dict[str, Any] = Field(default_factory=dict)


class NikkeVoiceLine(ApiSchema):
    label: str
    text: str = ''
    source_url: str = ''


class NikkeSkin(ApiSchema):
    skin_index: int | None = None
    name: str = ''
    title: str = ''
    series: str = ''
    obtain: str = ''
    is_collection_skin: bool = False
    thumbnail: NikkeAsset | None = None
    portrait: NikkeAsset | None = None
    sd_model: NikkeAsset | None = None
    burst_animation: NikkeAsset | None = None
    gallery: list[NikkeAsset] = Field(default_factory=list)
    live2d_models: list[NikkeLive2DModel] = Field(default_factory=list)
    voice_lines: list[NikkeVoiceLine] = Field(default_factory=list)
    rows: list[dict[str, Any]] = Field(default_factory=list)


class NikkeCharacterDetail(NikkeCharacterSummary):
    tj_list: dict[str, Any] | None = None
    base_info: dict[str, Any] = Field(default_factory=dict)
    skins: list[NikkeSkin] = Field(default_factory=list)
    live2d_models: list[NikkeLive2DModel] = Field(default_factory=list)
    assets: list[NikkeAsset] = Field(default_factory=list)

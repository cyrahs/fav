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


class BD2Asset(ApiSchema):
    kind: str
    path: str
    url: str
    content_type: str = ''
    size: int = 0
    sha256: str = ''
    status: str = ''
    label: str = ''
    field: str = ''
    style_index: int | None = None
    style_name: str = ''
    costume_title: str = ''
    costume_category: str = ''
    column_role: str = ''
    live2d_key: str = ''
    available: bool = False
    contexts: list[dict[str, Any]] = Field(default_factory=list)


class BD2ProfileItem(ApiSchema):
    key: str
    label: str
    value: str = ''
    asset: BD2Asset | None = None


class BD2CharacterSummary(ApiSchema):
    content_id: int
    title: str
    directory_name: str = ''
    source_url: str = ''
    updated_at: int | str | None = None
    fetched_at: str | None = None
    asset_counts: dict[str, int] = Field(default_factory=dict)
    tags: dict[str, str] = Field(default_factory=dict)
    profile: list[BD2ProfileItem] = Field(default_factory=list)
    icon: BD2Asset | None = None
    portrait: BD2Asset | None = None
    costume_count: int = 0
    live2d_model_count: int = 0
    search_terms: list[str] = Field(default_factory=list)


class BD2CharacterListResponse(ApiSchema):
    items: list[BD2CharacterSummary]
    total: int
    limit: int | None = None
    offset: int = 0


class BD2SidebarIcon(ApiSchema):
    url: str
    available: bool
    sha256: str
    content_type: str


class BD2SidebarCharacter(ApiSchema):
    content_id: int
    title: str
    icon: BD2SidebarIcon
    updated_at: int | str | None = None
    fetched_at: str | None = None


class BD2SidebarCharacterListResponse(ApiSchema):
    items: list[BD2SidebarCharacter]
    total: int


class BD2Live2DModel(ApiSchema):
    label: str = ''
    section: str = ''
    style_index: int | None = None
    style_name: str = ''
    costume_title: str = ''
    costume_category: str = ''
    row_index: int | None = None
    column_index: int | None = None
    field: str = ''
    is_art_row: bool = False
    column_name: str = ''
    column_category: str = ''
    column_role: str = ''
    column_header: str = ''
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


class BD2Costume(ApiSchema):
    style_index: int | None = None
    style_name: str = ''
    title: str = ''
    category: str = ''
    sprite: BD2Asset | None = None
    portrait: BD2Asset | None = None
    full_portrait: BD2Asset | None = None
    gallery: list[BD2Asset] = Field(default_factory=list)
    videos: list[BD2Asset] = Field(default_factory=list)
    audio: list[BD2Asset] = Field(default_factory=list)
    live2d_models: list[BD2Live2DModel] = Field(default_factory=list)
    rows: list[dict[str, Any]] = Field(default_factory=list)


class BD2CharacterDetail(BD2CharacterSummary):
    tree_row: dict[str, Any] | None = None
    base_info: dict[str, Any] = Field(default_factory=dict)
    costumes: list[BD2Costume] = Field(default_factory=list)
    live2d_models: list[BD2Live2DModel] = Field(default_factory=list)
    assets: list[BD2Asset] = Field(default_factory=list)


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

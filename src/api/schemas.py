from __future__ import annotations

import math
from enum import StrEnum
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field, field_validator


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


class ComponentReadiness(ApiSchema):
    status: Literal['ok', 'degraded', 'skipped']
    code: str
    message: str
    sampled_targets: int = Field(ge=0)


class ReadinessResponse(ApiSchema):
    status: Literal['ok', 'degraded']
    generated_at: str
    checks: dict[str, ComponentReadiness]


class JobRequestTarget(StrEnum):
    ALL = 'all'
    AZURLANE = 'azurlane'
    BD2 = 'bd2'
    BILIBILI = 'bilibili'
    HANIME1 = 'hanime1'
    JANDAN = 'jandan'
    KEMONO = 'kemono'
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
    # Settings section that owns this job's enabled/cron fields.
    section: str = ''
    # Non-empty when the source is switched on but still missing required values,
    # in which case the scheduler keeps it parked.
    missing_fields: list[str] = Field(default_factory=list)


class JobListResponse(ApiSchema):
    items: list[JobSummary]
    total: int


class JobRequestCreate(ApiSchema):
    target: JobRequestTarget


class JobRequest(ApiSchema):
    id: int
    target: JobRequestTarget
    kind: str = 'trigger_job'
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


class Hanime1SeedDetail(ApiSchema):
    video_id: str
    title: str
    label: str
    added_from_video_id: str
    video_count: int
    created_at: str | None = None
    updated_at: str | None = None
    last_scanned_at: str | None = None
    last_scan_error: str = ''
    watch_url: str


class Hanime1SeedListResponse(ApiSchema):
    items: list[Hanime1SeedDetail]
    total: int


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


class Live2DVector2(ApiSchema):
    x: float
    y: float

    @field_validator('x', 'y')
    @classmethod
    def validate_finite(cls, value: float) -> float:
        if not math.isfinite(value):
            msg = 'coordinate must be finite'
            raise ValueError(msg)
        return value


class Live2DViewOverrideUpsert(ApiSchema):
    position: Live2DVector2
    scale: float = Field(gt=0)
    background_position: Live2DVector2 | None = None
    background_scale: float | None = Field(default=None, gt=0)

    @field_validator('scale', 'background_scale')
    @classmethod
    def validate_scale(cls, value: float | None) -> float | None:
        if value is not None and not math.isfinite(value):
            msg = 'scale must be finite'
            raise ValueError(msg)
        return value


class Live2DViewOverrideValue(ApiSchema):
    position: Live2DVector2
    scale: float
    background_position: Live2DVector2 | None = None
    background_scale: float | None = None
    created_at: str
    updated_at: str


class Live2DViewOverride(Live2DViewOverrideValue):
    source: Literal['bd2', 'nikke']
    content_id: int
    model_id: str
    profile: str


class AzurLaneAsset(ApiSchema):
    kind: str
    path: str
    url: str
    source_url: str = ''
    normalized_url: str = ''
    downloaded_url: str = ''
    fallback_url: str = ''
    source_urls: dict[str, Any] = Field(default_factory=dict)
    original_filename: str = ''
    content_type: str = ''
    size: int = 0
    sha256: str = ''
    status: str = ''
    available: bool = False
    failed_count: int = 0
    error: str = ''
    last_attempt_at: str | None = None
    next_retry_at: str | None = None
    last_seen_at: str | None = None
    model_id: str = ''
    model_type: str = ''
    character_key: str = ''
    costume_key: str = ''
    field: str = ''
    catalog_source: str = ''
    context_hashes: list[str] = Field(default_factory=list)
    contexts: list[dict[str, Any]] = Field(default_factory=list)


class AzurLaneCharacterSummary(ApiSchema):
    schema_version: int = 0
    source: str = 'azurlane'
    character_key: str
    source_id: int | None = None
    title: str
    display_name: str
    name_zh: str = ''
    name_en: str = ''
    directory_name: str = ''
    source_metadata: dict[str, Any] = Field(default_factory=dict)
    fetched_at: str | None = None
    completed_at: str | None = None
    model_counts: dict[str, int] = Field(default_factory=dict)
    asset_counts: dict[str, int] = Field(default_factory=dict)
    source_counts: dict[str, int] = Field(default_factory=dict)
    tags: dict[str, str] = Field(default_factory=dict)
    representative_asset: AzurLaneAsset | None = None
    model_count: int = 0


class AzurLaneCharacterListResponse(ApiSchema):
    items: list[AzurLaneCharacterSummary]
    total: int
    limit: int | None = None
    offset: int = 0


class AzurLaneSidebarCharacter(ApiSchema):
    character_key: str
    title: str
    display_name: str
    name_zh: str = ''
    name_en: str = ''
    representative_asset: AzurLaneAsset | None = None
    model_count: int = 0
    model_counts: dict[str, int] = Field(default_factory=dict)
    source_counts: dict[str, int] = Field(default_factory=dict)
    fetched_at: str | None = None
    completed_at: str | None = None


class AzurLaneSidebarCharacterListResponse(ApiSchema):
    items: list[AzurLaneSidebarCharacter]
    total: int


class AzurLaneCostume(ApiSchema):
    key: str = ''
    id: int | None = None
    name_zh: str = ''
    name_en: str = ''


class AzurLaneModel(ApiSchema):
    model_id: str = ''
    type: str = ''
    source: str = ''
    character_key: str = ''
    costume: AzurLaneCostume = Field(default_factory=AzurLaneCostume)
    source_urls: dict[str, Any] = Field(default_factory=dict)
    availability: dict[str, Any] = Field(default_factory=dict)
    source_metadata: dict[str, Any] = Field(default_factory=dict)
    fetched_at: str | None = None
    completed_at: str | None = None
    asset_counts: dict[str, int] = Field(default_factory=dict)
    files: dict[str, Any] = Field(default_factory=dict)
    assets: list[AzurLaneAsset] = Field(default_factory=list)


class AzurLaneCharacterDetail(AzurLaneCharacterSummary):
    active: bool = True
    models: list[AzurLaneModel] = Field(default_factory=list)
    live2d_models: list[AzurLaneModel] = Field(default_factory=list)
    spine_models: list[AzurLaneModel] = Field(default_factory=list)
    assets: list[AzurLaneAsset] = Field(default_factory=list)


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
    model_id: str = ''
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
    source: str = ''
    variant: str = ''
    supplement_reason: str = ''
    viewer_entry_id: str = ''
    viewer_stem: str = ''
    source_page_url: str = ''
    position: dict[str, Any] = Field(default_factory=dict)
    bg_position: dict[str, Any] = Field(default_factory=dict)
    view_overrides: dict[str, Live2DViewOverrideValue] = Field(default_factory=dict)
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


class NikkeLayerMetadataCapture(ApiSchema):
    status: str = ''
    captured_at: str | None = None
    attempted_at: str | None = None
    capture_hash: str = ''
    fingerprint: str = ''
    reason: str = ''
    error_class: str = ''
    retryable: bool | None = None
    warnings: list[str] = Field(default_factory=list)


class NikkeRuntimeAnimation(ApiSchema):
    animation: str = ''
    enabled: bool = True
    loop: bool = True
    match_method: str = ''
    match_confidence: Literal['high', 'medium', 'low'] | None = None
    duration_ms: int | None = Field(default=None, ge=0)


class NikkeRuntimeAnimations(ApiSchema):
    idle: NikkeRuntimeAnimation | None = None
    click: NikkeRuntimeAnimation | None = None


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
    model_id: str = ''
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
    layer_order: int | None = None
    source_z_index: int | None = None
    source_layer_index: int | None = None
    is_primary: bool | None = None
    layer_match_method: str = ''
    layer_match_confidence: Literal['high', 'medium', 'low'] | None = None
    runtime_animations: NikkeRuntimeAnimations | None = None
    runtime_animation_match_method: str = ''
    runtime_animation_match_confidence: Literal['high', 'medium', 'low'] | None = None
    position: dict[str, Any] = Field(default_factory=dict)
    bg_position: dict[str, Any] = Field(default_factory=dict)
    view_overrides: dict[str, Live2DViewOverrideValue] = Field(default_factory=dict)
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
    layer_metadata_required: bool = False
    layer_metadata_status: Literal['complete', 'incomplete', 'error', 'missing'] = 'missing'
    layer_metadata_issues: list[dict[str, Any]] = Field(default_factory=list)
    layer_metadata_capture: NikkeLayerMetadataCapture = Field(default_factory=NikkeLayerMetadataCapture)
    runtime_animation_metadata_required: bool = False
    runtime_animation_metadata_status: Literal['complete', 'incomplete', 'error', 'missing'] = 'missing'
    runtime_animation_metadata_issues: list[dict[str, Any]] = Field(default_factory=list)
    voice_lines: list[NikkeVoiceLine] = Field(default_factory=list)
    rows: list[dict[str, Any]] = Field(default_factory=list)


class NikkeCharacterDetail(NikkeCharacterSummary):
    tj_list: dict[str, Any] | None = None
    base_info: dict[str, Any] = Field(default_factory=dict)
    skins: list[NikkeSkin] = Field(default_factory=list)
    live2d_models: list[NikkeLive2DModel] = Field(default_factory=list)
    layer_metadata_required: bool = False
    layer_metadata_status: Literal['complete', 'incomplete', 'error', 'missing'] = 'missing'
    layer_metadata_issues: list[dict[str, Any]] = Field(default_factory=list)
    layer_metadata_capture: NikkeLayerMetadataCapture = Field(default_factory=NikkeLayerMetadataCapture)
    runtime_animation_metadata_required: bool = False
    runtime_animation_metadata_status: Literal['complete', 'incomplete', 'error', 'missing'] = 'missing'
    runtime_animation_metadata_issues: list[dict[str, Any]] = Field(default_factory=list)
    assets: list[NikkeAsset] = Field(default_factory=list)


class JobRequestListResponse(ApiSchema):
    items: list[JobRequest]
    total: int


class ArchiveSourceStat(ApiSchema):
    source: str
    name: str
    total: int
    latest_at: str | None = None


class ArchiveSourceListResponse(ApiSchema):
    items: list[ArchiveSourceStat]
    total: int


class ArchiveItem(ApiSchema):
    source: str
    id: str
    title: str
    subtitle: str = ''
    created_at: str | None = None
    url: str | None = None
    fields: dict[str, Any] = Field(default_factory=dict)


class ArchiveListResponse(ApiSchema):
    items: list[ArchiveItem]
    total: int
    limit: int
    offset: int


class SettingsSection(ApiSchema):
    section: str
    value: dict[str, Any]
    # Field names that must be filled in before this section's job can run.
    missing_fields: list[str] = Field(default_factory=list)


class SettingsListResponse(ApiSchema):
    items: list[SettingsSection]
    total: int

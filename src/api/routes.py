from __future__ import annotations

import hashlib
import json
import re
from datetime import UTC, date, datetime
from typing import Annotated, Any

from fastapi import APIRouter, Depends, Path, Query, Request, Response, status

from .constants import (
    API_V2_PREFIX,
    TAG_ARCHIVE,
    TAG_AZURLANE,
    TAG_BD2,
    TAG_HANIME1,
    TAG_JOBS,
    TAG_NIKKE,
    TAG_SETTINGS,
)
from .dependencies import get_api_service, require_api_token
from .schemas import (
    ArchiveListResponse,
    ArchiveSourceListResponse,
    AzurLaneCharacterDetail,
    AzurLaneCharacterListResponse,
    AzurLaneProxyTestRequest,
    AzurLaneProxyTestResult,
    AzurLaneSidebarCharacter,
    AzurLaneSidebarCharacterListResponse,
    AzurLaneSkinUpdatesResponse,
    BD2CharacterDetail,
    BD2CharacterListResponse,
    BD2SidebarCharacter,
    BD2SidebarCharacterListResponse,
    CookieCloudTestRequest,
    CookieCloudTestResult,
    Hanime1ListResponse,
    Hanime1Seed,
    Hanime1SeedCreate,
    Hanime1SeedListResponse,
    JobListResponse,
    JobRequest,
    JobRequestCreate,
    JobRequestListResponse,
    JobRequestStatus,
    Live2DViewOverride,
    Live2DViewOverrideUpsert,
    NikkeCharacterDetail,
    NikkeCharacterListResponse,
    NikkeSidebarCharacter,
    NikkeSidebarCharacterListResponse,
    RedNoteProxyTestRequest,
    RedNoteProxyTestResult,
    SettingsListResponse,
    SettingsSection,
    TelegramNotificationTestResponse,
)

router = APIRouter(prefix=API_V2_PREFIX, dependencies=[Depends(require_api_token)])
ApiServiceDep = Annotated[Any, Depends(get_api_service)]
AzurLaneCharacterKeyPath = Annotated[str, Path(min_length=1, max_length=200)]
AzurLaneSearchQuery = Annotated[str | None, Query(alias='q', min_length=1)]
AzurLaneLimitQuery = Annotated[int, Query(ge=1, le=500)]
AzurLaneOffsetQuery = Annotated[int, Query(ge=0)]
BD2SearchQuery = Annotated[str | None, Query(alias='q', min_length=1)]
BD2LimitQuery = Annotated[int, Query(ge=1, le=500)]
BD2OffsetQuery = Annotated[int, Query(ge=0)]
NikkeSearchQuery = Annotated[str | None, Query(alias='q', min_length=1)]
NikkeLimitQuery = Annotated[int, Query(ge=1, le=500)]
NikkeOffsetQuery = Annotated[int, Query(ge=0)]
Live2DModelIdPath = Annotated[str, Path(min_length=1, max_length=160, pattern=r'^[A-Za-z0-9_.:-]+$')]
Live2DProfilePath = Annotated[str, Path(min_length=1, max_length=64, pattern=r'^[A-Za-z0-9_-]+$')]
SettingsSectionPath = Annotated[str, Path(min_length=1, max_length=64, pattern=r'^[a-z0-9_]+(\.[a-z0-9_]+)*$')]
ArchiveSearchQuery = Annotated[str | None, Query(alias='q', min_length=1, max_length=200)]
_DATE_PART_COUNT = 3
_AZURLANE_SIDEBAR_CACHE_CONTROL = 'public, max-age=300'
_BD2_SIDEBAR_CACHE_CONTROL = 'public, max-age=300'
_NIKKE_SIDEBAR_CACHE_CONTROL = 'public, max-age=300'
_SIDEBAR_RESPONSES = {
    304: {'description': 'Not modified'},
}


def _matches_azurlane_query(character: dict[str, Any], query: str) -> bool:
    normalized = query.casefold()
    haystack: list[str] = [
        str(character.get('character_key') or ''),
        str(character.get('source_id') or ''),
        str(character.get('title') or ''),
        str(character.get('display_name') or ''),
        str(character.get('name_zh') or ''),
        str(character.get('name_en') or ''),
        str(character.get('directory_name') or ''),
    ]
    tags = character.get('tags')
    if isinstance(tags, dict):
        haystack.extend(str(value) for value in tags.values())
    source_metadata = character.get('source_metadata')
    if isinstance(source_metadata, dict):
        haystack.extend(json.dumps(value, ensure_ascii=False, sort_keys=True) for value in source_metadata.values())
    return normalized in ' '.join(haystack).casefold()


def _azurlane_sidebar_character_payload(character: dict[str, Any]) -> dict[str, Any]:
    return {
        'character_key': character.get('character_key') or '',
        'title': character.get('title') or '',
        'display_name': character.get('display_name') or character.get('title') or '',
        'name_zh': character.get('name_zh') or '',
        'name_en': character.get('name_en') or '',
        'representative_asset': character.get('representative_asset') if isinstance(character.get('representative_asset'), dict) else None,
        'icon': character.get('icon') if isinstance(character.get('icon'), dict) else None,
        'source_metadata': character.get('source_metadata') if isinstance(character.get('source_metadata'), dict) else {},
        'model_count': character.get('model_count') or 0,
        'model_counts': character.get('model_counts') if isinstance(character.get('model_counts'), dict) else {},
        'source_counts': character.get('source_counts') if isinstance(character.get('source_counts'), dict) else {},
        'fetched_at': character.get('fetched_at'),
        'completed_at': character.get('completed_at'),
    }


def _matches_bd2_query(character: dict[str, Any], query: str) -> bool:
    normalized = query.casefold()
    haystack: list[str] = [str(character.get('content_id') or ''), str(character.get('title') or '')]
    tags = character.get('tags')
    if isinstance(tags, dict):
        haystack.extend(str(value) for value in tags.values())
    profile = character.get('profile')
    if isinstance(profile, list):
        haystack.extend(str(item.get('value') or '') for item in profile if isinstance(item, dict))
    search_terms = character.get('search_terms')
    if isinstance(search_terms, list):
        haystack.extend(str(term) for term in search_terms)
    costumes = character.get('costumes')
    if isinstance(costumes, list):
        for costume in costumes:
            if isinstance(costume, dict):
                haystack.extend((str(costume.get('title') or ''), str(costume.get('category') or '')))
    return normalized in ' '.join(haystack).casefold()


def _bd2_sidebar_icon_payload(character: dict[str, Any]) -> dict[str, Any]:
    icon = character.get('icon')
    if not isinstance(icon, dict):
        icon = {}
    return {
        'url': str(icon.get('url') or ''),
        'available': bool(icon.get('available')),
        'sha256': str(icon.get('sha256') or ''),
        'content_type': str(icon.get('content_type') or ''),
    }


def _bd2_sidebar_character_payload(character: dict[str, Any]) -> dict[str, Any]:
    return {
        'content_id': character.get('content_id'),
        'title': character.get('title') or '',
        'icon': _bd2_sidebar_icon_payload(character),
        'updated_at': character.get('updated_at'),
        'fetched_at': character.get('fetched_at'),
    }


def _matches_nikke_query(character: dict[str, Any], query: str) -> bool:
    normalized = query.casefold()
    haystack: list[str] = [str(character.get('content_id') or ''), str(character.get('title') or '')]
    tags = character.get('tags')
    if isinstance(tags, dict):
        haystack.extend(str(value) for value in tags.values())
    profile = character.get('profile')
    if isinstance(profile, list):
        haystack.extend(str(item.get('value') or '') for item in profile if isinstance(item, dict))
    return normalized in ' '.join(haystack).casefold()


def _nikke_profile_value(character: dict[str, Any], key: str) -> str | None:
    profile = character.get('profile')
    if not isinstance(profile, list):
        return None
    for item in profile:
        if not isinstance(item, dict) or item.get('key') != key:
            continue
        value = str(item.get('value') or '').strip()
        return value or None
    return None


def _nikke_sidebar_icon_payload(character: dict[str, Any]) -> dict[str, Any]:
    icon = character.get('icon')
    if not isinstance(icon, dict):
        icon = {}
    return {
        'url': str(icon.get('url') or ''),
        'available': bool(icon.get('available')),
        'sha256': str(icon.get('sha256') or ''),
        'content_type': str(icon.get('content_type') or ''),
    }


def _nikke_sidebar_character_payload(character: dict[str, Any]) -> dict[str, Any]:
    return {
        'content_id': character.get('content_id'),
        'title': character.get('title') or '',
        'icon': _nikke_sidebar_icon_payload(character),
        'implemented_at': _nikke_profile_value(character, 'implemented_at'),
        'updated_at': character.get('updated_at'),
        'fetched_at': character.get('fetched_at'),
    }


def _sort_timestamp(value: Any) -> float:
    result = 0.0
    if isinstance(value, bool) or value is None:
        return result
    if isinstance(value, (int, float)):
        return float(value)
    if not isinstance(value, str):
        return result

    text = value.strip()
    if not text:
        return result
    if text.isdigit():
        result = float(text)
    else:
        normalized = text.replace('Z', '+00:00')
        try:
            parsed = datetime.fromisoformat(normalized)
            result = parsed.replace(tzinfo=UTC).timestamp() if parsed.tzinfo is None else parsed.timestamp()
        except ValueError:
            result = 0.0
    return result


def _sort_date(value: Any) -> int:
    if not isinstance(value, str):
        return 0
    parts = [int(part) for part in re.findall(r'\d+', value)]
    if len(parts) < _DATE_PART_COUNT:
        return 0
    year, month, day = parts[:3]
    try:
        return date(year, month, day).toordinal()
    except ValueError:
        return 0


def _nikke_sidebar_sort_key(item: NikkeSidebarCharacter) -> tuple[int, float, float, int]:
    return (
        _sort_date(item.implemented_at),
        _sort_timestamp(item.updated_at),
        _sort_timestamp(item.fetched_at),
        item.content_id,
    )


def _bd2_sidebar_sort_key(item: BD2SidebarCharacter) -> tuple[float, float, int]:
    return (
        _sort_timestamp(item.updated_at),
        _sort_timestamp(item.fetched_at),
        item.content_id,
    )


def _azurlane_sidebar_sort_key(item: AzurLaneSidebarCharacter) -> tuple[float, float, str]:
    return (
        _sort_timestamp(item.completed_at),
        _sort_timestamp(item.fetched_at),
        item.character_key,
    )


def _json_etag(payload: Any) -> str:
    body = json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()
    return f'"{hashlib.sha256(body).hexdigest()}"'


def _sidebar_response_etag(
    payload: AzurLaneSidebarCharacterListResponse | BD2SidebarCharacterListResponse | NikkeSidebarCharacterListResponse,
) -> str:
    return _json_etag(payload.model_dump(mode='json'))


def _etag_matches(header_value: str | None, etag: str) -> bool:
    if not header_value:
        return False
    candidates = [value.strip() for value in header_value.split(',')]
    return '*' in candidates or etag in candidates


@router.get(
    '/jobs',
    operation_id='listJobs',
    response_model=JobListResponse,
    tags=[TAG_JOBS],
)
def list_jobs(service: ApiServiceDep) -> JobListResponse:
    items = [service.model_job(job) for job in service.list_jobs()]
    return JobListResponse(items=items, total=len(items))


@router.post(
    '/job-requests',
    operation_id='createJobRequest',
    response_model=JobRequest,
    status_code=status.HTTP_202_ACCEPTED,
    tags=[TAG_JOBS],
)
def create_job_request(
    payload: JobRequestCreate,
    service: ApiServiceDep,
) -> JobRequest:
    return service.model_job_request(service.create_job_request(payload.target.value))


@router.get(
    '/job-requests/{request_id}',
    operation_id='getJobRequest',
    response_model=JobRequest,
    tags=[TAG_JOBS],
)
def get_job_request(
    request_id: Annotated[int, Path(gt=0)],
    service: ApiServiceDep,
) -> JobRequest:
    return service.model_job_request(service.get_job_request(request_id))


@router.get(
    '/job-requests',
    operation_id='listJobRequests',
    response_model=JobRequestListResponse,
    tags=[TAG_JOBS],
)
def list_job_requests(
    service: ApiServiceDep,
    status_filter: Annotated[JobRequestStatus | None, Query(alias='status')] = None,
    limit: Annotated[int, Query(ge=1, le=200)] = 50,
) -> JobRequestListResponse:
    items = [
        service.model_job_request(request)
        for request in service.list_job_requests(
            status=status_filter.value if status_filter is not None else None,
            limit=limit,
        )
    ]
    return JobRequestListResponse(items=items, total=len(items))


@router.get(
    '/settings',
    operation_id='listSettings',
    response_model=SettingsListResponse,
    tags=[TAG_SETTINGS],
)
def list_settings(service: ApiServiceDep) -> SettingsListResponse:
    items = [service.model_settings_section(section) for section in service.list_settings()]
    return SettingsListResponse(items=items, total=len(items))


@router.get(
    '/settings/{section}',
    operation_id='getSettingsSection',
    response_model=SettingsSection,
    tags=[TAG_SETTINGS],
)
def get_settings_section(
    section: SettingsSectionPath,
    service: ApiServiceDep,
) -> SettingsSection:
    return service.model_settings_section(service.get_settings_section(section))


@router.put(
    '/settings/{section}',
    operation_id='updateSettingsSection',
    response_model=SettingsSection,
    tags=[TAG_SETTINGS],
)
def update_settings_section(
    section: SettingsSectionPath,
    payload: dict[str, Any],
    service: ApiServiceDep,
) -> SettingsSection:
    return service.model_settings_section(service.update_settings_section(section, payload))


@router.post(
    '/notifications/telegram/test',
    operation_id='testTelegramNotification',
    response_model=TelegramNotificationTestResponse,
    tags=[TAG_SETTINGS],
)
async def test_telegram_notification(service: ApiServiceDep) -> TelegramNotificationTestResponse:
    """Send a test message with the stored bot credentials."""
    return service.model_telegram_notification_test(await service.test_telegram_notification())


@router.post(
    '/azurlane/proxy/test',
    operation_id='testAzurLaneProxy',
    response_model=AzurLaneProxyTestResult,
    tags=[TAG_SETTINGS],
)
def test_azurlane_proxy(payload: AzurLaneProxyTestRequest, service: ApiServiceDep) -> AzurLaneProxyTestResult:
    """Check that the l2d.su origin is reachable through a proxy, without saving it."""
    return AzurLaneProxyTestResult.model_validate(service.test_azurlane_proxy(payload.model_dump()))


@router.post(
    '/rednote/proxy/test',
    operation_id='testRedNoteProxy',
    response_model=RedNoteProxyTestResult,
    tags=[TAG_SETTINGS],
)
def test_rednote_proxy(payload: RedNoteProxyTestRequest, service: ApiServiceDep) -> RedNoteProxyTestResult:
    """Check that xiaohongshu.com is reachable through a proxy, without saving it."""
    return RedNoteProxyTestResult.model_validate(service.test_rednote_proxy(payload.model_dump()))


@router.post(
    '/cookiecloud/test',
    operation_id='testCookieCloud',
    response_model=CookieCloudTestResult,
    tags=[TAG_SETTINGS],
)
def test_cookiecloud(payload: CookieCloudTestRequest, service: ApiServiceDep) -> CookieCloudTestResult:
    """Check a CookieCloud configuration without saving it."""
    return CookieCloudTestResult.model_validate(service.test_cookiecloud(payload.model_dump()))


@router.get(
    '/archive/sources',
    operation_id='listArchiveSources',
    response_model=ArchiveSourceListResponse,
    tags=[TAG_ARCHIVE],
)
def list_archive_sources(service: ApiServiceDep) -> ArchiveSourceListResponse:
    items = [service.model_archive_source(source) for source in service.list_archive_sources()]
    return ArchiveSourceListResponse(items=items, total=len(items))


@router.get(
    '/archive/items',
    operation_id='listArchiveItems',
    response_model=ArchiveListResponse,
    tags=[TAG_ARCHIVE],
)
def list_archive_items(
    service: ApiServiceDep,
    source: Annotated[str, Query(min_length=1, max_length=40)],
    query: ArchiveSearchQuery = None,
    limit: Annotated[int, Query(ge=1, le=200)] = 50,
    offset: Annotated[int, Query(ge=0)] = 0,
) -> ArchiveListResponse:
    return service.model_archive_list(
        service.list_archive_items(source=source, query=query or '', limit=limit, offset=offset),
    )


@router.get(
    '/hanime1/videos',
    operation_id='listHanime1Videos',
    response_model=Hanime1ListResponse,
    tags=[TAG_HANIME1],
)
def list_hanime1_videos(service: ApiServiceDep) -> Hanime1ListResponse:
    items = [service.model_hanime1_video(video) for video in service.list_hanime1_videos()]
    return Hanime1ListResponse(items=items, total=len(items))


@router.post(
    '/hanime1/seeds',
    operation_id='createHanime1Seed',
    response_model=Hanime1Seed,
    status_code=status.HTTP_201_CREATED,
    tags=[TAG_HANIME1],
)
def create_hanime1_seed(
    payload: Hanime1SeedCreate,
    service: ApiServiceDep,
) -> Hanime1Seed:
    return service.model_hanime1_seed(service.add_hanime1_seed(payload.seed))


@router.get(
    '/hanime1/seeds',
    operation_id='listHanime1Seeds',
    response_model=Hanime1SeedListResponse,
    tags=[TAG_HANIME1],
)
def list_hanime1_seeds(service: ApiServiceDep) -> Hanime1SeedListResponse:
    items = [service.model_hanime1_seed_detail(seed) for seed in service.list_hanime1_seeds()]
    return Hanime1SeedListResponse(items=items, total=len(items))


@router.delete(
    '/hanime1/seeds/{canonical_video_id}',
    operation_id='deleteHanime1Seed',
    status_code=status.HTTP_204_NO_CONTENT,
    tags=[TAG_HANIME1],
)
def delete_hanime1_seed(
    canonical_video_id: Annotated[str, Path(min_length=1, max_length=32, pattern=r'^[0-9]+$')],
    service: ApiServiceDep,
) -> Response:
    service.delete_hanime1_seed(canonical_video_id)
    return Response(status_code=status.HTTP_204_NO_CONTENT)


@router.get(
    '/azurlane/characters',
    operation_id='listAzurLaneCharacters',
    response_model=AzurLaneCharacterListResponse,
    tags=[TAG_AZURLANE],
)
def list_azurlane_characters(
    service: ApiServiceDep,
    query: AzurLaneSearchQuery = None,
    limit: AzurLaneLimitQuery = 200,
    offset: AzurLaneOffsetQuery = 0,
) -> AzurLaneCharacterListResponse:
    characters = service.list_azurlane_characters()
    if query:
        characters = [character for character in characters if _matches_azurlane_query(character, query)]
    total = len(characters)
    page = characters[offset : offset + limit]
    items = [service.model_azurlane_character_summary(character) for character in page]
    return AzurLaneCharacterListResponse(items=items, total=total, limit=limit, offset=offset)


@router.get(
    '/azurlane/sidebar/characters',
    operation_id='listAzurLaneSidebarCharacters',
    response_model=AzurLaneSidebarCharacterListResponse,
    responses=_SIDEBAR_RESPONSES,
    tags=[TAG_AZURLANE],
)
def list_azurlane_sidebar_characters(
    request: Request,
    response: Response,
    service: ApiServiceDep,
    query: AzurLaneSearchQuery = None,
) -> AzurLaneSidebarCharacterListResponse | Response:
    characters = service.list_azurlane_characters()
    if query:
        characters = [character for character in characters if _matches_azurlane_query(character, query)]

    items = [AzurLaneSidebarCharacter.model_validate(_azurlane_sidebar_character_payload(character)) for character in characters]
    items = sorted(items, key=_azurlane_sidebar_sort_key, reverse=True)
    payload = AzurLaneSidebarCharacterListResponse(items=items, total=len(items))
    etag = _sidebar_response_etag(payload)
    headers = {'Cache-Control': _AZURLANE_SIDEBAR_CACHE_CONTROL, 'ETag': etag}
    if _etag_matches(request.headers.get('if-none-match'), etag):
        return Response(status_code=status.HTTP_304_NOT_MODIFIED, headers=headers)

    response.headers.update(headers)
    return payload


@router.get(
    '/azurlane/skin-updates',
    operation_id='getAzurLaneSkinUpdates',
    response_model=AzurLaneSkinUpdatesResponse,
    responses=_SIDEBAR_RESPONSES,
    tags=[TAG_AZURLANE],
)
def get_azurlane_skin_updates(
    request: Request,
    response: Response,
    service: ApiServiceDep,
) -> AzurLaneSkinUpdatesResponse | Response:
    payload = AzurLaneSkinUpdatesResponse.model_validate(service.get_azurlane_skin_updates())
    etag = _json_etag(payload.model_dump(mode='json'))
    headers = {'Cache-Control': _AZURLANE_SIDEBAR_CACHE_CONTROL, 'ETag': etag}
    if _etag_matches(request.headers.get('if-none-match'), etag):
        return Response(status_code=status.HTTP_304_NOT_MODIFIED, headers=headers)

    response.headers.update(headers)
    return payload


@router.get(
    '/azurlane/characters/{character_key}',
    operation_id='getAzurLaneCharacter',
    response_model=AzurLaneCharacterDetail,
    tags=[TAG_AZURLANE],
)
def get_azurlane_character(
    character_key: AzurLaneCharacterKeyPath,
    service: ApiServiceDep,
) -> AzurLaneCharacterDetail:
    return service.model_azurlane_character_detail(service.get_azurlane_character(character_key))


@router.get(
    '/azurlane/characters/{character_key}/ship-detail',
    operation_id='getAzurLaneShipDetail',
    tags=[TAG_AZURLANE],
)
def get_azurlane_ship_detail(
    character_key: AzurLaneCharacterKeyPath,
    service: ApiServiceDep,
) -> dict[str, object]:
    return service.get_azurlane_ship_detail(character_key)


@router.get(
    '/bd2/characters',
    operation_id='listBD2Characters',
    response_model=BD2CharacterListResponse,
    tags=[TAG_BD2],
)
def list_bd2_characters(
    service: ApiServiceDep,
    query: BD2SearchQuery = None,
    limit: BD2LimitQuery = 200,
    offset: BD2OffsetQuery = 0,
) -> BD2CharacterListResponse:
    characters = service.list_bd2_characters()
    if query:
        characters = [character for character in characters if _matches_bd2_query(character, query)]
    total = len(characters)
    page = characters[offset : offset + limit]
    items = [service.model_bd2_character_summary(character) for character in page]
    return BD2CharacterListResponse(items=items, total=total, limit=limit, offset=offset)


@router.get(
    '/bd2/sidebar/characters',
    operation_id='listBD2SidebarCharacters',
    response_model=BD2SidebarCharacterListResponse,
    responses=_SIDEBAR_RESPONSES,
    tags=[TAG_BD2],
)
def list_bd2_sidebar_characters(
    request: Request,
    response: Response,
    service: ApiServiceDep,
    query: BD2SearchQuery = None,
) -> BD2SidebarCharacterListResponse | Response:
    characters = service.list_bd2_characters()
    if query:
        characters = [character for character in characters if _matches_bd2_query(character, query)]

    items = [BD2SidebarCharacter.model_validate(_bd2_sidebar_character_payload(character)) for character in characters]
    items = sorted(items, key=_bd2_sidebar_sort_key, reverse=True)
    payload = BD2SidebarCharacterListResponse(items=items, total=len(items))
    etag = _sidebar_response_etag(payload)
    headers = {'Cache-Control': _BD2_SIDEBAR_CACHE_CONTROL, 'ETag': etag}
    if _etag_matches(request.headers.get('if-none-match'), etag):
        return Response(status_code=status.HTTP_304_NOT_MODIFIED, headers=headers)

    response.headers.update(headers)
    return payload


@router.get(
    '/bd2/characters/{content_id}',
    operation_id='getBD2Character',
    response_model=BD2CharacterDetail,
    tags=[TAG_BD2],
)
def get_bd2_character(
    content_id: Annotated[int, Path(gt=0)],
    service: ApiServiceDep,
) -> BD2CharacterDetail:
    return service.model_bd2_character_detail(service.get_bd2_character(content_id))


@router.get(
    '/bd2/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}',
    operation_id='getBD2Live2DViewOverride',
    response_model=Live2DViewOverride,
    tags=[TAG_BD2],
)
def get_bd2_live2d_view_override(
    content_id: Annotated[int, Path(gt=0)],
    model_id: Live2DModelIdPath,
    profile: Live2DProfilePath,
    service: ApiServiceDep,
) -> Live2DViewOverride:
    return service.model_live2d_view_override(
        service.get_live2d_view_override(source='bd2', content_id=content_id, model_id=model_id, profile=profile),
    )


@router.put(
    '/bd2/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}',
    operation_id='putBD2Live2DViewOverride',
    response_model=Live2DViewOverride,
    tags=[TAG_BD2],
)
def put_bd2_live2d_view_override(
    content_id: Annotated[int, Path(gt=0)],
    model_id: Live2DModelIdPath,
    profile: Live2DProfilePath,
    payload: Live2DViewOverrideUpsert,
    service: ApiServiceDep,
) -> Live2DViewOverride:
    return service.model_live2d_view_override(
        service.upsert_live2d_view_override(
            source='bd2',
            content_id=content_id,
            model_id=model_id,
            profile=profile,
            position=payload.position.model_dump(),
            scale=payload.scale,
            background_position=payload.background_position.model_dump() if payload.background_position is not None else None,
            background_scale=payload.background_scale,
        ),
    )


@router.delete(
    '/bd2/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}',
    operation_id='deleteBD2Live2DViewOverride',
    status_code=status.HTTP_204_NO_CONTENT,
    tags=[TAG_BD2],
)
def delete_bd2_live2d_view_override(
    content_id: Annotated[int, Path(gt=0)],
    model_id: Live2DModelIdPath,
    profile: Live2DProfilePath,
    service: ApiServiceDep,
) -> Response:
    service.delete_live2d_view_override(source='bd2', content_id=content_id, model_id=model_id, profile=profile)
    return Response(status_code=status.HTTP_204_NO_CONTENT)


@router.get(
    '/nikke/characters',
    operation_id='listNikkeCharacters',
    response_model=NikkeCharacterListResponse,
    tags=[TAG_NIKKE],
)
def list_nikke_characters(
    service: ApiServiceDep,
    query: NikkeSearchQuery = None,
    limit: NikkeLimitQuery = 200,
    offset: NikkeOffsetQuery = 0,
) -> NikkeCharacterListResponse:
    characters = service.list_nikke_characters()
    if query:
        characters = [character for character in characters if _matches_nikke_query(character, query)]
    total = len(characters)
    page = characters[offset : offset + limit]
    items = [service.model_nikke_character_summary(character) for character in page]
    return NikkeCharacterListResponse(items=items, total=total, limit=limit, offset=offset)


@router.get(
    '/nikke/sidebar/characters',
    operation_id='listNikkeSidebarCharacters',
    response_model=NikkeSidebarCharacterListResponse,
    responses=_SIDEBAR_RESPONSES,
    tags=[TAG_NIKKE],
)
def list_nikke_sidebar_characters(
    request: Request,
    response: Response,
    service: ApiServiceDep,
    query: NikkeSearchQuery = None,
) -> NikkeSidebarCharacterListResponse | Response:
    characters = service.list_nikke_characters()
    if query:
        characters = [character for character in characters if _matches_nikke_query(character, query)]

    items = [NikkeSidebarCharacter.model_validate(_nikke_sidebar_character_payload(character)) for character in characters]
    items = sorted(items, key=_nikke_sidebar_sort_key, reverse=True)
    payload = NikkeSidebarCharacterListResponse(items=items, total=len(items))
    etag = _sidebar_response_etag(payload)
    headers = {'Cache-Control': _NIKKE_SIDEBAR_CACHE_CONTROL, 'ETag': etag}
    if _etag_matches(request.headers.get('if-none-match'), etag):
        return Response(status_code=status.HTTP_304_NOT_MODIFIED, headers=headers)

    response.headers.update(headers)
    return payload


@router.get(
    '/nikke/characters/{content_id}',
    operation_id='getNikkeCharacter',
    response_model=NikkeCharacterDetail,
    tags=[TAG_NIKKE],
)
def get_nikke_character(
    content_id: Annotated[int, Path(gt=0)],
    service: ApiServiceDep,
) -> NikkeCharacterDetail:
    return service.model_nikke_character_detail(service.get_nikke_character(content_id))


@router.get(
    '/nikke/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}',
    operation_id='getNikkeLive2DViewOverride',
    response_model=Live2DViewOverride,
    tags=[TAG_NIKKE],
)
def get_nikke_live2d_view_override(
    content_id: Annotated[int, Path(gt=0)],
    model_id: Live2DModelIdPath,
    profile: Live2DProfilePath,
    service: ApiServiceDep,
) -> Live2DViewOverride:
    return service.model_live2d_view_override(
        service.get_live2d_view_override(source='nikke', content_id=content_id, model_id=model_id, profile=profile),
    )


@router.put(
    '/nikke/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}',
    operation_id='putNikkeLive2DViewOverride',
    response_model=Live2DViewOverride,
    tags=[TAG_NIKKE],
)
def put_nikke_live2d_view_override(
    content_id: Annotated[int, Path(gt=0)],
    model_id: Live2DModelIdPath,
    profile: Live2DProfilePath,
    payload: Live2DViewOverrideUpsert,
    service: ApiServiceDep,
) -> Live2DViewOverride:
    return service.model_live2d_view_override(
        service.upsert_live2d_view_override(
            source='nikke',
            content_id=content_id,
            model_id=model_id,
            profile=profile,
            position=payload.position.model_dump(),
            scale=payload.scale,
            background_position=payload.background_position.model_dump() if payload.background_position is not None else None,
            background_scale=payload.background_scale,
        ),
    )


@router.delete(
    '/nikke/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}',
    operation_id='deleteNikkeLive2DViewOverride',
    status_code=status.HTTP_204_NO_CONTENT,
    tags=[TAG_NIKKE],
)
def delete_nikke_live2d_view_override(
    content_id: Annotated[int, Path(gt=0)],
    model_id: Live2DModelIdPath,
    profile: Live2DProfilePath,
    service: ApiServiceDep,
) -> Response:
    service.delete_live2d_view_override(source='nikke', content_id=content_id, model_id=model_id, profile=profile)
    return Response(status_code=status.HTTP_204_NO_CONTENT)

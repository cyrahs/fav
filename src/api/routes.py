from __future__ import annotations

from typing import Annotated, Any

from fastapi import APIRouter, Depends, Path, Query, status
from fastapi.responses import FileResponse

from .constants import API_V2_PREFIX, TAG_HANIME1, TAG_JOBS, TAG_NIKKE
from .dependencies import get_api_service, require_api_token
from .schemas import (
    Hanime1ListResponse,
    Hanime1Seed,
    Hanime1SeedCreate,
    JobListResponse,
    JobRequest,
    JobRequestCreate,
    NikkeCharacterDetail,
    NikkeCharacterListResponse,
)

router = APIRouter(prefix=API_V2_PREFIX, dependencies=[Depends(require_api_token)])
ApiServiceDep = Annotated[Any, Depends(get_api_service)]
NikkeSearchQuery = Annotated[str | None, Query(alias='q', min_length=1)]
NikkeLimitQuery = Annotated[int, Query(ge=1, le=500)]
NikkeOffsetQuery = Annotated[int, Query(ge=0)]


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
    '/nikke/assets/{content_id}/{asset_path:path}',
    operation_id='getNikkeAsset',
    tags=[TAG_NIKKE],
)
def get_nikke_asset(
    content_id: Annotated[int, Path(gt=0)],
    asset_path: str,
    service: ApiServiceDep,
) -> FileResponse:
    asset = service.get_nikke_asset(content_id, asset_path)
    return FileResponse(asset.path, media_type=asset.content_type or None, headers=asset.headers)

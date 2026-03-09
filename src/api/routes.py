from __future__ import annotations

from typing import Annotated, Any

from fastapi import APIRouter, Depends, Path, Query, Response, status

from .constants import API_V2_PREFIX, TAG_HANIME1, TAG_JOBS, TAG_NOTIFICATIONS
from .dependencies import get_api_service, require_api_token
from .schemas import (
    Hanime1ListResponse,
    Hanime1Seed,
    Hanime1SeedCreate,
    Hanime1SeedListResponse,
    JobListResponse,
    JobRequest,
    JobRequestCreate,
    NotificationAckRequest,
    NotificationAckResponse,
    NotificationListResponse,
    NotificationListStatus,
)

router = APIRouter(prefix=API_V2_PREFIX, dependencies=[Depends(require_api_token)])
ApiServiceDep = Annotated[Any, Depends(get_api_service)]


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
    '/notifications',
    operation_id='listNotifications',
    response_model=NotificationListResponse,
    tags=[TAG_NOTIFICATIONS],
)
def list_notifications(
    service: ApiServiceDep,
    status_filter: Annotated[NotificationListStatus, Query(alias='status')] = NotificationListStatus.UNREAD,
    limit: Annotated[int, Query(ge=1, le=200)] = 50,
    after_id: Annotated[int | None, Query(ge=0)] = None,
) -> NotificationListResponse:
    items = [service.model_notification(item) for item in service.list_notifications(status_filter.value, limit, after_id)]
    return NotificationListResponse(items=items, total=len(items))


@router.post(
    '/notifications/ack',
    operation_id='ackNotifications',
    response_model=NotificationAckResponse,
    tags=[TAG_NOTIFICATIONS],
)
def ack_notifications(
    payload: NotificationAckRequest,
    service: ApiServiceDep,
) -> NotificationAckResponse:
    return NotificationAckResponse(updated=service.ack_notifications(payload.ids))


@router.get(
    '/hanime1/videos',
    operation_id='listHanime1Videos',
    response_model=Hanime1ListResponse,
    tags=[TAG_HANIME1],
)
def list_hanime1_videos(service: ApiServiceDep) -> Hanime1ListResponse:
    items = [service.model_hanime1_video(video) for video in service.list_hanime1_videos()]
    return Hanime1ListResponse(items=items, total=len(items))


@router.get(
    '/hanime1/seeds',
    operation_id='listHanime1Seeds',
    response_model=Hanime1SeedListResponse,
    tags=[TAG_HANIME1],
)
def list_hanime1_seeds(service: ApiServiceDep) -> Hanime1SeedListResponse:
    items = [service.model_hanime1_seed(seed) for seed in service.list_hanime1_seeds()]
    return Hanime1SeedListResponse(items=items, total=len(items))


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


@router.delete(
    '/hanime1/seeds/{video_id}',
    operation_id='deleteHanime1Seed',
    status_code=status.HTTP_204_NO_CONTENT,
    tags=[TAG_HANIME1],
)
def delete_hanime1_seed(
    video_id: Annotated[int, Path(gt=0)],
    service: ApiServiceDep,
) -> Response:
    service.delete_hanime1_seed(str(video_id))
    return Response(status_code=status.HTTP_204_NO_CONTENT)

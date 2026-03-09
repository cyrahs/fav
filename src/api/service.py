from __future__ import annotations

import secrets
from collections.abc import Callable
from datetime import UTC, datetime

from src.core import logger
from src.core.config import config as app_config
from src.service.jobs import ScheduledJob, build_jobs
from src.tool.control_queue import ControlRequest, create_control_request_sync, get_control_request_sync
from src.tool.runtime_config import Hanime1RuntimeConfigService

from .config import fetch_hanime1_videos_from_db
from .constants import AUTH_PREFIX, WWW_AUTHENTICATE_BEARER
from .errors import ApiError
from .helpers import serialize_control_request, serialize_job, serialize_seed, utc_now_iso_z
from .schemas import Hanime1Seed, Hanime1Video, HealthResponse, JobRequest, JobSummary

log = logger.get('fav-api')

type DatetimeProvider = Callable[[], datetime]
type Hanime1VideoFetcher = Callable[[str], list[dict[str, str | None]]]
type JobProvider = Callable[[], list[ScheduledJob]]
type ControlRequestCreator = Callable[[str, str], ControlRequest]
type ControlRequestGetter = Callable[[int], ControlRequest | None]


class FavApiService:
    def __init__(  # noqa: PLR0913
        self,
        *,
        dsn: str,
        token: str,
        hanime1_video_fetcher: Hanime1VideoFetcher | None = None,
        now_provider: DatetimeProvider | None = None,
        job_provider: JobProvider | None = None,
        control_request_creator: ControlRequestCreator | None = None,
        control_request_getter: ControlRequestGetter | None = None,
        runtime_service: Hanime1RuntimeConfigService | None = None,
    ) -> None:
        self._dsn = dsn
        self._token = token
        self._hanime1_video_fetcher = hanime1_video_fetcher or (
            lambda dsn: fetch_hanime1_videos_from_db(dsn, host=app_config.web.hanime1.host)
        )
        self._now_provider = now_provider or (lambda: datetime.now(UTC))
        self._job_provider = job_provider or build_jobs
        self._control_request_creator = control_request_creator or (
            lambda kind, target: create_control_request_sync(self._dsn, kind=kind, target=target)
        )
        self._control_request_getter = control_request_getter or (lambda request_id: get_control_request_sync(self._dsn, request_id))
        self._runtime_service = runtime_service or Hanime1RuntimeConfigService(
            run_config=app_config.run_config,
            host=app_config.web.hanime1.host,
            user_lang=app_config.web.hanime1.user_lang,
            proxy=app_config.proxy or None,
        )

    def close(self) -> None:
        close = getattr(self._runtime_service, 'close', None)
        if callable(close):
            close()

    def authenticate(self, authorization: str | None) -> None:
        if not authorization:
            raise ApiError(
                status_code=401,
                code='missing_authorization',
                message='Authorization header is required.',
                headers={'WWW-Authenticate': WWW_AUTHENTICATE_BEARER},
            )
        if not authorization.startswith(AUTH_PREFIX):
            raise ApiError(
                status_code=401,
                code='invalid_authorization_scheme',
                message='Authorization scheme must be Bearer.',
                headers={'WWW-Authenticate': WWW_AUTHENTICATE_BEARER},
            )
        provided_token = authorization[len(AUTH_PREFIX) :].strip()
        if not provided_token:
            raise ApiError(
                status_code=401,
                code='missing_bearer_token',
                message='Bearer token is required.',
                headers={'WWW-Authenticate': WWW_AUTHENTICATE_BEARER},
            )
        if not secrets.compare_digest(provided_token, self._token):
            raise ApiError(
                status_code=403,
                code='invalid_token',
                message='Bearer token is invalid.',
            )

    def get_health(self) -> dict[str, str]:
        return {'status': 'ok', 'generated_at': utc_now_iso_z(self._now_provider)}

    def list_jobs(self) -> list[dict[str, str | bool]]:
        return [serialize_job(job) for job in self._job_provider()]

    def create_job_request(self, target: str) -> dict[str, object]:
        normalized_target = target.strip().lower()
        valid_targets = {job.key for job in self._job_provider()}
        valid_targets.add('all')
        if normalized_target not in valid_targets:
            raise ApiError(
                status_code=400,
                code='invalid_target',
                message='Unsupported job target.',
                details={'allowed_values': sorted(valid_targets)},
            )

        try:
            request = self._control_request_creator('trigger_job', normalized_target)
        except Exception:
            log.exception('Failed to create control request target=%s', normalized_target)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        return serialize_control_request(request)

    def get_job_request(self, request_id: int) -> dict[str, object]:
        request = self._control_request_getter(request_id)
        if request is None:
            raise ApiError(status_code=404, code='not_found', message='Job request not found.')
        return serialize_control_request(request)

    def list_hanime1_seeds(self) -> list[dict[str, str]]:
        return [serialize_seed(seed) for seed in self._runtime_service.list_seeds()]

    def list_hanime1_videos(self) -> list[dict[str, str | None]]:
        try:
            return self._hanime1_video_fetcher(self._dsn)
        except Exception:
            log.exception('Failed to list Hanime1 videos')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

    def add_hanime1_seed(self, seed: str) -> dict[str, str]:
        try:
            created_seed = self._runtime_service.add_seed(seed)
        except ValueError:
            raise ApiError(status_code=422, code='invalid_seed', message='Seed format is invalid.') from None
        except FileExistsError:
            raise ApiError(status_code=409, code='duplicate_seed', message='Hanime1 seed already exists.') from None
        except LookupError:
            raise ApiError(status_code=422, code='seed_resolve_failed', message='Unable to resolve Hanime1 seed.') from None
        except Exception:
            log.exception('Failed to add Hanime1 runtime seed')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        return serialize_seed(created_seed)

    def delete_hanime1_seed(self, video_id: str) -> None:
        try:
            removed_seed = self._runtime_service.delete_seed(video_id)
        except Exception:
            log.exception('Failed to delete Hanime1 runtime seed %s', video_id)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        if removed_seed is None:
            raise ApiError(status_code=404, code='not_found', message='Hanime1 seed not found.')

    @staticmethod
    def model_health(payload: dict[str, str]) -> HealthResponse:
        return HealthResponse.model_validate(payload)

    @staticmethod
    def model_job(payload: dict[str, str | bool]) -> JobSummary:
        return JobSummary.model_validate(payload)

    @staticmethod
    def model_job_request(payload: dict[str, object]) -> JobRequest:
        return JobRequest.model_validate(payload)

    @staticmethod
    def model_hanime1_seed(payload: dict[str, str]) -> Hanime1Seed:
        return Hanime1Seed.model_validate(payload)

    @staticmethod
    def model_hanime1_video(payload: dict[str, str | None]) -> Hanime1Video:
        return Hanime1Video.model_validate(payload)

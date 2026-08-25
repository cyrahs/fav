from __future__ import annotations

import asyncio
import json
import secrets
from collections.abc import Awaitable, Callable
from datetime import UTC, datetime
from time import monotonic
from typing import Any

from pydantic import BaseModel
from pydantic import ValidationError as PydanticValidationError

from src.core import logger, settings
from src.core.settings import SECTION_MODELS, UnknownSectionError
from src.service.jobs import ScheduledJob, build_jobs
from src.tool import cookiecloud as cookiecloud_tool
from src.tool.azurlane_l2d_sources import probe_l2d_su_origin
from src.tool.control_queue import (
    ControlRequest,
    create_control_request_sync,
    get_control_request_sync,
    list_control_requests_sync,
)
from src.tool.hanime1_author import Hanime1AuthorService
from src.tool.hanime1_series import Hanime1SeriesService
from src.tool.kemono_creator import KemonoCreatorResolver
from src.tool.runtime_config import Hanime1ParserIncompatibleError
from src.tool.telegram_bot import TelegramDeliveryError, TelegramDeliveryResult, TelegramNotConfiguredError, send_test_notification
from src.web.rednote_browser import probe_proxy as probe_rednote_proxy

from .archive import ARCHIVE_SOURCES, ArchiveLibrary, UnknownArchiveSourceError
from .azurlane import AzurLaneAssetNotFoundError, AzurLaneCharacterNotFoundError, AzurLaneLibrary
from .bd2 import BD2CharacterNotFoundError, BD2Library
from .config import fetch_hanime1_videos_from_db
from .constants import AUTH_PREFIX, WWW_AUTHENTICATE_BEARER
from .errors import ApiError
from .helpers import serialize_control_request, serialize_datetime, serialize_job, serialize_seed, utc_now_iso_z
from .live2d_overrides import (
    Live2DViewOverrideStore,
    PostgresLive2DViewOverrideStore,
    apply_live2d_view_overrides,
    assign_live2d_model_ids,
    iter_live2d_models,
    validate_live2d_profile,
    validate_live2d_source,
)
from .nikke import NikkeCharacterNotFoundError, NikkeLibrary
from .schemas import (
    ArchiveItem,
    ArchiveListResponse,
    ArchiveSourceStat,
    AzurLaneCharacterDetail,
    AzurLaneCharacterSummary,
    BD2CharacterDetail,
    BD2CharacterSummary,
    Hanime1Author,
    Hanime1AuthorDetail,
    Hanime1Seed,
    Hanime1SeedDetail,
    Hanime1Video,
    HealthResponse,
    JobRequest,
    JobSummary,
    KemonoCreatorResolved,
    Live2DViewOverride,
    NikkeCharacterDetail,
    NikkeCharacterSummary,
    ReadinessResponse,
    SettingsSection,
    TelegramNotificationTestResponse,
)
from .settings_masking import keep_secret, mask_section, unmask_section

log = logger.get('fav-api')

type DatetimeProvider = Callable[[], datetime]
type Hanime1VideoFetcher = Callable[[str], list[dict[str, str | None]]]
type JobProvider = Callable[[], list[ScheduledJob]]
type ControlRequestCreator = Callable[[str, str], ControlRequest]
type ControlRequestGetter = Callable[[int], ControlRequest | None]
type ControlRequestLister = Callable[[list[str] | None, int], list[ControlRequest]]
type SettingsSectionGetter = Callable[[str], BaseModel]
type SettingsSectionSaver = Callable[[str, dict[str, Any]], BaseModel]
type TelegramNotificationTester = Callable[[], Awaitable[TelegramDeliveryResult]]

_READINESS_CACHE_TTL_SECONDS = 300

_READINESS_PROBE_TIMEOUT_SECONDS = 10
_READINESS_TOTAL_TIMEOUT_SECONDS = 12
_READINESS_SAMPLE_SIZE = 3
_READINESS_MESSAGES = {
    'database_unavailable': 'Unable to load Hanime1 readiness targets.',
    'disabled': 'Hanime1 is disabled.',
    'no_targets': 'No Hanime1 scan targets are configured.',
    'ok': 'Hanime1 playlist parsing is healthy.',
    'parser_incompatible': 'Hanime1 playlist structure is incompatible with the parser.',
    'probe_failed': 'Hanime1 readiness probes failed.',
    'upstream_timeout': 'Hanime1 readiness check timed out.',
    'upstream_unavailable': 'Hanime1 watch pages are unavailable.',
}


def _missing_fields(model: BaseModel) -> list[str]:
    """Fields a section still needs before its source can run; empty for sections without a job."""
    validate_runnable = getattr(model, 'validate_runnable', None)
    return list(validate_runnable()) if callable(validate_runnable) else []


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
        control_request_lister: ControlRequestLister | None = None,
        settings_section_getter: SettingsSectionGetter | None = None,
        settings_section_saver: SettingsSectionSaver | None = None,
        telegram_notification_tester: TelegramNotificationTester | None = None,
        runtime_service: Hanime1SeriesService | None = None,
        author_service: Hanime1AuthorService | None = None,
        kemono_creator_resolver: KemonoCreatorResolver | None = None,
        archive_library: ArchiveLibrary | None = None,
        azurlane_library: AzurLaneLibrary | None = None,
        nikke_library: NikkeLibrary | None = None,
        bd2_library: BD2Library | None = None,
        live2d_view_override_store: Live2DViewOverrideStore | None = None,
    ) -> None:
        self._dsn = dsn
        self._token = token
        self._hanime1_video_fetcher = hanime1_video_fetcher or (
            lambda dsn: fetch_hanime1_videos_from_db(dsn, host=settings.load().web.hanime1.host)
        )
        self._now_provider = now_provider or (lambda: datetime.now(UTC))
        self._job_provider = job_provider or build_jobs
        self._control_request_creator = control_request_creator or (
            lambda kind, target: create_control_request_sync(self._dsn, kind=kind, target=target)
        )
        self._control_request_getter = control_request_getter or (lambda request_id: get_control_request_sync(self._dsn, request_id))
        self._control_request_lister = control_request_lister or (
            lambda statuses, limit: list_control_requests_sync(self._dsn, statuses=statuses, limit=limit)
        )
        self._settings_section_getter = settings_section_getter or settings.load_section
        self._settings_section_saver = settings_section_saver or settings.save_section
        self._telegram_notification_tester = telegram_notification_tester or send_test_notification
        self._archive_library = archive_library or ArchiveLibrary(self._dsn)
        self._runtime_service = runtime_service or Hanime1SeriesService(
            dsn=self._dsn,
            host=settings.load().web.hanime1.host,
            user_lang=settings.load().web.hanime1.user_lang,
        )
        self._author_service = author_service or Hanime1AuthorService(
            dsn=self._dsn,
            host=settings.load().web.hanime1.host,
            user_lang=settings.load().web.hanime1.user_lang,
        )
        self._kemono_creator_resolver = kemono_creator_resolver or KemonoCreatorResolver()
        self._azurlane_library = azurlane_library or AzurLaneLibrary(settings.load().web.azurlane.path)
        self._nikke_library = nikke_library or NikkeLibrary(settings.load().web.nikke.path)
        self._bd2_library = bd2_library or BD2Library(settings.load().web.bd2.path)
        self._live2d_view_override_store = live2d_view_override_store or PostgresLive2DViewOverrideStore(self._dsn)
        self._readiness_cache: tuple[float, dict[str, object]] | None = None
        self._readiness_lock = asyncio.Lock()

    def close(self) -> None:
        for service in (self._runtime_service, self._author_service, self._kemono_creator_resolver):
            close = getattr(service, 'close', None)
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

    async def get_readiness(self) -> dict[str, object]:
        cached = self._get_cached_readiness()
        if cached is not None:
            return cached

        async with self._readiness_lock:
            cached = self._get_cached_readiness()
            if cached is not None:
                return cached
            payload = await self._build_readiness()
            self._readiness_cache = (monotonic(), payload)
            return payload

    def _get_cached_readiness(self) -> dict[str, object] | None:
        if self._readiness_cache is None:
            return None
        checked_at, payload = self._readiness_cache
        return payload if monotonic() - checked_at < _READINESS_CACHE_TTL_SECONDS else None

    async def _build_readiness(self) -> dict[str, object]:
        if not settings.load().web.hanime1.enabled:
            return self._readiness_payload(component_status='skipped', code='disabled')

        seed_ids: list[str] = []
        try:
            async with asyncio.timeout(_READINESS_TOTAL_TIMEOUT_SECONDS):
                try:
                    seed_ids = await asyncio.to_thread(self._runtime_service.readiness_seed_ids, limit=_READINESS_SAMPLE_SIZE)
                except Exception:
                    log.exception('Failed to load Hanime1 readiness targets')
                    return self._readiness_payload(component_status='degraded', code='database_unavailable')

                if not seed_ids:
                    return self._readiness_payload(component_status='skipped', code='no_targets')

                failures = await self._probe_readiness_seeds(seed_ids)
        except TimeoutError:
            return self._readiness_payload(
                component_status='degraded',
                code='upstream_timeout',
                sampled_targets=len(seed_ids),
            )

        if failures is None:
            return self._readiness_payload(
                component_status='ok',
                code='ok',
                sampled_targets=len(seed_ids),
            )

        if all(isinstance(failure, TimeoutError) for failure in failures):
            code = 'upstream_timeout'
        elif all(isinstance(failure, Hanime1ParserIncompatibleError) for failure in failures):
            code = 'parser_incompatible'
        elif any(isinstance(failure, Hanime1ParserIncompatibleError) for failure in failures):
            code = 'probe_failed'
        else:
            code = 'upstream_unavailable'
        return self._readiness_payload(
            component_status='degraded',
            code=code,
            sampled_targets=len(seed_ids),
        )

    async def _probe_readiness_seeds(self, seed_ids: list[str]) -> list[BaseException] | None:
        tasks = [
            asyncio.create_task(
                asyncio.wait_for(
                    asyncio.to_thread(
                        self._runtime_service.probe_seed,
                        seed_id,
                        timeout_seconds=_READINESS_PROBE_TIMEOUT_SECONDS,
                    ),
                    timeout=_READINESS_PROBE_TIMEOUT_SECONDS,
                ),
            )
            for seed_id in seed_ids
        ]
        failures: list[BaseException] = []
        try:
            for completed in asyncio.as_completed(tasks):
                try:
                    await completed
                except Exception as exc:  # noqa: BLE001
                    failures.append(exc)
                else:
                    return None
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
        return failures

    def _readiness_payload(
        self,
        *,
        component_status: str,
        code: str,
        sampled_targets: int = 0,
    ) -> dict[str, object]:
        status = 'degraded' if component_status == 'degraded' else 'ok'
        return {
            'status': status,
            'generated_at': utc_now_iso_z(self._now_provider),
            'checks': {
                'hanime1': {
                    'status': component_status,
                    'code': code,
                    'message': _READINESS_MESSAGES[code],
                    'sampled_targets': sampled_targets,
                },
            },
        }

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

    def list_job_requests(self, *, statuses: list[str] | None = None, limit: int = 50) -> list[dict[str, object]]:
        try:
            requests = self._control_request_lister(statuses, limit)
        except ValueError as exc:
            raise ApiError(status_code=422, code='invalid_status', message=str(exc)) from None
        except Exception:
            log.exception('Failed to list job requests')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        return [serialize_control_request(request) for request in requests]

    def list_settings(self) -> list[dict[str, object]]:
        return [self.get_settings_section(section) for section in SECTION_MODELS]

    def get_settings_section(self, section: str) -> dict[str, object]:
        try:
            model = self._settings_section_getter(section)
        except UnknownSectionError:
            raise ApiError(status_code=404, code='unknown_section', message=f'Unknown settings section: {section}') from None
        except Exception:
            log.exception('Failed to load settings section=%s', section)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

        payload = json.loads(model.model_dump_json())
        return {
            'section': section,
            'value': mask_section(section, payload),
            'missing_fields': self._missing_fields_after_fallback(model),
        }

    def _shared_cookiecloud_configured(self) -> bool:
        """Whether the deployment-wide CookieCloud credential is fully filled in."""
        try:
            shared = self._settings_section_getter('credentials.cookiecloud')
        except Exception:  # noqa: BLE001 - an unreadable shared credential just means no fallback
            return False
        return bool(getattr(shared, 'configured', False))

    def _missing_fields_after_fallback(self, model: BaseModel) -> list[str]:
        """A section's missing fields, minus the CookieCloud ones the shared credential covers.

        Mirrors ``Settings.apply_shared_cookiecloud``: at run time an unconfigured
        per-source CookieCloud block is filled from ``credentials.cookiecloud``, so
        reporting it as missing here would block enabling a source that can run.
        """
        missing = _missing_fields(model)
        if any('cookiecloud.' in name for name in missing) and self._shared_cookiecloud_configured():
            missing = [name for name in missing if 'cookiecloud.' not in name]
        return missing

    def update_settings_section(self, section: str, payload: dict[str, Any]) -> dict[str, object]:
        if section not in SECTION_MODELS:
            raise ApiError(status_code=404, code='unknown_section', message=f'Unknown settings section: {section}')

        try:
            stored = json.loads(self._settings_section_getter(section).model_dump_json())
        except Exception:
            log.exception('Failed to load settings section=%s', section)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

        merged = unmask_section(section, payload, stored)

        # Validate before persisting so an incomplete source is rejected instead of
        # being written and then silently parked by the scheduler.
        try:
            candidate = SECTION_MODELS[section].model_validate(merged)
        except PydanticValidationError as exc:
            raise ApiError(
                status_code=422,
                code='invalid_settings',
                message='Settings validation failed.',
                details=json.loads(exc.json()),
            ) from None
        except ValueError as exc:
            raise ApiError(status_code=422, code='invalid_settings', message=str(exc)) from None

        candidate_missing = self._missing_fields_after_fallback(candidate)
        if getattr(candidate, 'enabled', False) and candidate_missing:
            raise ApiError(
                status_code=422,
                code='incomplete_settings',
                message='Cannot enable a source while its configuration is incomplete.',
                details={'missing_fields': candidate_missing},
            )

        try:
            saved = self._settings_section_saver(section, merged)
        except Exception:
            log.exception('Failed to save settings section=%s', section)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

        saved_payload = json.loads(saved.model_dump_json())
        missing = self._missing_fields_after_fallback(saved)
        return {
            'section': section,
            'value': mask_section(section, saved_payload),
            'missing_fields': missing,
        }

    async def test_telegram_notification(self) -> dict[str, object]:
        """Send a test message so the operator can confirm the bot can reach the chat."""
        try:
            result = await self._telegram_notification_tester()
        except TelegramNotConfiguredError:
            raise ApiError(
                status_code=422,
                code='telegram_not_configured',
                message='Save bot_token and chat_id before sending a test notification.',
            ) from None
        except TelegramDeliveryError as exc:
            raise ApiError(status_code=502, code='telegram_delivery_failed', message=str(exc)) from None
        return {
            'status': 'delivered',
            'message_id': result.message_id,
            'warnings': list(result.warnings),
        }

    def test_azurlane_proxy(self, payload: dict[str, Any]) -> dict[str, Any]:
        """Check that the l2d.su origin is reachable through a proxy, without saving it.

        The draft from the form is probed as-is: proxies are shown to the UI in
        plaintext, so there is no masked value to resolve.
        """
        result = probe_l2d_su_origin(str(payload.get('origin_proxy') or ''))
        return {'ok': result.ok, 'code': result.code, 'message': result.message, 'exit_ip': result.exit_ip}

    def _stored_section(self, section: str) -> dict[str, Any]:
        try:
            return json.loads(self._settings_section_getter(section).model_dump_json())
        except UnknownSectionError:
            raise ApiError(status_code=404, code='unknown_section', message=f'Unknown settings section: {section}') from None
        except Exception:
            log.exception('Failed to load %s for a settings test', section)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

    def test_rednote_proxy(self, payload: dict[str, Any]) -> dict[str, Any]:
        """Check the egress the RedNote source would use, without saving it.

        The draft from the form is probed as-is: proxies are shown to the UI in
        plaintext, so there is no masked value to resolve.
        """
        result = probe_rednote_proxy(str(payload.get('proxy') or ''))
        return {
            'ok': result.ok,
            'code': result.code,
            'message': result.message,
            'exit_ip': result.exit_ip,
            'direct': result.direct,
        }

    def _stored_account_password(self, account: str) -> str:
        """The CookieCloud password stored for one bilibili account, matched by name."""
        stored = self._stored_section('web.bilibili')
        accounts = stored.get('accounts')
        for candidate in accounts if isinstance(accounts, list) else []:
            if isinstance(candidate, dict) and str(candidate.get('name') or '') == account:
                nested = candidate.get('cookiecloud')
                nested = nested if isinstance(nested, dict) else {}
                return str(nested.get('password') or '')
        return ''

    def _stored_section_password(self, section: str) -> str:
        """The CookieCloud password of a source that holds a single vault."""
        nested = self._stored_section(section).get('cookiecloud')
        return str((nested if isinstance(nested, dict) else {}).get('password') or '')

    def _stored_cookiecloud_password(self, *, source: str, account: str) -> str:
        """Resolve a masked password against whatever the source stores it under.

        Bilibili keys its vaults by account name; the shared credential is its own
        section; every other source has exactly one, so it is read straight from
        that source's section.
        """
        if source == 'shared':
            return str(self._stored_section('credentials.cookiecloud').get('password') or '')
        if source == 'bilibili':
            if not account:
                raise ApiError(status_code=422, code='invalid_settings', message='account is required.')
            return self._stored_account_password(account)
        return self._stored_section_password(f'web.{source}')

    def test_cookiecloud(self, payload: dict[str, Any]) -> dict[str, Any]:
        """Check a source's CookieCloud credentials without saving them.

        The draft from the form wins, except that a masked or omitted password is
        resolved against what is already stored for that source. ``source='shared'``
        tests the deployment-wide credential: reachability and decryption only,
        since it is not tied to any one source's cookies.
        """
        source = str(payload.get('source') or 'bilibili')
        profile = None if source == 'shared' else cookiecloud_tool.PROFILES.get(source)
        if profile is None and source != 'shared':
            raise ApiError(status_code=422, code='invalid_settings', message=f'Unknown cookiecloud source: {source}')

        draft = {key: str(payload.get(key) or '') for key in ('server_url', 'uuid', 'password')}
        keep_secret(draft, 'password', self._stored_cookiecloud_password(source=source, account=str(payload.get('account') or '')))

        result = cookiecloud_tool.probe(draft['server_url'], draft['uuid'], draft['password'], profile=profile)
        return {
            'ok': result.ok,
            'code': result.code,
            'message': result.message,
            'domain_count': result.domain_count,
            'domain_cookie_count': result.domain_cookie_count,
            'missing_cookies': list(result.missing_cookies),
        }

    def list_archive_sources(self) -> list[dict[str, Any]]:
        try:
            return self._archive_library.list_sources()
        except Exception:
            log.exception('Failed to list archive sources')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

    def list_archive_items(self, *, source: str, query: str = '', limit: int = 50, offset: int = 0) -> dict[str, Any]:
        try:
            return self._archive_library.list_items(source_key=source, query=query, limit=limit, offset=offset)
        except UnknownArchiveSourceError:
            raise ApiError(
                status_code=404,
                code='unknown_archive_source',
                message=f'Unknown archive source: {source}',
                details={'allowed_values': sorted(ARCHIVE_SOURCES)},
            ) from None
        except Exception:
            log.exception('Failed to list archive items source=%s', source)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

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
            log.exception('Failed to add Hanime1 series seed')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        return serialize_seed(created_seed)

    def list_hanime1_seeds(self) -> list[dict[str, Any]]:
        try:
            seeds = self._runtime_service.list_seeds()
        except Exception:
            log.exception('Failed to list Hanime1 series seeds')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        return [
            {
                **seed,
                'created_at': serialize_datetime(seed.get('created_at')),
                'updated_at': serialize_datetime(seed.get('updated_at')),
                'last_scanned_at': serialize_datetime(seed.get('last_scanned_at')),
            }
            for seed in seeds
        ]

    def delete_hanime1_seed(self, canonical_video_id: str) -> None:
        try:
            deleted = self._runtime_service.delete_seed(canonical_video_id)
        except ValueError:
            raise ApiError(status_code=422, code='invalid_seed', message='Seed id is invalid.') from None
        except Exception:
            log.exception('Failed to delete Hanime1 series seed id=%s', canonical_video_id)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        if not deleted:
            raise ApiError(status_code=404, code='not_found', message='Hanime1 seed not found.')

    def add_hanime1_author(self, author: str) -> dict[str, str]:
        try:
            return self._author_service.add_author(author)
        except ValueError:
            raise ApiError(status_code=422, code='invalid_author', message='Author id or URL is invalid.') from None
        except FileExistsError:
            raise ApiError(status_code=409, code='duplicate_author', message='Hanime1 author already exists.') from None
        except LookupError:
            raise ApiError(status_code=422, code='author_resolve_failed', message='Unable to resolve Hanime1 author.') from None
        except Exception:
            log.exception('Failed to add Hanime1 author subscription')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

    def list_hanime1_authors(self) -> list[dict[str, Any]]:
        try:
            authors = self._author_service.list_authors()
        except Exception:
            log.exception('Failed to list Hanime1 author subscriptions')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        return [
            {
                **author,
                'created_at': serialize_datetime(author.get('created_at')),
                'updated_at': serialize_datetime(author.get('updated_at')),
                'last_scanned_at': serialize_datetime(author.get('last_scanned_at')),
            }
            for author in authors
        ]

    def delete_hanime1_author(self, author_id: str) -> None:
        try:
            deleted = self._author_service.delete_author(author_id)
        except ValueError:
            raise ApiError(status_code=422, code='invalid_author', message='Author id is invalid.') from None
        except Exception:
            log.exception('Failed to delete Hanime1 author subscription id=%s', author_id)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        if not deleted:
            raise ApiError(status_code=404, code='not_found', message='Hanime1 author not found.')

    def resolve_kemono_creator(self, creator: str) -> dict[str, str]:
        try:
            return self._kemono_creator_resolver.resolve(creator)
        except ValueError:
            raise ApiError(status_code=422, code='invalid_creator', message='Creator id or URL is invalid.') from None
        except LookupError:
            raise ApiError(status_code=404, code='creator_not_found', message='Creator not found upstream.') from None
        except ConnectionError:
            raise ApiError(status_code=502, code='creator_resolve_failed', message='Unable to reach the Kemono site.') from None
        except Exception:
            log.exception('Failed to resolve Kemono creator')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

    def list_azurlane_characters(self) -> list[dict[str, object]]:
        try:
            return self._azurlane_library.list_characters()
        except Exception:
            log.exception('Failed to list Azur Lane characters')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

    def get_azurlane_character(self, character_key: str) -> dict[str, object]:
        try:
            return self._azurlane_library.get_character(character_key)
        except AzurLaneCharacterNotFoundError:
            raise ApiError(status_code=404, code='azurlane_character_not_found', message='Azur Lane character not found.') from None
        except Exception:
            log.exception('Failed to get Azur Lane character character_key=%s', character_key)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

    def get_azurlane_ship_detail(self, character_key: str) -> dict[str, object]:
        try:
            return self._azurlane_library.get_ship_detail(character_key)
        except (AzurLaneCharacterNotFoundError, AzurLaneAssetNotFoundError):
            raise ApiError(status_code=404, code='azurlane_ship_detail_not_found', message='Azur Lane ship detail not found.') from None
        except Exception:
            log.exception('Failed to get Azur Lane ship detail character_key=%s', character_key)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

    def get_azurlane_skin_updates(self) -> dict[str, object]:
        try:
            return self._azurlane_library.get_skin_updates()
        except AzurLaneAssetNotFoundError:
            raise ApiError(status_code=404, code='azurlane_skin_updates_not_found', message='Azur Lane skin updates not found.') from None
        except Exception:
            log.exception('Failed to get Azur Lane skin updates')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

    def list_bd2_characters(self) -> list[dict[str, object]]:
        try:
            return self._bd2_library.list_characters()
        except Exception:
            log.exception('Failed to list BD2 characters')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

    def get_bd2_character(self, content_id: int) -> dict[str, object]:
        try:
            character = self._bd2_library.get_character(content_id)
        except BD2CharacterNotFoundError:
            raise ApiError(status_code=404, code='bd2_character_not_found', message='BD2 character not found.') from None
        except Exception:
            log.exception('Failed to get BD2 character content_id=%d', content_id)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        self._apply_live2d_view_overrides(source='bd2', content_id=content_id, character=character)
        return character

    def list_nikke_characters(self) -> list[dict[str, object]]:
        try:
            return self._nikke_library.list_characters()
        except Exception:
            log.exception('Failed to list Nikke characters')
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

    def get_nikke_character(self, content_id: int) -> dict[str, object]:
        try:
            character = self._nikke_library.get_character(content_id)
        except NikkeCharacterNotFoundError:
            raise ApiError(status_code=404, code='nikke_character_not_found', message='Nikke character not found.') from None
        except Exception:
            log.exception('Failed to get Nikke character content_id=%d', content_id)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        self._apply_live2d_view_overrides(source='nikke', content_id=content_id, character=character)
        return character

    def get_live2d_view_override(self, *, source: str, content_id: int, model_id: str, profile: str) -> dict[str, object]:
        source = self._normalize_live2d_source(source)
        profile = self._normalize_live2d_profile(profile)
        self._require_live2d_model(source=source, content_id=content_id, model_id=model_id)
        try:
            override = self._live2d_view_override_store.get(source=source, content_id=content_id, model_id=model_id, profile=profile)
        except Exception:
            log.exception(
                'Failed to get Live2D view override source=%s content_id=%d model_id=%s profile=%s',
                source,
                content_id,
                model_id,
                profile,
            )
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        if override is None:
            raise ApiError(status_code=404, code='live2d_view_override_not_found', message='Live2D view override not found.')
        return self._serialize_live2d_view_override(override)

    def upsert_live2d_view_override(  # noqa: PLR0913
        self,
        *,
        source: str,
        content_id: int,
        model_id: str,
        profile: str,
        position: dict[str, float],
        scale: float,
        background_position: dict[str, float] | None = None,
        background_scale: float | None = None,
    ) -> dict[str, object]:
        source = self._normalize_live2d_source(source)
        profile = self._normalize_live2d_profile(profile)
        self._require_live2d_model(source=source, content_id=content_id, model_id=model_id)
        try:
            override = self._live2d_view_override_store.upsert(
                source=source,
                content_id=content_id,
                model_id=model_id,
                profile=profile,
                position=position,
                scale=scale,
                background_position=background_position,
                background_scale=background_scale,
            )
        except Exception:
            log.exception(
                'Failed to save Live2D view override source=%s content_id=%d model_id=%s profile=%s',
                source,
                content_id,
                model_id,
                profile,
            )
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        return self._serialize_live2d_view_override(override)

    def delete_live2d_view_override(self, *, source: str, content_id: int, model_id: str, profile: str) -> None:
        source = self._normalize_live2d_source(source)
        profile = self._normalize_live2d_profile(profile)
        self._require_live2d_model(source=source, content_id=content_id, model_id=model_id)
        try:
            deleted = self._live2d_view_override_store.delete(source=source, content_id=content_id, model_id=model_id, profile=profile)
        except Exception:
            log.exception(
                'Failed to delete Live2D view override source=%s content_id=%d model_id=%s profile=%s',
                source,
                content_id,
                model_id,
                profile,
            )
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        if not deleted:
            raise ApiError(status_code=404, code='live2d_view_override_not_found', message='Live2D view override not found.')

    def _apply_live2d_view_overrides(self, *, source: str, content_id: int, character: dict[str, object]) -> None:
        try:
            overrides = self._live2d_view_override_store.list_for_character(source=source, content_id=content_id)
        except Exception:
            log.exception('Failed to list Live2D view overrides source=%s content_id=%d', source, content_id)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None
        apply_live2d_view_overrides(character, [self._serialize_live2d_view_override(override) for override in overrides])

    def _require_live2d_model(self, *, source: str, content_id: int, model_id: str) -> None:
        character = self._load_character_for_live2d_source(source=source, content_id=content_id)
        assign_live2d_model_ids(character)
        if any(model.get('model_id') == model_id for model in iter_live2d_models(character)):
            return
        raise ApiError(
            status_code=404,
            code=f'{source}_live2d_model_not_found',
            message='Live2D model not found.',
            details={'source': source, 'content_id': content_id, 'model_id': model_id},
        )

    def _load_character_for_live2d_source(self, *, source: str, content_id: int) -> dict[str, object]:
        try:
            if source == 'bd2':
                return self._bd2_library.get_character(content_id)
            if source == 'nikke':
                return self._nikke_library.get_character(content_id)
        except BD2CharacterNotFoundError:
            raise ApiError(status_code=404, code='bd2_character_not_found', message='BD2 character not found.') from None
        except NikkeCharacterNotFoundError:
            raise ApiError(status_code=404, code='nikke_character_not_found', message='Nikke character not found.') from None
        except Exception:
            log.exception('Failed to load Live2D source character source=%s content_id=%d', source, content_id)
            raise ApiError(status_code=500, code='internal_server_error', message='Internal server error.') from None

        raise ApiError(status_code=400, code='invalid_live2d_source', message='Unsupported Live2D source.')

    @staticmethod
    def _normalize_live2d_source(source: str) -> str:
        try:
            return validate_live2d_source(source)
        except ValueError as exc:
            raise ApiError(status_code=400, code='invalid_live2d_source', message=str(exc)) from None

    @staticmethod
    def _normalize_live2d_profile(profile: str) -> str:
        try:
            return validate_live2d_profile(profile)
        except ValueError as exc:
            raise ApiError(status_code=422, code='invalid_live2d_profile', message=str(exc)) from None

    @staticmethod
    def _serialize_live2d_view_override(override: dict[str, object]) -> dict[str, object]:
        return {
            'source': str(override.get('source') or ''),
            'content_id': int(override.get('content_id') or 0),
            'model_id': str(override.get('model_id') or ''),
            'profile': str(override.get('profile') or ''),
            'position': override.get('position') if isinstance(override.get('position'), dict) else {},
            'scale': float(override.get('scale') or 0),
            'background_position': override.get('background_position') if isinstance(override.get('background_position'), dict) else None,
            'background_scale': float(override['background_scale']) if override.get('background_scale') is not None else None,
            'created_at': _serialize_live2d_datetime(override.get('created_at')),
            'updated_at': _serialize_live2d_datetime(override.get('updated_at')),
        }

    @staticmethod
    def model_health(payload: dict[str, str]) -> HealthResponse:
        return HealthResponse.model_validate(payload)

    @staticmethod
    def model_readiness(payload: dict[str, object]) -> ReadinessResponse:
        return ReadinessResponse.model_validate(payload)

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

    @staticmethod
    def model_azurlane_character_summary(payload: dict[str, object]) -> AzurLaneCharacterSummary:
        return AzurLaneCharacterSummary.model_validate(payload)

    @staticmethod
    def model_azurlane_character_detail(payload: dict[str, object]) -> AzurLaneCharacterDetail:
        return AzurLaneCharacterDetail.model_validate(payload)

    @staticmethod
    def model_bd2_character_summary(payload: dict[str, object]) -> BD2CharacterSummary:
        return BD2CharacterSummary.model_validate(payload)

    @staticmethod
    def model_bd2_character_detail(payload: dict[str, object]) -> BD2CharacterDetail:
        return BD2CharacterDetail.model_validate(payload)

    @staticmethod
    def model_nikke_character_summary(payload: dict[str, object]) -> NikkeCharacterSummary:
        return NikkeCharacterSummary.model_validate(payload)

    @staticmethod
    def model_nikke_character_detail(payload: dict[str, object]) -> NikkeCharacterDetail:
        return NikkeCharacterDetail.model_validate(payload)

    @staticmethod
    def model_live2d_view_override(payload: dict[str, object]) -> Live2DViewOverride:
        return Live2DViewOverride.model_validate(payload)

    @staticmethod
    def model_hanime1_seed_detail(payload: dict[str, Any]) -> Hanime1SeedDetail:
        return Hanime1SeedDetail.model_validate(payload)

    @staticmethod
    def model_hanime1_author(payload: dict[str, str]) -> Hanime1Author:
        return Hanime1Author.model_validate(payload)

    @staticmethod
    def model_hanime1_author_detail(payload: dict[str, Any]) -> Hanime1AuthorDetail:
        return Hanime1AuthorDetail.model_validate(payload)

    @staticmethod
    def model_kemono_creator(payload: dict[str, str]) -> KemonoCreatorResolved:
        return KemonoCreatorResolved.model_validate(payload)

    @staticmethod
    def model_settings_section(payload: dict[str, object]) -> SettingsSection:
        return SettingsSection.model_validate(payload)

    @staticmethod
    def model_telegram_notification_test(payload: dict[str, object]) -> TelegramNotificationTestResponse:
        return TelegramNotificationTestResponse.model_validate(payload)

    @staticmethod
    def model_archive_source(payload: dict[str, Any]) -> ArchiveSourceStat:
        return ArchiveSourceStat.model_validate(payload)

    @staticmethod
    def model_archive_list(payload: dict[str, Any]) -> ArchiveListResponse:
        return ArchiveListResponse.model_validate(
            {
                'items': [ArchiveItem.model_validate(item) for item in payload['items']],
                'total': payload['total'],
                'limit': payload['limit'],
                'offset': payload['offset'],
            },
        )


def _serialize_live2d_datetime(value: object) -> str:
    if isinstance(value, datetime):
        return serialize_datetime(value) or ''
    return str(value or '')

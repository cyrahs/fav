"""MCP endpoint (``/mcp``) for developing and operating fav without a shell in the pod.

It is registered in the cluster's LiteLLM MCP gateway, which is the only caller
that reaches it with credentials; the public hostname sits behind Cloudflare
Access, the same arrangement as the other in-cluster MCP servers. Tools reuse
the API service where the UI already has an operation, and ``ops`` for the rest.
"""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING, Any

from mcp.server.fastmcp import FastMCP
from mcp.server.fastmcp.exceptions import ToolError
from mcp.server.transport_security import TransportSecuritySettings

from src.core import logger

from . import ops
from .errors import ApiError

if TYPE_CHECKING:
    from collections.abc import Awaitable, Callable

    from .service import FavApiService

log = logger.get('fav-mcp')

MCP_PATH = '/mcp'

_INSTRUCTIONS = """\
fav crawls content sources on a schedule (one job per source), stores media under collection/ and its
state in PostgreSQL, and notifies through Telegram. The API and the worker run in one pod; the worker
executes runs, and every run, scheduled or manual, is recorded in control_requests (list_runs).

Typical checks: get_runtime for the deployed git commit and restart times; list_jobs for schedules and
each job's last run; list_runs/get_run for errors; read_logs for detail (log files survive pod restarts);
wechat_status for the WeChat source. run_job queues a manual run that the worker picks up within about a
second; poll get_run with the returned id. Secret values are masked in settings, logs and errors."""


def _tool_error(exc: ApiError) -> ToolError:
    details = f' Details: {exc.details}' if exc.details else ''
    return ToolError(f'{exc.message}{details}')


async def _call_service(func: Callable[..., Any], *args: Any, **kwargs: Any) -> Any:
    try:
        return await asyncio.to_thread(func, *args, **kwargs)
    except ApiError as exc:
        raise _tool_error(exc) from None


def _merge(base: dict[str, Any], changes: dict[str, Any]) -> dict[str, Any]:
    merged = dict(base)
    for key, value in changes.items():
        current = merged.get(key)
        merged[key] = _merge(current, value) if isinstance(current, dict) and isinstance(value, dict) else value
    return merged


def build_mcp_server(get_service: Callable[[], FavApiService]) -> FastMCP:  # noqa: C901, PLR0915
    mcp = FastMCP(
        name='fav',
        instructions=_INSTRUCTIONS,
        streamable_http_path=MCP_PATH,
        stateless_http=True,
        json_response=True,
        # Reached by in-cluster Service names and the public hostname alike; the
        # localhost-only Host check FastMCP enables by default would reject both.
        transport_security=TransportSecuritySettings(enable_dns_rebinding_protection=False),
    )

    async def redactor() -> ops.Redactor:
        # The settings snapshot may need a (synchronous) database read.
        return await asyncio.to_thread(lambda: ops.Redactor(ops.collect_secrets()))

    async def ops_call(coro_factory: Callable[[], Awaitable[Any]]) -> Any:
        try:
            return await coro_factory()
        except ops.OpsError as exc:
            raise ToolError(str(exc)) from None

    # ---------- runtime ----------

    @mcp.tool()
    async def get_runtime() -> dict[str, Any]:
        """Deployed version and process state: git commit and build time of the image, pod name,
        when the container, API and worker processes started, and whether the worker is running."""
        return await asyncio.to_thread(ops.runtime_info)

    @mcp.tool()
    async def list_jobs() -> list[dict[str, Any]]:
        """Every job (one per source): enabled, cron, next run time, settings section, fields still
        missing before it can run, and its latest run."""
        jobs = await asyncio.to_thread(get_service().scheduled_jobs)
        return ops.describe_jobs(jobs, await ops.last_runs_by_job())

    @mcp.tool()
    async def list_runs(job: str = '', status: str = '', kind: str = '', since: str = '', limit: int = 20) -> list[dict[str, Any]]:
        """Recent runs, newest first, from control_requests.

        Args:
            job: job key such as wechat or azurlane ('all' for a manual run of every job).
            status: pending, running, succeeded, failed, rejected or interrupted.
            kind: scheduled_job or trigger_job (manual).
            since: ISO 8601 time or relative 30m/6h/2d.
            limit: at most 500. Errors are cut at 4000 characters; get_run returns them whole.
        """
        redact = await redactor()
        return await ops_call(
            lambda: ops.list_runs(job=job, status=status, kind=kind, since=ops.parse_since(since), limit=limit, redact=redact),
        )

    @mcp.tool()
    async def get_run(run_id: int) -> dict[str, Any]:
        """One run with its full result and error text."""
        run = await ops.get_run(run_id, redact=await redactor())
        if run is None:
            msg = f'Run {run_id} not found.'
            raise ToolError(msg)
        return run

    @mcp.tool()
    async def read_logs(  # noqa: PLR0913
        since: str = '1h',
        until: str = '',
        contains: str = '',
        logger_name: str = '',
        level: str = 'INFO',
        limit: int = 200,
    ) -> dict[str, Any]:
        """Application log records from the log files on the data volume (API and worker), which
        outlive pod restarts. Returns the newest matching records, oldest first.

        Args:
            since: ISO 8601 time or relative 30m/6h/2d.
            until: optional end of the window, same formats.
            contains: case-insensitive substring of the message (tracebacks included).
            logger_name: case-insensitive substring of the logger, e.g. wechat, azurlane, fav-api.
            level: minimum level: DEBUG, INFO, NOTICE, WARNING, ERROR or CRITICAL.
            limit: at most 1000 records.
        """
        from src.core.logger import log_dir  # noqa: PLC0415 - the logger module creates the directory on import

        def read() -> dict[str, Any]:
            return ops.read_logs(
                log_dir.resolve(),
                since=ops.parse_since(since),
                until=ops.parse_since(until),
                contains=contains,
                logger_name=logger_name,
                min_level=level,
                limit=limit,
                redact=ops.Redactor(ops.collect_secrets()),
            )

        try:
            return await asyncio.to_thread(read)
        except ops.OpsError as exc:
            raise ToolError(str(exc)) from None

    # ---------- settings ----------

    @mcp.tool()
    async def get_settings(section: str = '') -> list[dict[str, Any]] | dict[str, Any]:
        """Settings as the web UI shows them, secrets masked. Without a section, every section.
        Sections are named like web.wechat, web.azurlane, notifications.telegram, cookiecloud."""
        service = get_service()
        if section:
            return await _call_service(service.get_settings_section, section)
        return await _call_service(service.list_settings)

    @mcp.tool()
    async def update_settings(section: str, values: dict[str, Any]) -> dict[str, Any]:
        """Change settings in one section. `values` holds only the fields to change; nested objects
        are merged and lists are replaced whole. Masked secrets are kept as stored. The change is
        validated like a save from the UI and applies to the next run without a restart."""
        service = get_service()
        current = await _call_service(service.get_settings_section, section)
        merged = _merge(current['value'], values)
        result = await _call_service(service.update_settings_section, section, merged)
        log.info('MCP updated settings section=%s fields=%s', section, sorted(values))
        return result

    # ---------- control ----------

    @mcp.tool()
    async def run_job(job: str) -> dict[str, Any]:
        """Queue a manual run of a job (or 'all' for every enabled job), the same as the Run button.
        The worker claims it within about a second; poll get_run with the returned id."""
        result = await _call_service(get_service().create_job_request, job)
        log.info('MCP queued job=%s request=%s', job, result.get('id'))
        return result

    @mcp.tool()
    async def send_test_notification() -> dict[str, Any]:
        """Send a Telegram test message to check that the bot can reach the chat."""
        try:
            return await get_service().test_telegram_notification()
        except ApiError as exc:
            raise _tool_error(exc) from None

    # ---------- WeChat ----------

    @mcp.tool()
    async def wechat_status(recent_failures: int = 10) -> dict[str, Any]:
        """WeChat source state: wechat_state (events_after cursor, work_version), event counts by
        work and status, and the latest failed or retrying events with their errors."""
        return await ops.wechat_status(redact=await redactor(), recent_failures=recent_failures)

    @mcp.tool()
    async def list_wechat_events(
        status: str = '',
        work: str = '',
        msg_ids: list[int] | None = None,
        include_message: bool = False,  # noqa: FBT001, FBT002 - MCP tool arguments are keyword-only on the wire
        limit: int = 20,
    ) -> list[dict[str, Any]]:
        """WeChat events queued for processing, newest first.

        Args:
            status: pending, done or failed.
            work: the kind of work, e.g. image, article, channel_video.
            msg_ids: only these message ids.
            include_message: also return the stored bridge message (its content and media keys).
            limit: at most 500.
        """
        redact = await redactor()
        return await ops_call(
            lambda: ops.list_wechat_events(
                status=status,
                work=work,
                msg_ids=msg_ids,
                include_message=include_message,
                limit=limit,
                redact=redact,
            ),
        )

    @mcp.tool()
    async def retry_wechat_events(msg_ids: list[int] | None = None, status: str = 'failed') -> dict[str, Any]:
        """Put WeChat events back to pending with a fresh attempt budget, so the next WeChat run
        processes them again. Without msg_ids, every event in `status` (default failed)."""
        retried = await ops_call(lambda: ops.retry_wechat_events(msg_ids=msg_ids, status=status))
        log.info('MCP retried WeChat events count=%d', len(retried))
        return {'retried': retried, 'count': len(retried)}

    @mcp.tool()
    async def set_wechat_state(key: str, value: str) -> dict[str, Any]:
        """Set a WeChat source state value. events_after is the bridge event cursor (0 re-reads the
        whole event log; already queued events are not duplicated); work_version decides whether
        the log is re-read once after the set of handled message types changes."""
        result = await ops_call(lambda: ops.set_wechat_state(key, value))
        log.info('MCP set WeChat state %s from %s to %s', key, result['previous'], result['value'])
        return result

    # ---------- notifications ----------

    @mcp.tool()
    async def list_notifications(delivery_status: str = '', source: str = '', limit: int = 20) -> list[dict[str, Any]]:
        """Telegram notification outbox, newest first. delivery_status: pending, sending, delivered
        or failed; source: the job that queued it (or worker for job failures)."""
        redact = await redactor()
        return await ops_call(
            lambda: ops.list_notifications(delivery_status=delivery_status, source=source, limit=limit, redact=redact),
        )

    @mcp.tool()
    async def retry_notifications(ids: list[int]) -> dict[str, Any]:
        """Send failed notifications again with a fresh attempt budget."""
        retried = await ops_call(lambda: ops.retry_notifications(ids))
        log.info('MCP retried notifications count=%d', len(retried))
        return {'retried': retried, 'count': len(retried)}

    # ---------- archive ----------

    @mcp.tool()
    async def list_archive_sources() -> list[dict[str, Any]]:
        """What has been collected per source (counts and latest item), as on the Records page."""
        return await _call_service(get_service().list_archive_sources)

    @mcp.tool()
    async def search_archive(source: str, query: str = '', limit: int = 20, offset: int = 0) -> dict[str, Any]:
        """Collected items of one source, newest first, optionally filtered by a text query."""
        return await _call_service(get_service().list_archive_items, source=source, query=query, limit=limit, offset=offset)

    return mcp

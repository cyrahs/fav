from __future__ import annotations

from contextlib import asynccontextmanager
from pathlib import Path
from typing import Any

from fastapi import FastAPI, Request, Response
from fastapi.exceptions import RequestValidationError
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse, JSONResponse
from fastapi.staticfiles import StaticFiles
from starlette.exceptions import HTTPException as StarletteHTTPException

from src.core import logger

from .config import load_config_from_env
from .constants import API_DESCRIPTION, API_TITLE, APP_VERSION, DOCS_URL, HEALTH_ENDPOINT, OPENAPI_URL, READINESS_ENDPOINT, TAG_SYSTEM
from .errors import ApiError
from .routes import router as api_router
from .schemas import ErrorResponse, HealthResponse, ReadinessResponse
from .service import FavApiService

log = logger.get('fav-api')

# Paths owned by the backend. The SPA fallback must not answer for these, or a
# mistyped API route would return an HTML page with status 200.
_NON_SPA_PREFIXES = ('api', 'static', 'docs', 'openapi.json', 'healthz', 'readyz')
_HTTP_NOT_FOUND = 404


def _error_response(
    *,
    status_code: int,
    code: str,
    message: str,
    details: Any = None,
    headers: dict[str, str] | None = None,
) -> JSONResponse:
    payload = ErrorResponse.model_validate({'error': {'code': code, 'message': message, 'details': details}})
    return JSONResponse(
        status_code=status_code,
        content=payload.model_dump(mode='json'),
        headers=headers,
    )


def _register_exception_handlers(app: FastAPI) -> None:
    @app.exception_handler(ApiError)
    async def _handle_api_error(_request: Request, exc: ApiError) -> JSONResponse:
        return _error_response(
            status_code=exc.status_code,
            code=exc.code,
            message=exc.message,
            details=exc.details,
            headers=exc.headers,
        )

    @app.exception_handler(RequestValidationError)
    async def _handle_validation_error(_request: Request, exc: RequestValidationError) -> JSONResponse:
        return _error_response(
            status_code=422,
            code='validation_error',
            message='Request validation failed.',
            details=exc.errors(),
        )

    @app.exception_handler(StarletteHTTPException)
    async def _handle_http_exception(request: Request, exc: StarletteHTTPException) -> Response:
        spa_response = _spa_fallback_response(request, exc)
        if spa_response is not None:
            return spa_response
        error_code = {404: 'not_found', 405: 'method_not_allowed'}.get(exc.status_code, 'http_error')
        return _error_response(
            status_code=exc.status_code,
            code=error_code,
            message=str(exc.detail),
            headers=exc.headers,
        )

    @app.exception_handler(Exception)
    async def _handle_unexpected_error(_request: Request, exc: Exception) -> JSONResponse:
        log.exception('Unhandled API error', exc_info=exc)
        return _error_response(
            status_code=500,
            code='internal_server_error',
            message='Internal server error.',
        )


def create_app(
    *,
    config=None,  # noqa: ANN001
    service: FavApiService | None = None,
) -> FastAPI:
    resolved_config = config if config is not None else (load_config_from_env() if service is None else None)
    owned_service = service is None

    @asynccontextmanager
    async def lifespan(app: FastAPI):  # noqa: ANN202
        app.state.api_service = service or FavApiService(dsn=resolved_config.dsn, token=resolved_config.token)
        try:
            yield
        finally:
            if owned_service:
                app.state.api_service.close()

    app = FastAPI(
        title=API_TITLE,
        description=API_DESCRIPTION,
        version=APP_VERSION,
        docs_url=DOCS_URL,
        openapi_url=OPENAPI_URL,
        lifespan=lifespan,
    )
    if resolved_config is not None and resolved_config.cors_origins:
        app.add_middleware(
            CORSMiddleware,
            allow_origins=list(resolved_config.cors_origins),
            allow_credentials=resolved_config.cors_allow_credentials,
            allow_methods=['DELETE', 'GET', 'POST', 'PUT', 'OPTIONS'],
            allow_headers=['Authorization', 'Content-Type'],
        )
    _register_exception_handlers(app)
    app.include_router(api_router)

    @app.get(
        HEALTH_ENDPOINT,
        operation_id='getHealth',
        response_model=HealthResponse,
        tags=[TAG_SYSTEM],
    )
    def get_health(request: Request) -> HealthResponse:
        return request.app.state.api_service.model_health(request.app.state.api_service.get_health())

    @app.get(
        READINESS_ENDPOINT,
        operation_id='getReadiness',
        response_model=ReadinessResponse,
        responses={503: {'model': ReadinessResponse}},
        tags=[TAG_SYSTEM],
    )
    async def get_readiness(request: Request) -> JSONResponse:
        readiness = request.app.state.api_service.model_readiness(await request.app.state.api_service.get_readiness())
        status_code = 503 if readiness.status == 'degraded' else 200
        return JSONResponse(status_code=status_code, content=readiness.model_dump(mode='json'))

    _mount_web_ui(app)
    return app


def _is_backend_path(path: str) -> bool:
    stripped = path.lstrip('/')
    return any(stripped == prefix or stripped.startswith(f'{prefix}/') for prefix in _NON_SPA_PREFIXES)


def _spa_fallback_response(request: Request, exc: StarletteHTTPException) -> Response | None:
    """Serve index.html for unmatched front-end routes, so deep links survive a refresh.

    Implemented as a 404 handler rather than a catch-all route on purpose: a
    catch-all would also answer for unmatched API paths, turning their 404s into
    HTML 200s and their wrong-method 405s into 405s on a route that should not
    exist at all.
    """
    dist_dir = getattr(request.app.state, 'web_dist_dir', None)
    if dist_dir is None or exc.status_code != _HTTP_NOT_FOUND:
        return None
    if request.method not in {'GET', 'HEAD'} or _is_backend_path(request.url.path):
        return None
    return FileResponse(dist_dir / 'index.html', status_code=200)


def _mount_web_ui(app: FastAPI) -> None:
    """Serve the built front end from web/dist when it is present.

    Absent in development and in API-only deployments, so this is a no-op then.
    """
    dist_dir = Path(__file__).resolve().parents[2] / 'web' / 'dist'
    app.state.web_dist_dir = None
    if not (dist_dir / 'index.html').is_file():
        log.info('web/dist not found; serving API only')
        return

    app.state.web_dist_dir = dist_dir
    app.mount('/assets', StaticFiles(directory=dist_dir / 'assets'), name='web-assets')

    # Icons live at the document root because browsers request them there
    # (/favicon.ico unprompted), and the SPA fallback would otherwise answer
    # those requests with index.html.
    for name in ('favicon.ico', 'icon.svg', 'apple-touch-icon.png'):
        path = dist_dir / name
        if path.is_file():
            app.add_api_route(
                f'/{name}',
                _static_file_endpoint(path),
                methods=['GET'],
                include_in_schema=False,
            )

    log.info('Serving web UI from %s', dist_dir)


def _static_file_endpoint(path: Path):  # noqa: ANN202  # returns an inner endpoint coroutine
    async def endpoint() -> FileResponse:
        return FileResponse(path)

    return endpoint

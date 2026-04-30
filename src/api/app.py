from __future__ import annotations

from contextlib import asynccontextmanager
from typing import Any

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse
from starlette.exceptions import HTTPException as StarletteHTTPException

from src.core import logger

from .config import load_config_from_settings
from .constants import API_DESCRIPTION, API_TITLE, APP_VERSION, DOCS_URL, HEALTH_ENDPOINT, OPENAPI_URL, TAG_SYSTEM
from .errors import ApiError
from .routes import router as api_router
from .schemas import ErrorResponse, HealthResponse
from .service import FavApiService

log = logger.get('fav-api')


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
    async def _handle_http_exception(_request: Request, exc: StarletteHTTPException) -> JSONResponse:
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
    resolved_config = config if config is not None else (load_config_from_settings() if service is None else None)
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
            allow_methods=['GET', 'POST', 'OPTIONS'],
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

    return app

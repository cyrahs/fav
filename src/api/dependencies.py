from __future__ import annotations

from typing import Annotated, Any

from fastapi import Depends, Request

from .constants import HEADER_AUTHORIZATION


def get_api_service(request: Request) -> Any:
    return request.app.state.api_service


def require_api_token(
    request: Request,
    service: Annotated[Any, Depends(get_api_service)],
) -> None:
    service.authenticate(request.headers.get(HEADER_AUTHORIZATION))

"""Bootstrap configuration.

Everything else lives in the ``app_settings`` table and is edited from the web UI.
Only the values needed to reach the database and to protect the settings API
itself are read from the environment.
"""

from __future__ import annotations

from pydantic import Field, field_validator
from pydantic_settings import BaseSettings, SettingsConfigDict

MAX_PORT = 65535
# NOTICE is this app's own level, defined in src/core/logger.py.
LOG_LEVELS = ('DEBUG', 'INFO', 'NOTICE', 'WARNING', 'ERROR', 'CRITICAL')


class Env(BaseSettings):
    postgres_dsn: str = Field(validation_alias='POSTGRES_DSN')
    api_token: str = Field(validation_alias='API_TOKEN')
    # Raise to DEBUG to make a run explain itself: every intercepted XHR, and the
    # shape of the payloads a source parses. Worth its own variable because the
    # alternative is redeploying to learn what one request looked like.
    log_level: str = Field(default='INFO', validation_alias='LOG_LEVEL')
    api_bind: str = Field(default='0.0.0.0', validation_alias='API_BIND')  # noqa: S104
    api_port: int = Field(default=8091, validation_alias='API_PORT')
    # Comma-separated. Only needed by separate front ends such as the Live2D
    # viewer; the bundled UI is served same-origin. Applied at startup.
    api_cors_origins: str = Field(default='', validation_alias='API_CORS_ORIGINS')
    api_cors_allow_credentials: bool = Field(default=False, validation_alias='API_CORS_ALLOW_CREDENTIALS')

    model_config = SettingsConfigDict(env_file='.env', extra='ignore')

    @property
    def cors_origins(self) -> tuple[str, ...]:
        origins: list[str] = []
        seen: set[str] = set()
        for item in self.api_cors_origins.split(','):
            origin = item.strip().rstrip('/')
            if not origin or origin in seen:
                continue
            origins.append(origin)
            seen.add(origin)
        return tuple(origins)

    @field_validator('postgres_dsn')
    @classmethod
    def normalize_postgres_dsn(cls, value: str) -> str:
        normalized = value.strip()
        if not normalized:
            msg = 'POSTGRES_DSN cannot be empty'
            raise ValueError(msg)
        return normalized

    @field_validator('api_token')
    @classmethod
    def normalize_api_token(cls, value: str) -> str:
        normalized = value.strip()
        if not normalized:
            # The settings API is the only way to configure this deployment, so an
            # empty token would leave a freshly provisioned instance world-writable.
            msg = 'API_TOKEN cannot be empty'
            raise ValueError(msg)
        return normalized

    @field_validator('api_bind')
    @classmethod
    def normalize_api_bind(cls, value: str) -> str:
        return value.strip() or '0.0.0.0'  # noqa: S104

    @field_validator('log_level')
    @classmethod
    def normalize_log_level(cls, value: str) -> str:
        # Loud about a typo rather than quietly running at INFO: this is reached for
        # when something needs diagnosing, which is the worst time to silence it.
        normalized = value.strip().upper() or 'INFO'
        if normalized not in LOG_LEVELS:
            msg = f'LOG_LEVEL must be one of: {", ".join(LOG_LEVELS)}'
            raise ValueError(msg)
        return normalized

    @field_validator('api_port')
    @classmethod
    def validate_api_port(cls, value: int) -> int:
        if not (0 < value <= MAX_PORT):
            msg = f'API_PORT must be between 1 and {MAX_PORT}'
            raise ValueError(msg)
        return value


env = Env()

"""`src/core/env.py`: the bootstrap values read from the environment and `.env` (WP1).

Each case sets exactly the given variables (every other variable `Env` knows is removed
for the call) and records the parsed values, or pydantic's error list. The `.env` cases
write the file to a temporary directory and read it with no variables set.
"""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from typing import Any

from pydantic import ValidationError
from src.core.env import Env

NAMES = (
    'POSTGRES_DSN',
    'API_TOKEN',
    'LOG_LEVEL',
    'API_BIND',
    'API_PORT',
    'API_CORS_ORIGINS',
    'API_CORS_ALLOW_CREDENTIALS',
    'CREDENTIAL_CLIENTS',
)
BASE = {'POSTGRES_DSN': 'postgresql://db.local/fav', 'API_TOKEN': 'abc123'}

ENV_CASES: list[dict[str, str]] = [
    {},
    BASE,
    {'POSTGRES_DSN': '  postgresql://user:pass@db.local/fav  ', 'API_TOKEN': ' abc123 '},
    {'API_TOKEN': 'abc123'},
    {'POSTGRES_DSN': 'postgresql://db.local/fav'},
    {'POSTGRES_DSN': '   ', 'API_TOKEN': 'abc123'},
    {'POSTGRES_DSN': 'postgresql://db.local/fav', 'API_TOKEN': '   '},
    {'POSTGRES_DSN': '', 'API_TOKEN': ''},
    {**BASE, 'API_BIND': '127.0.0.1', 'API_PORT': '9000'},
    {**BASE, 'API_BIND': '   '},
    {**BASE, 'API_PORT': '70000'},
    {**BASE, 'API_PORT': '0'},
    {**BASE, 'API_PORT': '65535'},
    {**BASE, 'API_PORT': ' 8_080 '},
    {**BASE, 'API_PORT': 'http'},
    {**BASE, 'API_PORT': ''},
    {**BASE, 'LOG_LEVEL': 'debug'},
    {**BASE, 'LOG_LEVEL': ' notice '},
    {**BASE, 'LOG_LEVEL': ''},
    {**BASE, 'LOG_LEVEL': 'verbose'},
    {**BASE, 'LOG_LEVEL': 'warn'},
    {**BASE, 'API_CORS_ORIGINS': ' https://a.example/, https://b.example ,, https://a.example '},
    {**BASE, 'API_CORS_ORIGINS': 'https://a.example//'},
    {**BASE, 'API_CORS_ALLOW_CREDENTIALS': 'true'},
    {**BASE, 'API_CORS_ALLOW_CREDENTIALS': '1'},
    {**BASE, 'API_CORS_ALLOW_CREDENTIALS': 'off'},
    {**BASE, 'API_CORS_ALLOW_CREDENTIALS': 'maybe'},
    {**BASE, 'API_CORS_ALLOW_CREDENTIALS': ''},
    {**BASE, 'CREDENTIAL_CLIENTS': 'rsshub:abc:bilibili'},
    {'postgres_dsn': 'postgresql://lower/fav', 'Api_Token': 'mixed'},
    {**BASE, 'API_PORT': '-1', 'LOG_LEVEL': 'x', 'API_CORS_ALLOW_CREDENTIALS': 'x'},
]

DOTENV_CASES: list[str] = [
    'POSTGRES_DSN=postgresql://file/fav\nAPI_TOKEN=from-file\n',
    '# comment\nexport POSTGRES_DSN="postgresql://quoted/fav"\nAPI_TOKEN=\'single quoted\'\n\nAPI_PORT=9001 # trailing comment\n',
    'POSTGRES_DSN = postgresql://spaced/fav \nAPI_TOKEN="a#b"\nAPI_BIND=host#notcomment\nAPI_CORS_ORIGINS="https://a\\nhttps://b"\n',
    'POSTGRES_DSN=postgresql://x/fav\nAPI_TOKEN=\'lit\\nral\'\nLOG_LEVEL="debug" # c\nUNRELATED=1\n',
    'POSTGRES_DSN=postgresql://x/fav\nAPI_TOKEN=first\nAPI_TOKEN=second\n',
    'POSTGRES_DSN=postgresql://x/fav\nAPI_TOKEN=\n',
    'POSTGRES_DSN=postgresql://x/fav\nAPI_TOKEN\n',
    'postgres_dsn=postgresql://lower/fav\napi_token=lower\n',
    'POSTGRES_DSN="multi\nline"\nAPI_TOKEN=t\n',
    'POSTGRES_DSN=postgresql://x/fav\r\nAPI_TOKEN=crlf\r\n',
    'not a valid line\nPOSTGRES_DSN=postgresql://x/fav\nAPI_TOKEN=t\n',
]


def _result(build: Any) -> dict[str, Any]:
    try:
        env = build()
    except ValidationError as exc:
        return {'errors': [{key: error[key] for key in ('type', 'loc', 'msg', 'input')} for error in json.loads(exc.json())]}
    return {'value': env.model_dump(), 'cors_origins': list(env.cors_origins)}


def _with_environ(variables: dict[str, str], build: Any) -> dict[str, Any]:
    saved = dict(os.environ)
    try:
        for name in list(os.environ):
            if name.upper() in NAMES:
                del os.environ[name]
        os.environ.update(variables)
        return _result(build)
    finally:
        os.environ.clear()
        os.environ.update(saved)


def _env_cases() -> list[dict[str, Any]]:
    return [{'environ': case, **_with_environ(case, lambda: Env(_env_file=None))} for case in ENV_CASES]


def _dotenv_cases() -> list[dict[str, Any]]:
    cases = []
    for text in DOTENV_CASES:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / '.env'
            path.write_bytes(text.encode('utf-8'))
            cases.append({'dotenv': text, **_with_environ({}, lambda path=path: Env(_env_file=path))})
    # Variables win over the file.
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / '.env'
        text = 'POSTGRES_DSN=postgresql://file/fav\nAPI_TOKEN=from-file\nAPI_PORT=1\n'
        path.write_text(text, encoding='utf-8')
        variables = {'API_TOKEN': 'from-environ'}
        cases.append({'dotenv': text, 'environ': variables, **_with_environ(variables, lambda: Env(_env_file=path))})
    return cases


GOLDENS = {
    'env/environ.json': _env_cases,
    'env/dotenv.json': _dotenv_cases,
}

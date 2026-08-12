# ruff: noqa: INP001

import os

# Settings now live in PostgreSQL, and src.core.env refuses to import without
# bootstrap values. Set them before anything imports src.* so the suite keeps
# running without a database or a .env file.
os.environ.setdefault('POSTGRES_DSN', 'postgresql://tests@127.0.0.1:5432/fav-tests')
os.environ.setdefault('API_TOKEN', 'token-for-tests')

import pytest

from src.core import settings


@pytest.fixture(autouse=True)
def pinned_settings() -> object:
    """Pin a defaults-only settings snapshot so no test ever queries PostgreSQL.

    Tests that need specific values mutate the yielded object (it is the exact
    instance ``settings.load()`` returns) or call ``settings.use`` themselves.
    """
    snapshot = settings.Settings()
    settings.use(snapshot)
    try:
        yield snapshot
    finally:
        settings.use(None)

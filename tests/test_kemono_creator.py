# ruff: noqa: ANN001, ARG005, EM101, INP001, S101

import httpx
import pytest

from src.tool.kemono_creator import KemonoCreatorResolver, extract_kemono_creator


@pytest.mark.parametrize(
    ('raw', 'expected'),
    [
        ('70050825', ('fanbox', '70050825')),
        ('  70050825  ', ('fanbox', '70050825')),
        ('https://pawchive.pw/fanbox/user/70050825', ('fanbox', '70050825')),
        ('https://pawchive.pw/fanbox/user/70050825?o=50', ('fanbox', '70050825')),
        ('https://kemono.cr/patreon/user/9069743/posts', ('patreon', '9069743')),
        ('https://pawchive.pw/Fantia/user/12345', ('fantia', '12345')),
        ('fanbox/user/70050825', ('fanbox', '70050825')),
        ('https://coomer.st/onlyfans/user/some-creator_1', ('onlyfans', 'some-creator_1')),
    ],
)
def test_extract_kemono_creator_accepts_urls_and_bare_ids(raw, expected) -> None:
    assert extract_kemono_creator(raw) == expected


@pytest.mark.parametrize(
    'raw',
    [
        '',
        '   ',
        '0',
        'maplestar',
        'https://pawchive.pw/posts',
        'https://pawchive.pw/user/70050825',
        'https://pawchive.pw/fanbox/user/',
        'https://pawchive.pw/fanbox/user/id with spaces',
    ],
)
def test_extract_kemono_creator_rejects_invalid_input(raw) -> None:
    assert extract_kemono_creator(raw) is None


def _resolver(handler) -> KemonoCreatorResolver:
    return KemonoCreatorResolver(
        base_url_provider=lambda: 'https://pawchive.pw/',
        client=httpx.Client(transport=httpx.MockTransport(handler)),
    )


def test_resolve_fetches_name_from_profile_api() -> None:
    seen: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(str(request.url))
        return httpx.Response(200, json={'id': '70050825', 'name': 'Maplestar', 'service': 'fanbox'})

    resolver = _resolver(handler)
    resolved = resolver.resolve('https://pawchive.pw/fanbox/user/70050825')

    assert resolved == {'service': 'fanbox', 'id': '70050825', 'name': 'Maplestar'}
    assert seen == ['https://pawchive.pw/api/v1/fanbox/user/70050825/profile']


def test_resolve_falls_back_to_id_when_name_is_blank() -> None:
    resolver = _resolver(lambda request: httpx.Response(200, json={'name': '  '}))
    assert resolver.resolve('70050825')['name'] == '70050825'


def test_resolve_rejects_unparseable_input() -> None:
    resolver = _resolver(lambda request: httpx.Response(200, json={}))
    with pytest.raises(ValueError, match='invalid_creator'):
        resolver.resolve('not a creator')


def test_resolve_maps_missing_creator_to_lookup_error() -> None:
    resolver = _resolver(lambda request: httpx.Response(404))
    with pytest.raises(LookupError, match='creator_not_found'):
        resolver.resolve('70050825')


@pytest.mark.parametrize(
    'handler',
    [
        lambda request: httpx.Response(403),
        lambda request: httpx.Response(200, text='<html>not json</html>'),
    ],
)
def test_resolve_maps_upstream_failures_to_connection_error(handler) -> None:
    resolver = _resolver(handler)
    with pytest.raises(ConnectionError, match='creator_resolve_failed'):
        resolver.resolve('70050825')


def test_resolve_maps_transport_errors_to_connection_error() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError('boom', request=request)

    resolver = _resolver(handler)
    with pytest.raises(ConnectionError, match='creator_resolve_failed'):
        resolver.resolve('70050825')

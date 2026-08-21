# ruff: noqa: INP001, S101, ANN001

import httpx
import pytest

from src.tool.hanime1_author import (
    Hanime1AuthorService,
    Hanime1AuthorVideoEntry,
    extract_author_display_name,
    extract_author_video_entries,
    extract_hanime1_author_id,
)

_AUTHOR_PAGE_HTML = """
<html>
  <head><title>Maplestar - 影片 - H動漫/裏番/線上看 - Hanime1.me</title></head>
  <body>
    <h1 class="profile-display-name">Maplestar</h1>
    <a href="https://hanime1.me/watch?v=407657" class="video-link">
      <div class="thumb-container">
        <img class="main-thumb" src="https://cdn.example.com/407657l.jpg">
        <div class="duration">03:14</div>
      </div>
      <div class="title">SAKAMOTO DAYS FULL ANIMATION</div>
    </a>
    <a class="video-link" href="/watch?v=406852">
      <div class="title">Reze (Chainsaw Man) - Animation</div>
    </a>
  </body>
</html>
"""


def test_extract_author_display_name_prefers_h1() -> None:
    assert extract_author_display_name(_AUTHOR_PAGE_HTML) == 'Maplestar'


def test_extract_author_display_name_falls_back_to_page_title() -> None:
    page_html = '<html><head><title>Maplestar - 影片 - H動漫/裏番/線上看 - Hanime1.me</title></head><body></body></html>'
    assert extract_author_display_name(page_html) == 'Maplestar'


def test_extract_author_display_name_returns_none_when_missing() -> None:
    assert extract_author_display_name('<html><head><title>搜尋 - Hanime1.me</title></head></html>') is None
    assert extract_author_display_name('') is None


def test_extract_author_video_entries_parses_cards() -> None:
    entries = extract_author_video_entries(_AUTHOR_PAGE_HTML)

    assert entries == [
        Hanime1AuthorVideoEntry(video_id='407657', title='SAKAMOTO DAYS FULL ANIMATION'),
        Hanime1AuthorVideoEntry(video_id='406852', title='Reze (Chainsaw Man) - Animation'),
    ]


def test_extract_author_video_entries_handles_attribute_order_and_missing_title() -> None:
    page_html = """
    <a data-x="1" class="card video-link extra" href="https://hanime1.me/watch?v=100">
      <div class="thumb-container"></div>
    </a>
    <a href="/watch?v=100" class="video-link"><div class="title">dup</div></a>
    <a href="/watch?v=abc" class="video-link"><div class="title">bad id</div></a>
    <a href="/playlist?list=1" class="video-link"><div class="title">not a watch link</div></a>
    """

    entries = extract_author_video_entries(page_html)

    assert entries == [Hanime1AuthorVideoEntry(video_id='100', title=None)]


def test_extract_author_video_entries_ignores_plain_links() -> None:
    assert extract_author_video_entries('<a href="/watch?v=407657">no video-link class</a>') == []


@pytest.mark.parametrize(
    ('raw', 'expected'),
    [
        ('202534', '202534'),
        (' 202534 ', '202534'),
        ('https://hanime1.me/user/202534/uploaded', '202534'),
        ('https://hanime1.me/user/202534/uploaded?page=2', '202534'),
        ('https://hanime1.me/user/202534', '202534'),
        ('/user/202534/uploaded', '202534'),
        ('0', None),
        ('-5', None),
        ('https://hanime1.me/watch?v=202534', None),
        ('garbage', None),
        ('', None),
    ],
)
def test_extract_hanime1_author_id(raw: str, expected: str | None) -> None:
    assert extract_hanime1_author_id(raw) == expected


def _make_service(handler) -> tuple[Hanime1AuthorService, httpx.Client]:
    client = httpx.Client(transport=httpx.MockTransport(handler))
    service = Hanime1AuthorService(
        dsn='postgresql://unused',
        host='https://hanime1.me',
        user_lang='zhs',
        client=client,
    )
    return service, client


def test_author_service_adds_author_from_profile_page(monkeypatch) -> None:
    requested: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requested.append(str(request.url))
        return httpx.Response(200, text=_AUTHOR_PAGE_HTML)

    service, client = _make_service(handler)
    inserted: dict[str, str] = {}
    monkeypatch.setattr(service, '_insert_author', lambda **payload: inserted.update(payload))

    created = service.add_author('https://hanime1.me/user/202534/uploaded')

    assert created == {
        'author_id': '202534',
        'name': 'Maplestar',
        'author_url': 'https://hanime1.me/user/202534/uploaded',
    }
    assert inserted == {'author_id': '202534', 'name': 'Maplestar'}
    assert requested == ['https://hanime1.me/user/202534/uploaded']
    client.close()


def test_author_service_rejects_invalid_input() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        msg = 'no request expected'
        raise AssertionError(msg)

    service, client = _make_service(handler)

    with pytest.raises(ValueError, match='invalid_author'):
        service.add_author('not-an-author')
    client.close()


def test_author_service_raises_lookup_error_when_name_unresolvable(monkeypatch) -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text='<html><head><title>搜尋 - Hanime1.me</title></head></html>')

    service, client = _make_service(handler)
    monkeypatch.setattr(service, '_insert_author', lambda **_payload: pytest.fail('unresolved author was inserted'))

    with pytest.raises(LookupError, match='author_resolve_failed'):
        service.add_author('202534')
    client.close()


def test_author_service_raises_lookup_error_on_http_error(monkeypatch) -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(404, text='not found')

    service, client = _make_service(handler)
    monkeypatch.setattr(service, '_insert_author', lambda **_payload: pytest.fail('unresolved author was inserted'))

    with pytest.raises(LookupError, match='author_resolve_failed'):
        service.add_author('202534')
    client.close()


def test_author_service_raises_lookup_error_when_blocked_by_cloudflare(monkeypatch) -> None:
    blocked_html = '<html><title>Attention Required! | Cloudflare</title><div id="cf-error-details"></div></html>'

    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text=blocked_html)

    service, client = _make_service(handler)
    monkeypatch.setattr(service, '_insert_author', lambda **_payload: pytest.fail('blocked author was inserted'))

    with pytest.raises(LookupError, match='author_resolve_failed'):
        service.add_author('202534')
    client.close()


def test_author_service_converts_display_name_to_simplified(monkeypatch) -> None:
    page_html = '<h1 class="profile-display-name">動畫社團</h1>'

    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text=page_html)

    service, client = _make_service(handler)
    inserted: dict[str, str] = {}
    monkeypatch.setattr(service, '_insert_author', lambda **payload: inserted.update(payload))

    created = service.add_author('202534')

    assert created['name'] == '动画社团'
    assert inserted['name'] == '动画社团'
    client.close()


def test_author_service_delete_rejects_invalid_id() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        msg = 'no request expected'
        raise AssertionError(msg)

    service, client = _make_service(handler)

    with pytest.raises(ValueError, match='invalid_author'):
        service.delete_author('abc')
    with pytest.raises(ValueError, match='invalid_author'):
        service.delete_author('0')
    client.close()

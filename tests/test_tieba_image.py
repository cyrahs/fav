# ruff: noqa: INP001, S101, ANN001

import asyncio

import httpx
import pytest

from src.tool import tieba_image
from src.tool.tieba_image import TiebaImageUnavailableError

SHARED = (
    'https://tiebapic.baidu.com/forum/w%3D1080%3Bq%3D90/sign=6eb1ec28c101a18bf0eb164fa61f3c79/'
    '96345d3d269759eed8080acff4fb43166c22dfad.jpg?tbpicau=2026-09-25-05_2daa83af8c7c63168704575e8ee9e72b&naImageProxy=1'
)
ORIGINAL = (
    'https://tiebapic.baidu.com/forum/pic/item/96345d3d269759eed8080acff4fb43166c22dfad.jpg'
    '?tbpicau=2026-09-25-05_2daa83af8c7c63168704575e8ee9e72b&naImageProxy=1'
)
REFUSED_HEADERS = {'Content-Type': 'image/jpeg', 'Error-Message': 'img not auth'}


def test_extract_finds_links_inside_chinese_text_and_entities() -> None:
    text = f'看这张{SHARED}好看。 https://example.com/a.jpg {SHARED}'
    hidden = 'https://imgsa.baidu.com/forum/pic/item/abc.jpg'

    urls = tieba_image.extract_tieba_image_urls(text, [hidden, 'https://tieba.baidu.com/p/1'])

    assert urls == [SHARED, hidden]


def test_extract_ignores_other_baidu_paths_and_trailing_punctuation() -> None:
    assert tieba_image.extract_tieba_image_urls('(https://tiebapic.baidu.com/forum/pic/item/a.jpg).') == [
        'https://tiebapic.baidu.com/forum/pic/item/a.jpg',
    ]
    assert tieba_image.extract_tieba_image_urls('https://tiebapic.baidu.com/other/a.jpg https://baidu.com/forum/a.jpg') == []


def test_strip_leaves_the_text_around_the_link() -> None:
    assert tieba_image.strip_tieba_image_urls(f'  {SHARED}\n  ') == ''
    assert tieba_image.strip_tieba_image_urls(f'风景 {SHARED} https://example.com/x') == '风景 https://example.com/x'


def test_original_image_url_drops_the_resize_segment_and_keeps_the_grant() -> None:
    assert tieba_image.original_image_url(SHARED) == ORIGINAL
    assert tieba_image.original_image_url(ORIGINAL) == ORIGINAL


def _client(handler) -> httpx.AsyncClient:
    return httpx.AsyncClient(transport=httpx.MockTransport(handler))


async def _fetch(handler, url: str = SHARED) -> tieba_image.TiebaImage:
    async with _client(handler) as client:
        return await tieba_image.fetch_tieba_image(client, url)


def test_fetch_prefers_the_original_upload() -> None:
    requested: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        requested.append(str(request.url))
        return httpx.Response(200, headers={'Content-Type': 'image/png'}, content=b'original')

    image = asyncio.run(_fetch(handler))

    assert image.content == b'original'
    assert image.extension == '.png'
    assert requested == [ORIGINAL]


def test_fetch_falls_back_to_the_shared_link_when_the_original_is_refused() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        if '/pic/item/' in request.url.path:
            return httpx.Response(200, headers=REFUSED_HEADERS, content=b'logo')
        return httpx.Response(200, headers={'Content-Type': 'image/jpeg'}, content=b'resized')

    image = asyncio.run(_fetch(handler))

    assert image.content == b'resized'
    assert image.extension == '.jpg'


def test_fetch_reports_an_expired_link() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, headers=REFUSED_HEADERS, content=b'logo')

    with pytest.raises(TiebaImageUnavailableError, match='img not auth'):
        asyncio.run(_fetch(handler))


def test_fetch_rejects_a_non_image_answer() -> None:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, headers={'Content-Type': 'text/html'}, content=b'<html>')

    with pytest.raises(TiebaImageUnavailableError, match='text/html'):
        asyncio.run(_fetch(handler, ORIGINAL))

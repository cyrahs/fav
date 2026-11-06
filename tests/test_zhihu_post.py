# ruff: noqa: INP001, S101, S106, ANN001, ANN202, EM101, PLR2004, RUF001, TRY003

import asyncio
import json

import httpx
import pytest

from src.web import zhihu_post
from src.web.zhihu_post import ZhihuAccountError, ZhihuError, ZhihuLink, ZhihuVideo

# Trimmed from a real fetch_answer_detail answer: each picture appears twice (a
# <noscript> copy and the lazy-loaded one), and a video box carries a poster image.
_ANSWER_HTML = (
    '<p data-pid="a">黑丝或者肉丝叭</p>'
    '<figure data-size="normal"><noscript><img src="https://pic1.zhimg.com/50/v2-aaa_720w.jpg?source=2c26e567" '
    'data-original="https://pica.zhimg.com/v2-aaa_r.jpg?source=2c26e567"/></noscript>'
    '<img src="data:image/svg+xml;utf8,&lt;svg&gt;" class="origin_image lazy" '
    'data-original="https://pica.zhimg.com/v2-aaa_r.jpg?source=2c26e567" '
    'data-actualsrc="https://pic1.zhimg.com/50/v2-aaa_720w.jpg?source=2c26e567"/></figure>'
    '<figure><img src="https://picx.zhimg.com/v2-bbb_720w.jpg"/></figure>'
    '<img src="https://www.zhihu.com/equation?tex=x"/>'
    '<a class="video-box" href="https://link.zhihu.com/?target=https%3A//www.zhihu.com/video/2091194612627121396" '
    'target="_blank" data-poster="https://pica.zhimg.com/v2-poster.jpg?source=ccfced1a" data-lens-id="2091194612627121396">'
    '<img class="thumbnail" src="https://pica.zhimg.com/v2-poster.jpg?source=ccfced1a"/></a>'
)


@pytest.mark.parametrize(
    ('url', 'expected'),
    [
        ('https://www.zhihu.com/question/1981023166554989468/answer/2100311203096573240', ZhihuLink('answer', '2100311203096573240')),
        ('https://www.zhihu.com/question/1/answer/2/?share_code=abc&utm_psn=1', ZhihuLink('answer', '2')),
        ('https://www.zhihu.com/answer/2100311203096573240', ZhihuLink('answer', '2100311203096573240')),
        ('https://zhuanlan.zhihu.com/p/669214677', ZhihuLink('article', '669214677')),
        ('http://www.zhihu.com/pin/2100667138746983678', ZhihuLink('pin', '2100667138746983678')),
        ('https://link.zhihu.com/?target=https%3A//zhuanlan.zhihu.com/p/42', ZhihuLink('article', '42')),
        ('https://www.zhihu.com/question/1981023166554989468', None),
        ('https://zhihu.com.evil.example/answer/1', None),
        ('https://example.com/answer/1', None),
    ],
)
def test_parse_zhihu_url(url, expected) -> None:
    assert zhihu_post.parse_zhihu_url(url) == expected


def test_find_zhihu_link_reads_share_text_and_preview() -> None:
    share = '高跟鞋配白丝真的很难看吗？ - 知乎用户mYQRnd的回答 - 知乎\nhttps://www.zhihu.com/question/1981023166554989468/answer/2100311203096573240'
    assert zhihu_post.find_zhihu_link(share) == ZhihuLink('answer', '2100311203096573240')
    assert zhihu_post.find_zhihu_link('看这个（https://zhuanlan.zhihu.com/p/42）') == ZhihuLink('article', '42')
    assert zhihu_post.find_zhihu_link('no link here', 'https://www.zhihu.com/pin/7') == ZhihuLink('pin', '7')
    assert zhihu_post.find_zhihu_link('https://www.bilibili.com/video/BV1xx') is None


def test_answer_content_keeps_full_size_pictures_once_and_skips_video_posters() -> None:
    assert zhihu_post.content_images(_ANSWER_HTML) == [
        'https://pica.zhimg.com/v2-aaa_r.jpg?source=2c26e567',
        'https://picx.zhimg.com/v2-bbb_720w.jpg',
    ]
    assert zhihu_post.content_videos(_ANSWER_HTML) == [
        ZhihuVideo(id='2091194612627121396', cover_url='https://pica.zhimg.com/v2-poster.jpg?source=ccfced1a'),
    ]


def test_parse_answer_uses_question_title_and_author() -> None:
    link = ZhihuLink('answer', '2100311203096573240')
    post = zhihu_post.parse_post(
        link,
        {
            'content': _ANSWER_HTML,
            'author': {'name': '知乎用户mYQRnd'},
            'created_time': 1793533212,
            'question': {'title': '高跟鞋配白丝真的很难看吗？'},
        },
    )
    assert post.title == '高跟鞋配白丝真的很难看吗？'
    assert post.author == '知乎用户mYQRnd'
    assert post.published_at is not None
    assert len(post.images) == 2
    assert [video.id for video in post.videos] == ['2091194612627121396']


def test_parse_pin_reads_items() -> None:
    link = ZhihuLink('pin', '2100667138746983678')
    post = zhihu_post.parse_post(
        link,
        {
            'author': {'name': 'someone'},
            'created': 1793533212,
            'excerpt_title': '穿搭小技巧 | <a class="hash_tag" href="https://www.zhihu.com/topic/1">#大衣穿搭</a>',
            'content': [
                {'type': 'text', 'own_text': '穿搭小技巧 | <a class="hash_tag" href="https://www.zhihu.com/topic/1">#大衣穿搭</a>'},
                {
                    'type': 'image',
                    'url': 'https://pic1.zhimg.com/100/v2-wm_720w.jpg',
                    'original_url': 'https://picx.zhimg.com/v2-orig.png?source=e3d01f54',
                },
                {
                    'type': 'video',
                    'video_id': '2100665385288836251',
                    'thumbnail': 'https://pic1.zhimg.com/80/v2-thumb',
                    'video_info': {
                        'playlist': [
                            {'quality': 'ld', 'width': 476, 'height': 848, 'url': 'https://vdn3.vzuu.com/SD/x.mp4'},
                            {'quality': 'hd', 'width': 720, 'height': 1280, 'url': 'https://vdn3.vzuu.com/HD/x.mp4'},
                        ],
                    },
                },
            ],
        },
    )
    assert post.title == '穿搭小技巧 | #大衣穿搭'
    assert post.images == ['https://picx.zhimg.com/v2-orig.png?source=e3d01f54']
    assert post.videos == [
        ZhihuVideo(id='2100665385288836251', fallback_url='https://vdn3.vzuu.com/HD/x.mp4', cover_url='https://pic1.zhimg.com/80/v2-thumb'),
    ]


def test_fetch_post_calls_the_endpoint_for_the_kind() -> None:
    requests: list[httpx.Request] = []

    def _handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return httpx.Response(200, json={'code': 200, 'data': {'title': 'Article', 'content': _ANSWER_HTML, 'author': {'name': 'a'}}})

    async def _run():
        async with httpx.AsyncClient(transport=httpx.MockTransport(_handler)) as client:
            return await zhihu_post.fetch_post(client, ZhihuLink('article', '42'), api_url='https://api.tikhub.io/', token='key')

    post = asyncio.run(_run())
    assert post.title == 'Article'
    assert requests[0].url.path == '/api/v1/zhihu/web/fetch_column_article_detail'
    assert requests[0].url.params['article_id'] == '42'
    assert requests[0].headers['Authorization'] == 'Bearer key'


@pytest.mark.parametrize(
    ('status', 'body', 'error'),
    [
        (402, {'detail': 'balance'}, ZhihuAccountError),
        (400, {'detail': {'message': 'not found'}}, ZhihuError),
        (200, {'code': 200, 'data': {}}, ZhihuError),
    ],
)
def test_fetch_post_failures(status, body, error) -> None:
    async def _run():
        transport = httpx.MockTransport(lambda _request: httpx.Response(status, json=body))
        async with httpx.AsyncClient(transport=transport) as client:
            await zhihu_post.fetch_post(client, ZhihuLink('answer', '1'), api_url='https://api.tikhub.io', token='key')

    with pytest.raises(error):
        asyncio.run(_run())


def test_fetch_post_without_key_makes_no_request() -> None:
    def _handler(_request: httpx.Request) -> httpx.Response:
        raise AssertionError('no request without a key')

    async def _run():
        async with httpx.AsyncClient(transport=httpx.MockTransport(_handler)) as client:
            await zhihu_post.fetch_post(client, ZhihuLink('answer', '1'), api_url='https://api.tikhub.io', token='')

    with pytest.raises(ZhihuAccountError):
        asyncio.run(_run())


def test_resolve_video_takes_widest_lens_entry_and_falls_back() -> None:
    lens = {
        'playlist': {
            'LD': {'width': 476, 'height': 848, 'bitrate': 379.5, 'play_url': 'https://vdn3.vzuu.com/SD/v.mp4'},
            'HD': {'width': 720, 'height': 1280, 'bitrate': 559.1, 'play_url': 'https://vdn3.vzuu.com/HD/v.mp4'},
        },
    }

    def _handler(request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith('/1'):
            return httpx.Response(200, text=json.dumps(lens))
        return httpx.Response(403, json={'error': {'code': 40362}})

    async def _run(video: ZhihuVideo) -> str:
        async with httpx.AsyncClient(transport=httpx.MockTransport(_handler)) as client:
            return await zhihu_post.resolve_video(client, video)

    assert asyncio.run(_run(ZhihuVideo(id='1'))) == 'https://vdn3.vzuu.com/HD/v.mp4'
    assert asyncio.run(_run(ZhihuVideo(id='2', fallback_url='https://vdn3.vzuu.com/HD/p.mp4'))) == 'https://vdn3.vzuu.com/HD/p.mp4'
    assert asyncio.run(_run(ZhihuVideo(id='2'))) == ''

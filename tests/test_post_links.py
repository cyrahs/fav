# ruff: noqa: INP001, S101, ANN001, ANN202, RUF001

import asyncio

import httpx
import pytest

from src.core import settings
from src.web import post_links
from src.web.post_links import PostCaptchaError, PostLinkError

_ZHIHU_CARD = """<msg><appmsg appid="" sdkver="0">
<title>高跟鞋配白丝真的很难看吗？</title><des>知乎用户的回答</des><type>5</type>
<url>https://www.zhihu.com/question/1981023166554989468/answer/2100311203096573240?share_code=x&amp;utm_psn=1</url>
</appmsg></msg>"""


@pytest.mark.parametrize(
    ('url', 'expected'),
    [
        ('https://tieba.baidu.com/p/9876543210?share=9', 'https://tieba.baidu.com/p/9876543210'),
        ('https://bbs.nga.cn/read.php?tid=12345&page=2', 'https://bbs.nga.cn/read.php?tid=12345'),
        ('https://ngabbs.com/read.php?tid=12345', 'https://bbs.nga.cn/read.php?tid=12345'),
        ('https://zhuanlan.zhihu.com/p/42?utm=1', 'https://zhuanlan.zhihu.com/p/42'),
        ('https://www.zhihu.com/pin/7', 'https://www.zhihu.com/pin/7'),
        ('http://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b&chksm=c', 'https://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b&chksm=c'),
        ('https://www.bilibili.com/video/BV1xx', None),
        ('ftp://tieba.baidu.com/p/1', None),
    ],
)
def test_link_for_url_names_each_site_canonically(url, expected) -> None:
    link = post_links.link_for_url(url)
    assert (link.url if link else None) == expected


def test_find_link_reads_text_then_preview() -> None:
    assert post_links.find_link('看这个（https://zhuanlan.zhihu.com/p/42）').url == 'https://zhuanlan.zhihu.com/p/42'
    assert post_links.find_link('nothing here', 'https://tieba.baidu.com/p/5').url == 'https://tieba.baidu.com/p/5'
    assert post_links.find_link('https://example.com/a', '') is None


def test_a_wechat_card_for_a_zhihu_answer_is_a_shared_link() -> None:
    link = post_links.shared_link(_ZHIHU_CARD)
    assert link is not None
    assert link.url == 'https://www.zhihu.com/answer/2100311203096573240'
    assert link.title == '高跟鞋配白丝真的很难看吗？'
    assert post_links.site_name(link) == 'Zhihu'


def _run_fetch(link, handler):
    async def _run():
        async with httpx.AsyncClient(transport=httpx.MockTransport(handler), follow_redirects=True) as client:
            return await post_links.fetch_page(client, link)

    return asyncio.run(_run())


def test_a_zhihu_post_is_read_with_the_tikhub_service_key() -> None:
    tikhub = settings.load().services.tikhub
    tikhub.api_url, tikhub.token = 'https://api.tikhub.dev', 'service-key'
    requests: list[httpx.Request] = []

    def _handler(request: httpx.Request) -> httpx.Response:
        requests.append(request)
        return httpx.Response(200, json={'data': {'title': 'T', 'content': '<img data-original="https://pic1.zhimg.com/v2-a_r.jpg"/>'}})

    page, html = _run_fetch(post_links.link_for_url('https://zhuanlan.zhihu.com/p/42'), _handler)
    assert (page.title, page.images, html) == ('T', ['https://pic1.zhimg.com/v2-a_r.jpg'], '')
    assert requests[0].url.host == 'api.tikhub.dev'
    assert requests[0].headers['authorization'] == 'Bearer service-key'


def test_a_zhihu_post_without_a_tikhub_key_waits_for_one() -> None:
    settings.load().services.tikhub.token = ''
    with pytest.raises(PostLinkError) as caught:
        _run_fetch(post_links.link_for_url('https://www.zhihu.com/pin/7'), lambda _request: httpx.Response(500))
    assert caught.value.needs == 'tikhub'


def test_an_nga_thread_without_a_login_waits_for_one(monkeypatch) -> None:
    class _Sessions:
        async def load(self, _site):
            return None

    monkeypatch.setattr(post_links, 'site_sessions', _Sessions())
    with pytest.raises(PostLinkError) as caught:
        _run_fetch(post_links.link_for_url('https://bbs.nga.cn/read.php?tid=1'), lambda _request: httpx.Response(500))
    assert caught.value.needs == 'nga-login'


def test_a_wechat_image_post_behind_the_captcha_is_its_own_error() -> None:
    def _handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == '/s':
            return httpx.Response(302, headers={'location': 'https://mp.weixin.qq.com/mp/wappoc_appmsgcaptcha?x=1'})
        return httpx.Response(200, text='captcha')

    with pytest.raises(PostCaptchaError):
        _run_fetch(post_links.link_for_url('https://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b'), _handler)


def test_fetch_image_skips_tiebas_expired_grant_logo() -> None:
    def _handler(request: httpx.Request) -> httpx.Response:
        if 'expired' in str(request.url):
            return httpx.Response(200, content=b'\x89PNGlogo', headers={'content-type': 'image/png', 'error-message': 'expired'})
        return httpx.Response(200, content=b'\xff\xd8\xffjpeg', headers={'content-type': 'image/jpeg'})

    async def _run(url: str):
        async with httpx.AsyncClient(transport=httpx.MockTransport(_handler)) as client:
            return await post_links.fetch_image(client, url)

    assert asyncio.run(_run('https://pic1.zhimg.com/v2-a_r.jpg')) == (b'\xff\xd8\xffjpeg', 'jpg')
    assert asyncio.run(_run('https://tiebapic.baidu.com/expired.jpg')) is None

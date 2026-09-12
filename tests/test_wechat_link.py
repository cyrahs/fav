# ruff: noqa: INP001, S101, S106, ANN001, ANN003, ANN202, ARG001, ARG005, PLR2004, SLF001, E501

import asyncio
import json
from pathlib import Path
from types import SimpleNamespace

import httpx

import src.web.wechat as wechat_module
from src.core.settings import WeChatAccount
from src.tool.wechat_link import download_link_images, extract_image_urls, sniff_image_extension
from src.tool.wechat_queue import WeChatMediaJob
from src.tool.wechat_webwx import parse_sync_message

_PNG = b'\x89PNG\r\n\x1a\n' + b'\x00' * 20000
_JPG = b'\xff\xd8\xff' + b'\x00' * 20000
_TINY = b'\xff\xd8\xff' + b'\x00' * 100

_MP_ARTICLE = """
<html><head><meta property="og:image" content="https://mmbiz.qpic.cn/mmbiz_jpg/cover/0?wx_fmt=jpeg" /></head>
<body><div id="js_content">
<p><img data-src="https://mmbiz.qpic.cn/mmbiz_jpg/abc/640?wx_fmt=jpeg&amp;tp=webp&amp;wxfrom=5&amp;wx_lazy=1" src="data:image/gif;base64,R0lGOD" /></p>
<p><img data-src="https://mmbiz.qpic.cn/mmbiz_png/def/640?wx_fmt=png" /></p>
<p><img data-src="https://mmbiz.qpic.cn/mmbiz_jpg/abc/640?wx_fmt=jpeg" /></p>
<img src="/static/icon.svg" />
</div></body></html>
"""


def test_extract_image_urls_normalizes_mp_article_pictures() -> None:
    urls = extract_image_urls(_MP_ARTICLE, 'https://mp.weixin.qq.com/s/xyz')

    assert urls == [
        'https://mmbiz.qpic.cn/mmbiz_jpg/abc/0?wx_fmt=jpeg',
        'https://mmbiz.qpic.cn/mmbiz_png/def/0?wx_fmt=png',
        'https://mmbiz.qpic.cn/mmbiz_jpg/cover/0?wx_fmt=jpeg',
    ]


def test_extract_image_urls_handles_picture_message_pages_and_generic_sites() -> None:
    picture_page = "<script>window.picture_page_info_list = [{cdn_url: 'https://mmbiz.qpic.cn/sz_mmbiz_jpg/p1/640?wx_fmt=jpeg'}];</script>"
    assert extract_image_urls(picture_page, 'https://mp.weixin.qq.com/s/pic') == ['https://mmbiz.qpic.cn/sz_mmbiz_jpg/p1/0?wx_fmt=jpeg']

    generic = '<img src="img/a.jpg"><img src="//cdn.example/b.png"><img src="javascript:void(0)"><meta content="https://x.example/og.jpg" property="og:image">'
    assert extract_image_urls(generic, 'https://site.example/post/1') == [
        'https://site.example/post/img/a.jpg',
        'https://cdn.example/b.png',
        'https://x.example/og.jpg',
    ]


def test_sniff_image_extension_falls_back_to_the_content_type() -> None:
    assert sniff_image_extension(_PNG[:16]) == 'png'
    assert sniff_image_extension(b'RIFF\x00\x00\x00\x00WEBPVP8 ') == 'webp'
    assert sniff_image_extension(b'nothing', fallback='gif') == 'gif'


def _site_transport() -> httpx.MockTransport:
    def handler(request: httpx.Request) -> httpx.Response:
        path = request.url.path
        if path == '/s/xyz':
            return httpx.Response(200, text=_MP_ARTICLE, headers={'content-type': 'text/html; charset=utf-8'})
        if path.endswith('/abc/0'):
            assert request.headers['Referer'] == 'https://mp.weixin.qq.com/s/xyz'
            return httpx.Response(200, content=_JPG, headers={'content-type': 'image/jpeg'})
        if path.endswith('/def/0'):
            return httpx.Response(200, content=_PNG, headers={'content-type': 'image/png'})
        if path.endswith('/cover/0'):
            return httpx.Response(200, content=_TINY, headers={'content-type': 'image/jpeg'})
        if path == '/direct.png':
            return httpx.Response(200, content=_PNG, headers={'content-type': 'image/png'})
        return httpx.Response(404)

    return httpx.MockTransport(handler)


def test_download_link_images_keeps_real_pictures_and_drops_icons(tmp_path: Path) -> None:
    async def _run() -> list[Path]:
        async with httpx.AsyncClient(transport=_site_transport()) as client:
            return await download_link_images(client, 'https://mp.weixin.qq.com/s/xyz', tmp_path / 'article')

    saved = asyncio.run(_run())

    assert [p.name for p in saved] == ['001.jpg', '002.png']
    assert (tmp_path / 'article' / '001.jpg').read_bytes() == _JPG
    assert not list((tmp_path / 'article').glob('.*.part'))


def test_download_link_images_saves_a_direct_image_link(tmp_path: Path) -> None:
    async def _run() -> list[Path]:
        async with httpx.AsyncClient(transport=_site_transport()) as client:
            return await download_link_images(client, 'https://img.example/direct.png', tmp_path / 'direct')

    saved = asyncio.run(_run())

    assert [p.name for p in saved] == ['001.png']


def test_web_parser_turns_a_link_card_into_a_link_item() -> None:
    item = {
        'MsgId': '9001',
        'FromUserName': '@me',
        'ToUserName': 'filehelper',
        'MsgType': 49,
        'AppMsgType': 5,
        'FileName': '一篇文章',
        'Url': 'https://mp.weixin.qq.com/s/xyz',
        'Content': '&lt;msg&gt;&lt;appmsg&gt;&lt;title&gt;一篇文章&lt;/title&gt;&lt;url&gt;&lt;![CDATA[https://mp.weixin.qq.com/s/xyz]]&gt;&lt;/url&gt;&lt;/appmsg&gt;&lt;/msg&gt;',
    }
    message = parse_sync_message(item)
    assert message is not None
    assert message.text == '一篇文章'
    assert message.media[0].kind == 'link'
    assert json.loads(message.media[0].media.encrypt_query_param) == {
        'kind': 'link',
        'url': 'https://mp.weixin.qq.com/s/xyz',
        'title': '一篇文章',
    }

    # Without the top-level Url the XML still carries it.
    without_url = {k: v for k, v in item.items() if k != 'Url'}
    parsed = parse_sync_message(without_url)
    assert parsed is not None
    assert json.loads(parsed.media[0].media.encrypt_query_param)['url'] == 'https://mp.weixin.qq.com/s/xyz'

    # A merged chat record has nothing the web protocol can fetch.
    record = {**item, 'AppMsgType': 19, 'Url': ''}
    parsed_record = parse_sync_message(record)
    assert parsed_record is not None
    assert parsed_record.media == ()


def test_link_job_archives_a_folder_per_link_and_notifies(monkeypatch, tmp_path: Path) -> None:
    account = WeChatAccount(name='main', transport='filehelper', user_id='1', path=str(tmp_path / 'wechat'), media_types=['link'])
    queries: list[tuple[str, tuple]] = []
    notifications: list[dict] = []
    marks: list[str] = []

    async def _query_db(sql: str, params: tuple = ()):
        queries.append((sql, params))
        return []

    async def _completed(job, owner_token):
        marks.append('completed')
        return True

    async def _notify(**kwargs):
        notifications.append(kwargs)

    receiver = wechat_module.WeChat()
    receiver._link_client = httpx.AsyncClient(transport=_site_transport())
    monkeypatch.setattr(wechat_module, 'database', SimpleNamespace(query_db=_query_db))
    monkeypatch.setattr(wechat_module, 'mark_wechat_media_job_completed', _completed)
    monkeypatch.setattr(wechat_module, 'enqueue_notification', _notify)
    job = WeChatMediaJob(
        account_name='main',
        message_id=9001,
        item_index=0,
        media_type='link',
        title='一篇文章',
        file_name='',
        from_user_id='filehelper',
        context_token='',
        encrypt_query_param=json.dumps({'kind': 'link', 'url': 'https://mp.weixin.qq.com/s/xyz', 'title': '一篇文章'}),
        aes_key='',
        media_size=0,
        media_md5='',
        attempt_count=1,
    )

    downloaded = asyncio.run(receiver._process_job(account=account, client=SimpleNamespace(), job=job, owner_token='o'))

    assert downloaded is True
    folder = tmp_path / 'wechat' / '一篇文章 [9001]'
    assert sorted(p.name for p in folder.iterdir()) == ['001.jpg', '002.png']
    assert marks == ['completed']
    insert = next(params for sql, params in queries if 'INSERT INTO wechat ' in sql)
    assert insert[3] == 'link'
    assert insert[6] == str(folder)
    assert notifications[0]['link_url'] == 'https://mp.weixin.qq.com/s/xyz'
    assert notifications[0]['payload']['image_count'] == 2
    assert notifications[0]['payload']['image_path'] == str(folder / '001.jpg')


def test_link_job_without_pictures_is_discarded_not_retried(monkeypatch, tmp_path: Path) -> None:
    account = WeChatAccount(name='main', transport='filehelper', user_id='1', path=str(tmp_path / 'wechat'), media_types=['link'])
    outcomes: list[str] = []

    async def _query_db(sql: str, params: tuple = ()):
        return []

    async def _discard(job, owner_token, *, error):
        outcomes.append(error)
        return True

    receiver = wechat_module.WeChat()
    receiver._link_client = httpx.AsyncClient(
        transport=httpx.MockTransport(
            lambda request: httpx.Response(200, text='<html>no pictures</html>', headers={'content-type': 'text/html'})
        )
    )
    monkeypatch.setattr(wechat_module, 'database', SimpleNamespace(query_db=_query_db))
    monkeypatch.setattr(wechat_module, 'mark_wechat_media_job_discarded', _discard)
    job = WeChatMediaJob(
        account_name='main',
        message_id=9002,
        item_index=0,
        media_type='link',
        title='',
        file_name='',
        from_user_id='filehelper',
        context_token='',
        encrypt_query_param=json.dumps({'kind': 'link', 'url': 'https://site.example/empty', 'title': ''}),
        aes_key='',
        media_size=0,
        media_md5='',
        attempt_count=1,
    )

    asyncio.run(receiver._process_job(account=account, client=SimpleNamespace(), job=job, owner_token='o'))

    assert outcomes == ['ILinkError: no images found behind https://site.example/empty']
    assert not (tmp_path / 'wechat' / 'link [9002]').exists()

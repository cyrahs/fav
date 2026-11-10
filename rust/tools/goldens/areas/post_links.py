# ruff: noqa: RUF001
# (The full-width punctuation in this file is part of the inputs under test.)
"""Post links (WP13): 公众号 articles, Tieba, NGA, Zhihu, TikHub.

`parsing.json` calls the pure functions of `src/web/wechat_article.py`, `tieba_post.py`,
`nga_post.py`, `zhihu_post.py` and `post_links.py` on the inputs of the Python tests plus
awkward variants (entities, CDATA, nesting, escapes, odd JSON types, out-of-range
timestamps), as `{fn, args, result}`; a raise is `{"error": "Class: message"}`.

`fetch.json` runs the functions that talk to the network (`fetch_tieba_post`,
`fetch_nga_post`, `verify_session`, `fetch_post`, `resolve_video`, `zhihu_post.fetch_page`,
`post_links.fetch_page`, `fetch_image`, `fetch_video`, `tikhub.check_account`) against an
`httpx.MockTransport` that answers from a table of URLs. Each case records the requests
made (method, URL, the headers that matter, body), the result or the exception (class,
text and, for `PostLinkError`, `needs`), what was logged, and the NGA session checks
recorded. Transport failures are left to the Rust unit tests: their texts are httpcore's.
"""

from __future__ import annotations

import asyncio
import base64
import logging
import tempfile
from dataclasses import asdict, is_dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

import httpx
from src.core import settings
from src.tool import tikhub
from src.tool.site_sessions import SiteSession, StoredCookie
from src.web import nga_post, post_links, tieba_post, wechat_article, zhihu_post
from src.web.post_links import PostCaptchaError, PostLinkError
from src.web.wechat_article import ArticleLink, PostVideo
from src.web.zhihu_post import ZhihuLink, ZhihuVideo


def _plain(value: Any) -> Any:
    if is_dataclass(value) and not isinstance(value, type):
        return {key: _plain(item) for key, item in asdict(value).items()}
    if isinstance(value, tuple | list):
        return [_plain(item) for item in value]
    if isinstance(value, dict):
        return {str(key): _plain(item) for key, item in value.items()}
    if isinstance(value, datetime):
        return value.isoformat()
    if isinstance(value, bytes):
        return {'hex': value.hex()}
    return value


def _error(exc: BaseException) -> dict[str, Any]:
    out: dict[str, Any] = {'error': f'{exc.__class__.__name__}: {exc}'}
    if isinstance(exc, PostLinkError):
        out['needs'] = exc.needs
    if isinstance(exc, tikhub.TikHubError):
        out['code'] = exc.code
    return out


def _attempt(call: Any) -> Any:
    try:
        return _plain(call())
    except Exception as exc:  # noqa: BLE001 - the error is the recorded answer
        return _error(exc)


def _link(**fields: Any) -> ArticleLink:
    return ArticleLink(**fields)


# ---------- inputs ----------

_SHARE_URL = 'http://mp.weixin.qq.com/s?__biz=MzA5==&amp;mid=2247&amp;idx=1&amp;sn=abc123&amp;chksm=feed#rd'
_COVER = 'https://mmbiz.qpic.cn/sz_mmbiz_jpg/COVER/0?wx_fmt=jpeg'


def _share(url: str = _SHARE_URL, kind: str = '5', extra: str = '', title: str = '<title>每日分享</title>') -> str:
    return (
        f'<msg><appmsg appid="" sdkver="0">{title}<des>今天的</des><action>view</action>'
        f'<type>{kind}</type><url>{url}</url><sourcedisplayname>小叶菌ovo</sourcedisplayname>'
        f'<refermsg><type>1</type></refermsg>{extra}</appmsg><fromusername>wxid_sender</fromusername></msg>'
    )


def _image_post(count: str = '9', cover: str = _COVER) -> str:
    extra = (
        f'<mmreadershare><itemshowtype>8</itemshowtype><coverpicimageurl>{cover}</coverpicimageurl>'
        f'<piccount>{count}</piccount></mmreadershare>'
    )
    return _share(extra=extra)


CARDS = [
    _share(),
    _share(kind='4'),
    _share(kind='6'),
    _share(kind=' 5 '),
    _share(url='https://example.com/s?a=1'),
    _share(url='ftp://mp.weixin.qq.com/s?a=1'),
    _share(url=''),
    _share(url='<![CDATA[https://mp.weixin.qq.com/s/abcDEF]]>'),
    _share(title='<title><![CDATA[a &amp; b]]></title>'),
    _share(title='<title>  &lt;b&gt;x&lt;/b&gt;  </title>'),
    _share(title=''),
    _image_post(),
    _image_post(count='x'),
    _image_post(count='０'),
    _image_post(count='²'),
    _image_post(count=''),
    _image_post(cover='https://example.com/a.jpg'),
    _image_post(cover='https://mmbiz.qpic.cn/a b.jpg'),
    _share(url='https://tieba.baidu.com/p/9876543210?share=9&amp;sfc=weixin'),
    _share(url='https://tieba.baidu.com/p/11104915948?share_from=post&amp;is_video=true', kind='4'),
    _share(url='https://tieba.baidu.com/f?kw=x'),
    _share(url='https://bbs.nga.cn/read.php?tid=47695975'),
    _share(url='https://bbs.nga.cn/read.php?tid=47695975', kind='4'),
    _share(url='https://www.zhihu.com/question/1981023166554989468/answer/2100311203096573240?share_code=x&amp;utm_psn=1'),
    _share(url='https://zhuanlan.zhihu.com/p/42', kind='4'),
    _share(url='http://www.bilibili.com/video/BV1xx'),
    '<msg><appmsg><type>5</type><url>https://mp.weixin.qq.com/s/x</url>',
    '<appmsg>\n<type>5</type>\n<url>\n https://mp.weixin.qq.com/s/x \n</url></appmsg>',
    '<APPMSG><type>5</type><url>https://mp.weixin.qq.com/s/x</url></APPMSG>',
    '<appmsgx><type>5</type><url>https://mp.weixin.qq.com/s/x</url></appmsgx>',
    '<appmsg><title>t</title><url>https://mp.weixin.qq.com/s/x</url><type>5</type></appmsg>',
    '<appmsg><type>5</type><type>4</type><url>https://mp.weixin.qq.com/s/first</url><url>https://x/second</url></appmsg>',
    'plain text',
    '',
]

TAGS = [
    ('<a>x</a>', 'a'),
    ('<a> <![CDATA[ x ]]> </a>', 'a'),
    ('<a><![CDATA[x]]></a><a>y</a>', 'a'),
    ('<a>&amp;lt;</a>', 'a'),
    ('<a>&#x4e2d;&nbsp;</a>', 'a'),
    ('<a>\n line \n two </a>', 'a'),
    ('<a></a>', 'a'),
    ('<b>x</b>', 'a'),
    ('<a x="1">x</a>', 'a'),
    ('<a><![CDATA[]]></a>', 'a'),
    ('<a><![CDATA[x]]> tail</a>', 'a'),
]

CANONICAL = [
    'https://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123&chksm=feed#rd',
    'https://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123&scene=1&from_masonry=1&t=1',
    'https://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1',
    'https://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=',
    'https://mp.weixin.qq.com/s?__biz=a+b&mid=%E4%B8%AD&idx=1&sn=x&sn=y',
    'https://mp.weixin.qq.com/s/abcDEF?scene=1#rd',
    'http://mp.weixin.qq.com/s/abcDEF',
    'https://MP.WEIXIN.QQ.COM/s/abc',
    'https://mp.weixin.qq.com:443/s?__biz=a&mid=1&idx=1&sn=b',
    'https://mp.weixin.qq.com',
    'https://mp.weixin.qq.com/mp/appmsg/show?__biz=a&mid=1&idx=1&sn=b',
    'https://tieba.baidu.com/p/1#frag',
    'https://bbs.nga.cn/read.php?tid=1#x',
    'https://example.com/a?b=1#c',
    'not a url',
    '',
]

_IMAGE_A = 'https://mmbiz.qpic.cn/sz_mmbiz_jpg/AAA/640?wx_fmt=jpeg&from=appmsg'
_IMAGE_B = 'https://mmbiz.qpic.cn/sz_mmbiz_png/BBB/640?wx_fmt=png'
_PAGE = f"""<html><head><meta property="og:title" content="og title" /></head>
<script>var msg_title = '每日Coser分享'.html(false); var nickname = htmlDecode("小叶菌ovo"); var ct = "1759600000";</script>
<img class="avatar" data-src="https://mmbiz.qpic.cn/mmbiz_png/AVATAR/0?wx_fmt=png" />
<div class="rich_media_content" id="js_content">
<p><img class="rich_pages" data-src="{_IMAGE_A.replace('&', '&amp;')}" /></p>
<p><img data-ratio="1" data-src="{_IMAGE_B}" /></p>
<p><img data-src="{_IMAGE_A.replace('&', '&amp;')}" /></p>
</div></html>"""


def _picture_item(name: str) -> str:
    return (
        f"{{ width: '2560' * 1, height: '3413' * 1,"
        f" cdn_url: 'https:\\x2f\\x2fmmbiz.qpic.cn\\x2fsz_mmbiz_jpg\\x2f{name}\\x2f0?wx_fmt=jpeg\\x26amp;from=appmsg',"
        f" watermark_info: {{ cdn_url: 'https://mmbiz.qpic.cn/sz_mmbiz_jpg/W{name}/0?wx_fmt=jpeg', width: '0' * 1,"
        f" title: 'a }} [ \\' tricky' }}, live_photo: [], }}"
    )


_PICTURE_PAGE = (
    '<html><script>var msg_title = \'九张图\';\nvar nickname = htmlDecode("某号");\nvar ct = "1760000000";\n'
    'window.picture_page_info_list = [' + ','.join(_picture_item(f'P{n}') for n in range(3)) + '];\n'
    'if (window.picture_page_info_list.length) {}</script><div id="js_content">caption</div></html>'
)

PAGES = [
    _PAGE,
    _PICTURE_PAGE,
    "var msg_title = 'pics'; picture_page_info_list = [{cdn_url: 'https://mmbiz.qpic.cn/mmbiz_jpg/P1/0?wx_fmt=jpeg'}];",
    'picture_page_info_list = []; <div id="js_content"></div>',
    '<meta property="og:title" content="&lt;OG&gt; &amp; title" /><div id="js_content"><img data-src="http://mmbiz.qpic.cn/x/640"></div>',
    'var msg_title = \'  &amp;spaced&#39;  \'; var ct = "0";',
    'var ct = "١٢٣"; var nickname = htmlDecode("a&quot;b");',
    'var ct = "1000000000000";',
    'var ct = "100000000000000000";',
    'var ct = "100000000000000000000";',
    'var ct = "253402300799";',
    'var ct = "1759600000"; var ct = "1";',
    '<IMG DATA-SRC="https://mmbiz.qpic.cn/a/640">' + '<div id="js_content"><IMG class=x DATA-SRC="https://mmbiz.qpic.cn/b/640"></div>',
    '<div id="js_content"><img data-src="https://mmbiz.qpic.cn/a/640"><img src="https://mmbiz.qpic.cn/c/640"><img data-src=\'https://mmbiz.qpic.cn/d\'></div>',
    '<div id="js_content"><img alt=">" data-src="https://mmbiz.qpic.cn/e/640"><imgx data-src="https://mmbiz.qpic.cn/f/640"></div>',
    '<div id="js_content"><img data-src="https://mmbiz.qpic.cn/g/640?a=1&amp;b=2"><img xdata-src="https://mmbiz.qpic.cn/h/640"></div>',
    (
        'picture_page_info_list: [{"cdn_url": "https://mmbiz.qpic.cn/q/0"}, {\'cdn_url\' : "https://mmbiz.qpic.cn/q/0"},'
        ' {x_cdn_url: "https://mmbiz.qpic.cn/z/0", cdn_url: "https://mmbiz.qpic.cn/r/0"}]'
    ),
    "picture_page_info_list = [{cdn_url: 'https://example.com/x.jpg'}]; picture_page_info_list = [{cdn_url: 'https://mmbiz.qpic.cn/s/0'}]",
    "picture_page_info_list = [{a: [1, {cdn_url: 'https://mmbiz.qpic.cn/nested/0'}], cdn_url: 'https://mmbiz.qpic.cn/t/0'}, {cdn_url: \"https://mmbiz.qpic.cn/u\\u002f0\"}]",
    "picture_page_info_list = [{cdn_url: 'https://mmbiz.qpic.cn/v/0\\'x'}, {cdn_url: `https://mmbiz.qpic.cn/w/0`}, {$cdn_url: 'https://mmbiz.qpic.cn/y/0'}]",
    "picture_page_info_list = [{cdn_url: 'https://mmbiz.qpic.cn/unclosed/0'",
    "picture_page_info_list = [{cdn_url: ' https://mmbiz.qpic.cn/sp/0 '}, {cdn_url: 'https://mmbiz.qpic.cn/sp/0\\n'}]",
    "picture_page_info_list=[ {cdn_url:'https://mmbiz.qpic.cn/amp/0?a=1\\x26amp;b=2&amp;c'} ]",
    "picture_page_info_list = [{cdn_url: 'https://mmbiz.qpic.cn/esc/\\t\\r\\q\\\\0'}]",
    "picture_page_info_list = [ 'x', {cdn_url: 'https://mmbiz.qpic.cn/str/0'} ]",
    '',
]

IMAGE_URLS = [
    _IMAGE_A,
    'https://mmbiz.qpic.cn/mmbiz_gif/C/640',
    'https://mmbiz.qpic.cn/mmbiz_gif/C/640/',
    'https://mmbiz.qpic.cn/mmbiz_gif/123/640?x=1',
    'https://mmbiz.qpic.cn/mmbiz_gif/C/0',
    'https://mmbiz.qpic.cn/mmbiz_gif/C',
    'https://mmbiz.qpic.cn/a/１２/b',
    'https://mmbiz.qpic.cn/a#frag/640',
    'http://u:p@mmbiz.qpic.cn:8080/a/300?x',
    '',
]

EXTENSIONS = [
    (_IMAGE_B, ''),
    (_IMAGE_A, 'image/webp'),
    (_IMAGE_A, 'IMAGE/PNG; charset=x'),
    (_IMAGE_A, 'image/svg+xml'),
    (_IMAGE_A, 'text/html'),
    ('https://mmbiz.qpic.cn/a/0?wx_fmt=GIF', ''),
    ('https://mmbiz.qpic.cn/a/0?wx_fmt=tiff', ''),
    ('https://mmbiz.qpic.cn/a/0?wx_fmt=', 'image/'),
    ('https://mmbiz.qpic.cn/a/0?wx_fmt=bmp&wx_fmt=png', ' image/jpeg '),
    ('https://mmbiz.qpic.cn/a/0', ''),
]

THREAD_URLS = [
    'https://tieba.baidu.com/p/9876543210?share=9',
    'https://tieba.baidu.com/p/9876543210/',
    'https://tieba.baidu.com/p/9876543210//',
    'http://m.tieba.baidu.com/p/1',
    'https://jump2.tieba.baidu.com/p/1',
    'https://TIEBA.BAIDU.COM/p/1',
    'https://tieba.baidu.com.evil/p/1',
    'https://eviltieba.baidu.com/p/1',
    'https://tieba.baidu.com/p/１２',
    'https://tieba.baidu.com/p/12a',
    'https://tieba.baidu.com/p/',
    'https://tieba.baidu.com/f?kw=x',
    'https://tieba.baidu.com/mo/p/1',
    'ftp://tieba.baidu.com/p/1',
    'tieba.baidu.com/p/1',
    '',
]

NGA_URLS = [
    'https://bbs.nga.cn/read.php?tid=47695975',
    'https://ngabbs.com/read.php?tid=123&rand=5',
    'https://nga.178.com/read.php?pid=1&tid=9',
    'http://bbs.ngacn.cc/read?tid=1',
    'https://g.nga.cn/read.php/?tid=1',
    'https://nga.cn/read.php?tid=1234567890123',
    'https://nga.cn/read.php?tid=123456789012',
    'https://nga.cn/read.php?tid=',
    'https://nga.cn/read.php?tid=a1',
    'https://nga.cn/read.php?tid=%31%32',
    'https://nga.cn/read.php?tid=１２',
    'https://nga.cn/read.php?tid=+1',
    'https://nga.cn/read.php?tid=1&tid=2',
    'https://nga.cn/read.php?tid=&tid=3',
    'https://BBS.NGA.CN/read.php?tid=1',
    'https://bbs.nga.cn/thread.php?fid=-7',
    'https://evil.example/read.php?tid=1',
    'ftp://bbs.nga.cn/read.php?tid=1',
    'https://bbs.nga.cn:8443/read.php?tid=1',
    '',
]

NGA_SRCS = [
    './mon_202510/11/A.jpg.medium.jpg',
    './mon_202510/11/A.png.thumb_s.jpg',
    './mon_202510/11/A.gif.thumb_ss.jpeg',
    './mon_202510/11/A.JPG.THUMB.JPG',
    './mon_202510/11/A.webp.medium.png',
    './mon_202510/11/A.jpg.medium.jpg?x=1',
    'http://img4.nga.178.com/attachments/mon_202510/11/A.jpg',
    'https://img.nga.178.com/attachments/mon_202510/11/B.jpg',
    'http://img.nga.178.com:8080/attachments/x.jpg',
    'http://user@img2.nga.178.com/x.jpg',
    'http://imgx.nga.178.com/x.jpg',
    'https://img.nga.cn/attachments/mon_202510/11/A.jpg',
    '/attachments/mon_202510/11/A.jpg',
    'mon_202510/11/A.jpg',
    ' &amp;./mon_1/a.jpg ',
    '&#46;/mon_1/a.jpg',
    'javascript:alert(1)',
    '//img.nga.cn/x.jpg',
    'ftp://x/y.jpg',
    '',
    './../x.jpg',
]

NGA_ROWS: list[Any] = [
    {
        'lou': 0,
        'content': (
            'text<br/>[img]./mon_202510/11/A.jpg.medium.jpg[/img]'
            '[img]https://img.nga.178.com/attachments/mon_202510/11/B.jpg[/img][img]./mon_202510/11/A.jpg[/img]'
        ),
        'attachs': {'0': {'attachurl': 'mon_202510/11/C.jpg', 'type': 'img'}, '1': {'attachurl': 'mon_202510/11/x.zip', 'type': 'zip'}},
    },
    {'content': '[IMG] ./a.jpg [/IMG][img]\n./b.jpg\n[/img][img][/img][img]javascript:x[/img]'},
    {
        'content': None,
        'attachs': [
            {'attachurl': 'http://x.example/a.jpg'},
            {'attachurl': './b.png', 'type': None},
            {'attachurl': ''},
            'x',
            {'type': 'img'},
        ],
    },
    {'content': 5, 'attachs': 'nope'},
    {'attachs': [{'attachurl': 5, 'type': 'img'}, {'attachurl': 'mon_1/a.jpg', 'type': 0}, {'attachurl': 'mon_1/b.jpg', 'type': ''}]},
    {},
]

NGA_PAYLOADS: list[Any] = [
    {'error': ['1:未登录', '<br/><a href=/nuke.php>[登录]</a>'], 'data': {'__MESSAGE': {}}},
    {'error': ['你必须先登录论坛'], 'time': 1},
    {'error': []},
    {'error': '12:帖子不存在'},
    {'error': None, 'data': {'__MESSAGE': {'1': '<b>无此帖</b>', '0': 'x'}}},
    {'data': {'__MESSAGE': {'0': '15:访客不能 &amp; 查看'}}},
    {'data': {'__MESSAGE': {}}},
    {'data': {'__MESSAGE': '2:message string'}},
    {'data': {'__MESSAGE': {'1': ''}}},
    {'data': {'__MESSAGE': {'1': 0, '2': 'second'}}},
    {'data': []},
    {'error': 5},
    {'error': [0]},
    {'error': ['  3: spaced ']},
    {'error': ['a:b']},
    {'data': {'0': []}, 'time': 1},
    [],
    None,
    'text',
]

TIMESTAMP_VALUES: list[Any] = [
    1791654000,
    '1791654000',
    ' 1791654000 ',
    '١٢٣',
    '²',
    '',
    None,
    0,
    '0',
    -5,
    '-5',
    1.5,
    True,
    '1000000000000',
    '100000000000000000',
    '9223372036854775808',
    [],
]

ZHIHU_URLS = [
    'https://www.zhihu.com/question/1981023166554989468/answer/2100311203096573240',
    'https://www.zhihu.com/question/1/answer/2/?share_code=abc&utm_psn=1',
    'https://www.zhihu.com/answer/2100311203096573240',
    'https://zhuanlan.zhihu.com/p/669214677',
    'http://www.zhihu.com/pin/2100667138746983678',
    'https://link.zhihu.com/?target=https%3A//zhuanlan.zhihu.com/p/42',
    'https://link.zhihu.com/?x=1&target=https%3A%2F%2Fwww.zhihu.com%2Fpin%2F7&y=2',
    'https://link.zhihu.com/?target=https%3A//example.com/answer/1',
    'https://link.zhihu.com/?target=',
    'https://link.zhihu.com/?xtarget=https%3A//zhuanlan.zhihu.com/p/42',
    'https://link.zhihu.com/',
    'https://www.zhihu.com/question/1981023166554989468',
    'https://zhihu.com.evil.example/answer/1',
    'https://example.com/answer/1',
    'https://zhihu.com/answer/1',
    'https://ZHUANLAN.ZHIHU.COM/p/1/',
    'https://zhuanlan.zhihu.com/answer/1',
    'https://www.zhihu.com/p/1',
    'https://www.zhihu.com/pin/１２',
    'https://www.zhihu.com/question/x/answer/1',
    'https://m.zhihu.com/answer/5',
    ' https://www.zhihu.com/pin/7 ',
    'ftp://www.zhihu.com/pin/7',
    'https://www.zhihu.com/zvideo/1',
    '',
]

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

ZHIHU_HTML = [
    _ANSWER_HTML,
    _ANSWER_HTML + _ANSWER_HTML.replace('2091194612627121396', '1'),
    '<IMG DATA-ORIGINAL="https://pic1.zhimg.com/a.jpg"><img data-original="https://evil.example/a.jpg" data-actualsrc="https://pic2.zhimg.com/b.jpg">',
    '<img data-original="" src="https://pic3.zhimg.com/c.jpg?a=1&amp;b=2"><img src=" https://zhimg.com/d.jpg ">',
    '<img xdata-original="https://pic1.zhimg.com/x.jpg" src="ftp://pic1.zhimg.com/y.jpg"><img data-original=\'https://pic1.zhimg.com/q.jpg\'>',
    '<a class="video-box" href="https://www.zhihu.com/video/555"><img src="https://pic1.zhimg.com/p.jpg"></a>',
    (
        '<a class="x video-box y" data-poster="https://evil.example/p.jpg" data-lens-id="777">v</a>'
        '<a class="video-boxes" data-lens-id="1">n</a>'
    ),
    '<A CLASS="video-box" data-lens-id="888">\n</A><a class="video-box">no id</a><a class="video-box" data-lens-id="888">dup</a>',
    '<a class="video-box" data-poster="https://pic1.zhimg.com/p.jpg?a=1&amp;b=2" data-lens-id="99">',
    '<a class="video-box" data-lens-id="12">x</a> <img src="https://pic1.zhimg.com/after.jpg">',
    '<img src="https://pic1.zhimg.com/a.jpg"><img src="https://pic1.zhimg.com/a.jpg">',
    '<a href="x" class="video-box" data-lens-id="abc">x</a>',
    '',
]

PLAYLISTS: list[Any] = [
    {
        'LD': {'width': 476, 'height': 848, 'bitrate': 379.5, 'play_url': 'https://vdn3.vzuu.com/SD/v.mp4'},
        'HD': {'width': 720, 'height': 1280, 'bitrate': 559.1, 'play_url': 'https://vdn3.vzuu.com/HD/v.mp4'},
    },
    [
        {'quality': 'ld', 'width': 476, 'height': 848, 'url': 'https://vdn3.vzuu.com/SD/x.mp4'},
        {'quality': 'hd', 'width': 720, 'height': 1280, 'url': 'https://vdn3.vzuu.com/HD/x.mp4'},
    ],
    [
        {'width': 720, 'height': 1280, 'bitrate': 1, 'url': 'https://a/1.mp4'},
        {'width': 720, 'height': 1280, 'bitrate': 2, 'url': 'https://a/2.mp4'},
        {'width': 720, 'height': 1280, 'bitrate': 2, 'url': 'https://a/3.mp4'},
    ],
    [{'url': 'https://a/none.mp4'}, {'width': None, 'height': 0, 'url': 'https://a/zero.mp4'}],
    [
        {'width': '720', 'height': '1280', 'bitrate': '1.5', 'play_url': 'http://a/str.mp4'},
        {'width': 1, 'height': 1, 'url': 'https://a/s.mp4'},
    ],
    [{'width': 720.9, 'height': 2, 'url': 'https://a/f.mp4'}, {'width': True, 'height': 3000, 'url': 'https://a/b.mp4'}],
    [{'width': 'wide', 'height': 1, 'url': 'https://a/bad.mp4'}],
    [{'bitrate': 'fast', 'url': 'https://a/bad.mp4'}],
    [{'bitrate': [], 'url': 'https://a/bad.mp4'}],
    [{'play_url': '', 'url': 'https://a/fallback.mp4'}, {'play_url': 'ftp://a/x.mp4'}, {'url': 5}, 'x', None],
    [{'width': -1, 'height': 1, 'url': 'https://a/neg.mp4'}],
    {},
    [],
    None,
    'playlist',
]

PIN_ITEMS: list[Any] = [
    [
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
    [
        {'type': 'image', 'url': 'https://pic1.zhimg.com/a.jpg', 'original_url': 'https://evil.example/a.jpg'},
        {'type': 'image', 'url': 'https://pic1.zhimg.com/a.jpg'},
        {'type': 'image', 'original_url': 5},
        {'type': 'video', 'video_id': 123, 'playlist': [{'url': 'https://a/p.mp4'}], 'thumbnail': 'https://evil.example/t.jpg'},
        {'type': 'video', 'video_id': 'abc'},
        {'type': 'video', 'video_id': '²', 'video_info': 'x'},
        {'type': 'video', 'video_id': 0},
        {'type': 'video', 'video_id': '7', 'video_info': {'playlist': None}, 'playlist': [{'url': 'https://a/q.mp4'}], 'thumbnail': 5},
        {'type': 'text', 'content': '&lt;b&gt;two&lt;/b&gt;'},
        {'type': 'text', 'own_text': '', 'content': 'fallback'},
        {'type': 'text'},
        {'type': 'link', 'url': 'https://pic1.zhimg.com/link.jpg'},
        'string item',
        None,
    ],
    [{'type': 'text', 'own_text': '  <p>x</p>  '}, {'type': 'text', 'own_text': 5}],
    {'type': 'text'},
    'text',
    None,
    [],
]

ZHIHU_POSTS: list[tuple[str, str, Any]] = [
    (
        'answer',
        '2100311203096573240',
        {
            'content': _ANSWER_HTML,
            'author': {'name': '知乎用户mYQRnd'},
            'created_time': 1793533212,
            'question': {'title': '高跟鞋配白丝真的很难看吗？'},
        },
    ),
    ('answer', '9', {'question': {'title': '  '}, 'author': {'name': ' padded '}, 'created_time': '1793533212'}),
    ('answer', '9', {'question': 'not a dict', 'author': 'x', 'created_time': -1, 'content': 5}),
    ('answer', '9', {'created_time': '1.5', 'author': {'name': None}}),
    ('answer', '9', {'created_time': 10**12}),
    ('answer', '9', {'created_time': 1.9}),
    ('article', '42', {'title': 'Article', 'content': _ANSWER_HTML, 'author': {'name': 'a'}, 'created': 1793533212}),
    ('article', '42', {'title': None, 'created': 0}),
    ('article', '42', {'title': 5, 'created': True}),
    (
        'pin',
        '2100667138746983678',
        {
            'author': {'name': 'someone'},
            'created': 1793533212,
            'excerpt_title': '穿搭小技巧 | <a class="hash_tag" href="https://www.zhihu.com/topic/1">#大衣穿搭</a>',
            'content': PIN_ITEMS[0],
        },
    ),
    ('pin', '7', {'content': [{'type': 'text', 'own_text': '只有正文 &amp; 文字'}]}),
    ('pin', '7', {'content': [{'type': 'text', 'own_text': '长' * 40}]}),
    ('pin', '7', {'excerpt_title': 'a' * 79 + '中文', 'content': []}),
    ('pin', '7', {'excerpt_title': 'a' * 80, 'content': []}),
    ('pin', '7', {'excerpt_title': 'a' * 78 + ' 中', 'content': []}),
    ('pin', '7', {'excerpt_title': '<a></a>', 'content': None}),
    ('pin', '7', {'created': 'x'}),
]

SHORT = [('', 80), ('abc', 3), ('abcd', 3), ('中文字', 6), ('中文字', 7), ('ab 中', 4), ('a \t\n中', 4), ('😀😀', 5)]

POST_URLS = [
    'https://tieba.baidu.com/p/9876543210?share=9',
    'https://bbs.nga.cn/read.php?tid=12345&page=2',
    'https://ngabbs.com/read.php?tid=12345',
    'https://zhuanlan.zhihu.com/p/42?utm=1',
    'https://www.zhihu.com/pin/7',
    'http://mp.weixin.qq.com/s?__biz=a&mid=1&idx=1&sn=b&chksm=c',
    'HTTP://mp.weixin.qq.com/s/x',
    'http://MP.WEIXIN.QQ.COM/s/x',
    ' https://mp.weixin.qq.com/s/x ',
    'https://mp.weixin.qq.com/s/x#rd',
    'https://www.bilibili.com/video/BV1xx',
    'ftp://tieba.baidu.com/p/1',
    'https://link.zhihu.com/?target=https%3A//www.zhihu.com/answer/5',
    'mp.weixin.qq.com/s/x',
    '',
]

TEXTS: list[tuple[str, ...]] = [
    ('看这个（https://zhuanlan.zhihu.com/p/42）',),
    ('nothing here', 'https://tieba.baidu.com/p/5'),
    ('https://example.com/a', ''),
    ('see https://example.com/a and https://www.zhihu.com/pin/7 too',),
    ('【https://tieba.baidu.com/p/1】', 'https://www.zhihu.com/pin/7'),
    ('<a href="https://www.zhihu.com/pin/8">x</a>',),
    ("'https://www.zhihu.com/pin/9'",),
    ('[https://www.zhihu.com/pin/10]',),
    ('HTTPS://WWW.ZHIHU.COM/pin/11',),
    ('https://www.zhihu.com/pin/12　next',),
    ('https://www.zhihu.com/pin/13\x1cnext',),
    ('httpx://www.zhihu.com/pin/14 http://',),
    ('https://',),
    ('',),
    (),
]

UNSUPPORTED = [('https://example.com/a', 'a text message'), ('', 'a WeChat sticker'), ('', ''), ('x', '')]

SNIFF = [
    b'\xff\xd8\xff\xe0',
    b'\x89PNG\r\n\x1a\n',
    b'GIF89a',
    b'RIFF\x00\x00\x00\x00WEBPVP8 ',
    b'RIFF\x00\x00\x00\x00WEBX',
    b'RIFF',
    b'wxgf....',
    b'\xff\xd8',
    b'',
]

MP4_HEADS = [b'\x00\x00\x00\x18ftypmp42', b'\x00\x00\x00\x18ftyp', b'\x00\x00\x00\x18moov', b'ftyp', b'']


def _parsing() -> list[dict[str, Any]]:  # noqa: C901, PLR0912 - one block per function under test
    cases: list[dict[str, Any]] = []

    def add(fn: str, args: list[Any], call: Any) -> None:
        cases.append({'fn': fn, 'args': _plain(args), 'result': _attempt(call)})

    # wechat_article
    for xml, name in TAGS:
        add('wechat_article._tag', [xml, name], lambda xml=xml, name=name: wechat_article._tag(xml, name))  # noqa: SLF001
    for card in CARDS:
        add('wechat_article.appmsg_link', [card, ['5']], lambda card=card: wechat_article.appmsg_link(card))
        add('wechat_article.appmsg_link', [card, ['5', '4']], lambda card=card: wechat_article.appmsg_link(card, ('5', '4')))
        add('wechat_article.parse_article_link', [card], lambda card=card: wechat_article.parse_article_link(card))
        add('tieba_post.parse_tieba_link', [card], lambda card=card: tieba_post.parse_tieba_link(card))
        add('nga_post.parse_nga_link', [card], lambda card=card: nga_post.parse_nga_link(card))
        add('post_links.shared_link', [card], lambda card=card: post_links.shared_link(card))
    for link in [
        {'url': 'u', 'title': 't', 'account': 'a', 'show_type': '8', 'picture_count': 9, 'cover_url': _COVER},
        {'url': 'u', 'title': 't', 'show_type': '8', 'picture_count': 0, 'cover_url': _COVER},
        {'url': 'u', 'title': 't', 'show_type': '8', 'picture_count': 3, 'cover_url': ''},
        {'url': 'u', 'title': 't', 'show_type': '5', 'picture_count': 3, 'cover_url': _COVER},
    ]:
        add('wechat_article.cover_only_page', [link], lambda link=link: wechat_article.cover_only_page(_link(**link)))
    for url in CANONICAL:
        add('wechat_article.canonical_url', [url], lambda url=url: wechat_article.canonical_url(url))
    for text in [
        r'https:\x2f\x2fa',
        r'中文',
        r'\n\t\r\q\\',
        r'&amp;\x26amp;',
        r'\u00',
        r'\x4',
        '\\',
        'a\\\nb',
        r'\'\"',
        '',
    ]:
        add('wechat_article._js_unescape', [text], lambda text=text: wechat_article._js_unescape(text))  # noqa: SLF001
    for page in PAGES:
        add('wechat_article.picture_list', [page], lambda page=page: wechat_article.picture_list(page))
        add('wechat_article.parse_article_page', [page], lambda page=page: wechat_article.parse_article_page(page))
    for url in IMAGE_URLS:
        add('wechat_article.original_image_url', [url], lambda url=url: wechat_article.original_image_url(url))
    for url, content_type in EXTENSIONS:
        add(
            'wechat_article.image_extension',
            [url, content_type],
            lambda url=url, content_type=content_type: wechat_article.image_extension(url, content_type),
        )
    for url in [
        'https://mp.weixin.qq.com/mp/wappoc_appmsgcaptcha?poc_token=x',
        'https://mp.weixin.qq.com/mp/verifycode',
        'https://mp.weixin.qq.com/mp/verifycode/',
        'https://example.com/mp/verifycode',
        'https://mp.weixin.qq.com/s?x=/mp/verifycode',
        '',
    ]:
        add('wechat_article.is_captcha', [url], lambda url=url: wechat_article.is_captcha(url))

    # tieba_post
    for url in THREAD_URLS:
        add('tieba_post.thread_id', [url], lambda url=url: tieba_post.thread_id(url))
    for params in [
        {'_client_type': '2', '_client_version': '12.57.4.2', 'kz': '9876543210', 'pn': '1', 'rn': '2'},
        {'b': 'x', 'a': '中文', 'A': ' '},
        {},
    ]:
        add('tieba_post._signed', [params], lambda params=params: tieba_post._signed(params))  # noqa: SLF001
    for post in [
        {
            'content': [
                {'type': '0', 'text': 'floor'},
                {'type': '3', 'origin_src': 'http://tiebapic.baidu.com/A.jpg', 'big_cdn_src': 'http://b/A.jpg'},
                {'type': 3, 'origin_src': '', 'big_cdn_src': 'https://b/B.jpg', 'cdn_src': 'https://c/B.jpg'},
                {'type': '3', 'cdn_src': 'ftp://c/C.jpg'},
                {'type': '3', 'origin_src': 5, 'big_cdn_src': 'https://b/D.jpg'},
                {'type': 3.0, 'origin_src': 'https://x/float.jpg'},
                {'type': True, 'origin_src': 'https://x/bool.jpg'},
                {'type': '5', 'link': 'https://tb-video.bdstatic.com/v.mp4', 'src': 'http://tiebapic.baidu.com/cover.jpg'},
                {'type': 5, 'link': 'https://tb-video.bdstatic.com/v.mp4', 'src': 'https://other/cover.jpg'},
                {'type': 5, 'link': 'https://tb-video.bdstatic.com/w.mp4', 'src': 5},
                {'type': 5, 'link': 'ftp://x/v.mp4'},
                'x',
                None,
            ],
        },
        {'content': None},
        {'content': 'abc'},
        {'content': {'type': '3'}},
        {'content': 5},
        {'content': True},
        {},
    ]:
        add('tieba_post._images', [post], lambda post=post: tieba_post._images(post))  # noqa: SLF001
        for thread in [{}, {'video_info': {'video_url': 'https://tb-video.bdstatic.com/t.mp4', 'thumbnail_url': 'ftp://x'}}]:
            add('tieba_post._videos', [post, thread], lambda post=post, thread=thread: tieba_post._videos(post, thread))  # noqa: SLF001
    for thread in [
        {'video_info': {'video_url': 'https://tb-video.bdstatic.com/t.mp4', 'thumbnail_url': 'http://tiebapic.baidu.com/t.jpg'}},
        {'video_info': {'video_url': 'tb-video.bdstatic.com/t.mp4'}},
        {'video_info': 'x'},
        {'video_info': {'video_url': 5}},
    ]:
        add('tieba_post._videos', [{}, thread], lambda thread=thread: tieba_post._videos({}, thread))  # noqa: SLF001

    # nga_post
    for url in NGA_URLS:
        add('nga_post.nga_thread_id', [url], lambda url=url: nga_post.nga_thread_id(url))
    for src in NGA_SRCS:
        add('nga_post.original_url', [src], lambda src=src: nga_post.original_url(src))
    for row in NGA_ROWS:
        add('nga_post.floor_images', [row], lambda row=row: nga_post.floor_images(row))
    for payload in NGA_PAYLOADS:
        add('nga_post._error_text', [payload], lambda payload=payload: nga_post._error_text(payload))  # noqa: SLF001
    for value in TIMESTAMP_VALUES:
        add('nga_post._timestamp', [value], lambda value=value: nga_post._timestamp(value))  # noqa: SLF001
    for cookies in [
        [('ngaPassportUid', '42'), ('ngaPassportCid', 'cid')],
        [('NGAPASSPORTCID', 'cid'), ('ngaPassportUid', ''), ('ngapassportuid', '7')],
        [('ngaPassportUid', '42')],
        [],
    ]:
        add(
            'nga_post.login_cookies',
            [cookies],
            lambda cookies=cookies: nga_post.login_cookies(
                tuple(StoredCookie(name=name, value=value, domain='.nga.cn') for name, value in cookies)
            ),
        )

    # zhihu_post
    for url in ZHIHU_URLS:
        add('zhihu_post.parse_zhihu_url', [url], lambda url=url: zhihu_post.parse_zhihu_url(url))
    for kind, ident in [('answer', '1'), ('article', '2'), ('pin', '3')]:
        add('zhihu_post.ZhihuLink.url', [kind, ident], lambda kind=kind, ident=ident: ZhihuLink(kind, ident).url)
    for content in ZHIHU_HTML:
        add('zhihu_post.content_images', [content], lambda content=content: zhihu_post.content_images(content))
        add('zhihu_post.content_videos', [content], lambda content=content: zhihu_post.content_videos(content))
    for playlist in PLAYLISTS:
        add('zhihu_post._best_playlist_url', [playlist], lambda playlist=playlist: zhihu_post._best_playlist_url(playlist))  # noqa: SLF001
    for items in PIN_ITEMS:
        add('zhihu_post._pin_media', [items], lambda items=items: zhihu_post._pin_media(items))  # noqa: SLF001
    for value in TIMESTAMP_VALUES:
        add('zhihu_post._timestamp', [value], lambda value=value: zhihu_post._timestamp(value))  # noqa: SLF001
    for text, limit in SHORT:
        add('zhihu_post._short', [text, limit], lambda text=text, limit=limit: zhihu_post._short(text, limit))  # noqa: SLF001
    for kind, ident, data in ZHIHU_POSTS:
        add(
            'zhihu_post.parse_post',
            [kind, ident, data],
            lambda kind=kind, ident=ident, data=data: zhihu_post.parse_post(ZhihuLink(kind, ident), data),
        )

    # post_links
    for url in POST_URLS + THREAD_URLS + NGA_URLS + ZHIHU_URLS:
        add('post_links.link_for_url', [url], lambda url=url: post_links.link_for_url(url))
    add(
        'post_links.link_for_url',
        ['https://www.zhihu.com/pin/7', 'T', 'D'],
        lambda: post_links.link_for_url('https://www.zhihu.com/pin/7', title='T', description='D'),
    )
    for texts in TEXTS:
        add('post_links.find_link', list(texts), lambda texts=texts: post_links.find_link(*texts))
        add('post_links.first_url', list(texts), lambda texts=texts: post_links.first_url(*texts))
    for url, what in UNSUPPORTED:
        add('post_links.unsupported_reason', [url, what], lambda url=url, what=what: post_links.unsupported_reason(url, what))
    for url in [
        'https://tieba.baidu.com/p/1',
        'https://bbs.nga.cn/read.php?tid=1',
        'https://www.zhihu.com/pin/7',
        'https://mp.weixin.qq.com/s/x',
        'https://example.com/',
    ]:
        add('post_links.site_name', [url], lambda url=url: post_links.site_name(_link(url=url, title='')))
    for content in SNIFF:
        add('post_links.sniff_extension', [{'hex': content.hex()}], lambda content=content: post_links.sniff_extension(content))
    for head in MP4_HEADS:
        add('post_links.is_mp4', [{'hex': head.hex()}], lambda head=head: post_links.is_mp4(head))
    return cases


# ---------- fetch scenarios ----------

# The request headers a case records: the rest (accept, connection, host) are the
# client library's own.
_HEADERS = ('user-agent', 'authorization', 'cookie', 'referer', 'content-type')

Route = dict[str, Any]


def _ok(status: int = 200, *, json: Any = None, text: str | None = None, body: bytes | None = None, headers: dict | None = None) -> Route:
    route: Route = {'status': status, 'headers': dict(headers or {})}
    if json is not None:
        route['json'] = json
    elif text is not None:
        route['text'] = text
    elif body is not None:
        route['body_b64'] = base64.b64encode(body).decode()
    return route


def _response(route: Route) -> httpx.Response:
    headers = route.get('headers') or {}
    if 'json' in route:
        return httpx.Response(route['status'], json=route['json'], headers=headers)
    if 'text' in route:
        return httpx.Response(route['status'], text=route['text'], headers=headers)
    if 'body_b64' in route:
        return httpx.Response(route['status'], content=base64.b64decode(route['body_b64']), headers=headers)
    return httpx.Response(route['status'], headers=headers)


class _Web:
    def __init__(self, routes: dict[str, Route]) -> None:
        self.routes = routes
        self.requests: list[dict[str, Any]] = []

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(
            {
                'method': request.method,
                'url': str(request.url),
                'headers': {name: request.headers[name] for name in _HEADERS if name in request.headers},
                'body': request.content.decode('utf-8', 'replace'),
            },
        )
        route = self.routes.get(str(request.url))
        if route is None:
            return httpx.Response(404, text='not scripted')
        return _response(route)


class _Logs(logging.Handler):
    def __init__(self) -> None:
        super().__init__(logging.DEBUG)
        self.records: list[list[str]] = []

    def emit(self, record: logging.LogRecord) -> None:
        self.records.append([record.name, record.levelname, record.getMessage()])


class _Sessions:
    """Stands in for src.tool.site_sessions in post_links: an optional NGA login, and the checks recorded."""

    def __init__(self, login: dict[str, str] | None) -> None:
        self.login = login
        self.checks: list[list[Any]] = []

    async def load(self, site: str) -> SiteSession | None:
        if self.login is None:
            return None
        cookies = tuple(StoredCookie(name=name, value=value, domain='.nga.cn') for name, value in self.login.items())
        return SiteSession(
            site=site,
            cookies=cookies,
            account='42',
            status='valid',
            message='',
            expires_at=None,
            signed_in_at=None,
            checked_at=None,
        )

    async def record_check(self, site: str, *, ok: bool, message: str = '') -> None:
        self.checks.append([site, ok, message])


def _post_client(web: _Web) -> httpx.AsyncClient:
    # `post_links.new_client()` with the mock transport in place of the network.
    return httpx.AsyncClient(
        transport=httpx.MockTransport(web),
        headers={'User-Agent': post_links.USER_AGENT},
        timeout=60.0,
        follow_redirects=True,
        trust_env=False,
    )


_TIEBA_API = 'http://c.tieba.baidu.com/c/f/pb/page'
_TIEBA_PIC = 'http://tiebapic.baidu.com/forum/pic/item/{}.jpg?tbpicau=grant'
_TIEBA_VIDEO = 'http://tb-video.bdstatic.com/tieba-smallvideo-transcode/v.mp4?tbau=grant'
_TIEBA_COVER = 'http://tiebapic.baidu.com/forum/w%3D960/sign=x/cover.jpg?tbpicau=grant'


def _floor(floor: Any, *pictures: str) -> dict[str, Any]:
    content: list[Any] = [{'type': '0', 'text': f'floor {floor}'}]
    content += [
        {'type': '3', 'origin_src': _TIEBA_PIC.format(name), 'big_cdn_src': f'http://tiebapic.baidu.com/big/{name}.jpg'}
        for name in pictures
    ]
    return {'floor': floor, 'author': {'id': '7'}, 'content': content}


def _tieba_page(**overrides: Any) -> dict[str, Any]:
    page = {
        'error_code': '0',
        'thread': {'title': '其实HDR比dlss5重要', 'create_time': '1791654000', 'author': {'id': '7'}},
        'forum': {'name': '显卡'},
        'post_list': [_floor('1', 'A', 'B', 'C', 'B'), _floor('2', 'LATER')],
    }
    page.update(overrides)
    return page


_NGA_READ = 'https://bbs.nga.cn/read.php?tid=47695975&__output=11'
_NGA_CHECK = 'https://bbs.nga.cn/nuke.php?__lib=noti&__act=get_all&__output=11'
_NGA_LOGIN = {'ngaPassportUid': '42', 'ngaPassportCid': 'session-cid'}
_NGA_LINK = {'url': 'https://bbs.nga.cn/read.php?tid=47695975', 'title': '[同人搬运]贪吃蛇'}


def _nga_thread(**data_overrides: Any) -> dict[str, Any]:
    first = {
        'lou': 0,
        'subject': '',
        'postdatetimestamp': 1791654000,
        'content': (
            'text<br/>[img]./mon_202510/11/A.jpg.medium.jpg[/img]'
            '[img]https://img.nga.178.com/attachments/mon_202510/11/B.jpg[/img][img]./mon_202510/11/A.jpg[/img]'
        ),
        'attachs': {'0': {'attachurl': 'mon_202510/11/C.jpg', 'type': 'img'}, '1': {'attachurl': 'mon_202510/11/x.zip', 'type': 'zip'}},
    }
    reply = {'lou': 1, 'content': '[img]./mon_202510/11/LATER.jpg[/img]'}
    data = {
        '__R': {'0': first, '1': reply},
        '__T': {'subject': '[同人搬运]贪吃蛇 &amp; 更多', 'postdate': 1791654000},
        '__F': {'name': '二次元国家地理'},
    }
    data.update(data_overrides)
    return {'data': data, 'time': 1791654100}


_TIKHUB = 'http://api.tikhub.dev'
_ZHIHU_ANSWER = f'{_TIKHUB}/api/v1/zhihu/web/fetch_answer_detail?answer_id=9'
_ZHIHU_ARTICLE = f'{_TIKHUB}/api/v1/zhihu/web/fetch_column_article_detail?article_id=42'
_ZHIHU_PIN = f'{_TIKHUB}/api/v1/zhihu/web/fetch_pin_detail?pin_id=7'
_LENS = 'https://lens.zhihu.com/api/v4/videos/{}'
_LENS_OK = {'playlist': {'HD': {'width': 720, 'height': 1280, 'play_url': 'https://vdn3.vzuu.com/HD/v.mp4'}}}
_USER_INFO = f'{_TIKHUB}/api/v1/tikhub/user/get_user_info'

_ARTICLE = 'http://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123&chksm=feed'
_CAPTCHA = 'http://mp.weixin.qq.com/mp/wappoc_appmsgcaptcha?poc_token=x'
_JPEG = b'\xff\xd8\xff\xe0original-bytes'
_PNG = b'\x89PNG\r\n\x1a\nfull-size'
_MP4 = b'\x00\x00\x00\x18ftypmp42video-bytes'
_IMAGE = 'http://mmbiz.qpic.cn/sz_mmbiz_jpg/AAA/640?wx_fmt=jpeg'
_IMAGE_ORIGINAL = 'http://mmbiz.qpic.cn/sz_mmbiz_jpg/AAA/0?wx_fmt=jpeg'

_ARTICLE_PAGE = _PAGE.replace('https://mmbiz', 'http://mmbiz')


def _scenarios() -> list[dict[str, Any]]:  # noqa: PLR0915 - one entry per scenario
    out: list[dict[str, Any]] = []

    def add(name: str, call: str, args: dict[str, Any], routes: dict[str, Route], **extra: Any) -> None:
        out.append({'name': name, 'call': call, 'args': args, 'routes': routes, **extra})

    # tieba_post.fetch_tieba_post
    tieba = {'url': 'https://tieba.baidu.com/p/9876543210', 'title': 'link title'}
    add('tieba first floor', 'tieba_post.fetch_tieba_post', {'link': tieba}, {_TIEBA_API: _ok(json=_tieba_page())})
    video_floor = _floor(1)
    video_floor['content'].append({'type': 5, 'link': _TIEBA_VIDEO, 'src': _TIEBA_COVER, 'during_time': 13})
    video_thread = {
        'title': '这个数值够不够还原',
        'create_time': '1791984959',
        'video_info': {'video_url': _TIEBA_VIDEO, 'thumbnail_url': _TIEBA_COVER},
    }
    add(
        'tieba video in floor',
        'tieba_post.fetch_tieba_post',
        {'link': tieba},
        {_TIEBA_API: _ok(json=_tieba_page(thread=video_thread, forum={'name': '绝区零内鬼'}, post_list=[video_floor]))},
    )
    add(
        'tieba video in thread only',
        'tieba_post.fetch_tieba_post',
        {'link': tieba},
        {_TIEBA_API: _ok(json=_tieba_page(thread=video_thread, post_list=[_floor(1)]))},
    )
    add(
        'tieba refused',
        'tieba_post.fetch_tieba_post',
        {'link': tieba},
        {_TIEBA_API: _ok(json={'error_code': '4', 'error_msg': '贴子可能已被删除'})},
    )
    add('tieba refused without message', 'tieba_post.fetch_tieba_post', {'link': tieba}, {_TIEBA_API: _ok(json={'error_code': 0.0})})
    add('tieba integer zero', 'tieba_post.fetch_tieba_post', {'link': tieba}, {_TIEBA_API: _ok(json=_tieba_page(error_code=0))})
    add('tieba list', 'tieba_post.fetch_tieba_post', {'link': tieba}, {_TIEBA_API: _ok(json=['x'])})
    add('tieba not json', 'tieba_post.fetch_tieba_post', {'link': tieba}, {_TIEBA_API: _ok(text='<html>安全验证</html>')})
    add('tieba server error', 'tieba_post.fetch_tieba_post', {'link': tieba}, {_TIEBA_API: _ok(500, text='busy')})
    add(
        'tieba no first floor',
        'tieba_post.fetch_tieba_post',
        {'link': tieba},
        {_TIEBA_API: _ok(json=_tieba_page(post_list=[_floor('2', 'A'), 'x']))},
    )
    add(
        'tieba sparse thread',
        'tieba_post.fetch_tieba_post',
        {'link': tieba},
        {_TIEBA_API: _ok(json=_tieba_page(thread='x', forum={'name': ''}, post_list=[_floor(1.0, 'A'), _floor(1, 'B')]))},
    )
    add(
        'tieba odd create times',
        'tieba_post.fetch_tieba_post',
        {'link': tieba},
        {_TIEBA_API: _ok(json=_tieba_page(thread={'title': '', 'create_time': 1791654000}))},
    )
    add(
        'tieba superscript create time',
        'tieba_post.fetch_tieba_post',
        {'link': tieba},
        {_TIEBA_API: _ok(json=_tieba_page(thread={'create_time': '²'}))},
    )
    add(
        'tieba create time out of range',
        'tieba_post.fetch_tieba_post',
        {'link': tieba},
        {_TIEBA_API: _ok(json=_tieba_page(thread={'create_time': '1000000000000'}))},
    )
    add('tieba not a tieba link', 'tieba_post.fetch_tieba_post', {'link': {'url': 'https://example.com/p/1', 'title': ''}}, {})

    # nga_post.fetch_nga_post
    add('nga first floor', 'nga_post.fetch_nga_post', {'link': _NGA_LINK, 'cookies': _NGA_LOGIN}, {_NGA_READ: _ok(json=_nga_thread())})
    add(
        'nga cookies in order and only set ones',
        'nga_post.fetch_nga_post',
        {'link': _NGA_LINK, 'cookies': {'ngaPassportCid': 'cid', 'ngaPassportUid': '', 'other': 'x'}},
        {_NGA_READ: _ok(json=_nga_thread())},
    )
    add(
        'nga not signed in',
        'nga_post.fetch_nga_post',
        {'link': _NGA_LINK, 'cookies': _NGA_LOGIN},
        {_NGA_READ: _ok(403, json={'error': ['1:未登录', '<br/><a href=/nuke.php>[登录]</a>'], 'data': {'__MESSAGE': {}}})},
    )
    add(
        'nga 403 page',
        'nga_post.fetch_nga_post',
        {'link': _NGA_LINK, 'cookies': _NGA_LOGIN},
        {_NGA_READ: _ok(403, text='<html>403</html>')},
    )
    add('nga 401 empty', 'nga_post.fetch_nga_post', {'link': _NGA_LINK, 'cookies': _NGA_LOGIN}, {_NGA_READ: _ok(401)})
    add('nga 500 page', 'nga_post.fetch_nga_post', {'link': _NGA_LINK, 'cookies': _NGA_LOGIN}, {_NGA_READ: _ok(500, text='oops')})
    add('nga 403 json list', 'nga_post.fetch_nga_post', {'link': _NGA_LINK, 'cookies': _NGA_LOGIN}, {_NGA_READ: _ok(403, json=[1])})
    add(
        'nga deleted',
        'nga_post.fetch_nga_post',
        {'link': _NGA_LINK, 'cookies': _NGA_LOGIN},
        {_NGA_READ: _ok(json={'data': {'__MESSAGE': {'1': '帖子被锁定'}}})},
    )
    add(
        'nga error beside data',
        'nga_post.fetch_nga_post',
        {'link': _NGA_LINK, 'cookies': _NGA_LOGIN},
        {_NGA_READ: _ok(json={'error': ['12:x'], 'data': {}})},
    )
    add('nga no data', 'nga_post.fetch_nga_post', {'link': _NGA_LINK, 'cookies': _NGA_LOGIN}, {_NGA_READ: _ok(json={'time': 1})})
    add(
        'nga no first floor',
        'nga_post.fetch_nga_post',
        {'link': _NGA_LINK, 'cookies': _NGA_LOGIN},
        {_NGA_READ: _ok(json={'data': {'__R': [{'lou': 1}, 'x', {'lou': '2'}]}})},
    )
    add(
        'nga first floor in a list',
        'nga_post.fetch_nga_post',
        {'link': _NGA_LINK, 'cookies': _NGA_LOGIN},
        {
            _NGA_READ: _ok(
                json={'data': {'__R': [{'lou': '0', 'subject': ' Own &amp; subject ', 'postdatetimestamp': 'x'}], '__T': [], '__F': 'x'}}
            )
        },
    )
    add(
        'nga title from link',
        'nga_post.fetch_nga_post',
        {'link': _NGA_LINK, 'cookies': _NGA_LOGIN},
        {_NGA_READ: _ok(json={'data': {'__R': {'0': {'lou': 0.0, 'content': None}}, '__T': {'subject': '  ', 'postdate': '1791654000'}}})},
    )
    add(
        'nga postdate out of range',
        'nga_post.fetch_nga_post',
        {'link': _NGA_LINK, 'cookies': _NGA_LOGIN},
        {_NGA_READ: _ok(json={'data': {'__R': {'0': {'lou': 0}}, '__T': {'postdate': 10**18}}})},
    )
    add(
        'nga not a thread',
        'nga_post.fetch_nga_post',
        {'link': {'url': 'https://bbs.nga.cn/thread.php?fid=1', 'title': ''}, 'cookies': _NGA_LOGIN},
        {},
    )

    # nga_post.verify_session
    add('nga verified', 'nga_post.verify_session', {'cookies': _NGA_LOGIN}, {_NGA_CHECK: _ok(json={'data': {'0': []}, 'time': 1})})
    add(
        'nga turned away',
        'nga_post.verify_session',
        {'cookies': _NGA_LOGIN},
        {_NGA_CHECK: _ok(json={'error': ['你必须先登录论坛'], 'time': 1})},
    )
    add('nga half a login', 'nga_post.verify_session', {'cookies': {'ngaPassportUid': '42'}}, {})
    add('nga empty cid', 'nga_post.verify_session', {'cookies': {'ngaPassportUid': '42', 'ngaPassportCid': ''}}, {})
    add('nga bad gateway', 'nga_post.verify_session', {'cookies': _NGA_LOGIN}, {_NGA_CHECK: _ok(502, text='<html>bad gateway</html>')})
    add('nga json list', 'nga_post.verify_session', {'cookies': _NGA_LOGIN}, {_NGA_CHECK: _ok(json=[])})
    add('nga other error', 'nga_post.verify_session', {'cookies': _NGA_LOGIN}, {_NGA_CHECK: _ok(json={'error': ['服务器忙']})})
    add(
        'nga login message in data',
        'nga_post.verify_session',
        {'cookies': _NGA_LOGIN},
        {_NGA_CHECK: _ok(json={'data': {'__MESSAGE': {'1': '29:请先登录'}}})},
    )
    add(
        'nga redirect not followed',
        'nga_post.verify_session',
        {'cookies': _NGA_LOGIN},
        {_NGA_CHECK: _ok(302, headers={'location': '/login'})},
    )

    # zhihu_post.fetch_post
    answer = {'data': {'content': _ANSWER_HTML, 'author': {'name': 'a'}, 'question': {'title': 'Q'}, 'created_time': 1793533212}}
    add(
        'zhihu answer',
        'zhihu_post.fetch_post',
        {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': 'key'},
        {_ZHIHU_ANSWER: _ok(json=answer)},
    )
    add(
        'zhihu article with a trailing slash',
        'zhihu_post.fetch_post',
        {'kind': 'article', 'id': '42', 'api_url': _TIKHUB + '//', 'token': 'key'},
        {_ZHIHU_ARTICLE: _ok(json={'code': 200, 'data': {'title': 'Article', 'content': _ANSWER_HTML, 'author': {'name': 'a'}}})},
    )
    add(
        'zhihu pin',
        'zhihu_post.fetch_post',
        {'kind': 'pin', 'id': '7', 'api_url': _TIKHUB, 'token': 'key'},
        {_ZHIHU_PIN: _ok(json={'data': {'content': PIN_ITEMS[0], 'excerpt_title': 'pin', 'created': 1}})},
    )
    add('zhihu without a key', 'zhihu_post.fetch_post', {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': ''}, {})
    for status in (401, 402, 403):
        add(
            f'zhihu refused key {status}',
            'zhihu_post.fetch_post',
            {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': 'key'},
            {_ZHIHU_ANSWER: _ok(status, json={'detail': 'balance'})},
        )
    add(
        'zhihu bad request',
        'zhihu_post.fetch_post',
        {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': 'key'},
        {_ZHIHU_ANSWER: _ok(400, json={'detail': {'message': 'not found'}})},
    )
    add(
        'zhihu empty data',
        'zhihu_post.fetch_post',
        {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': 'key'},
        {_ZHIHU_ANSWER: _ok(json={'code': 200, 'data': {}})},
    )
    add(
        'zhihu data list',
        'zhihu_post.fetch_post',
        {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': 'key'},
        {_ZHIHU_ANSWER: _ok(json={'data': [1], 'detail': ''})},
    )
    add(
        'zhihu payload list',
        'zhihu_post.fetch_post',
        {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': 'key'},
        {_ZHIHU_ANSWER: _ok(json=[])},
    )
    add(
        'zhihu not json',
        'zhihu_post.fetch_post',
        {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': 'key'},
        {_ZHIHU_ANSWER: _ok(502, text='<html>gateway</html>')},
    )
    add(
        'zhihu error in data',
        'zhihu_post.fetch_post',
        {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': 'key'},
        {_ZHIHU_ANSWER: _ok(json={'data': {'error': {'code': 4041, 'message': '内容不存在'}}})},
    )
    add(
        'zhihu error without message',
        'zhihu_post.fetch_post',
        {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': 'key'},
        {_ZHIHU_ANSWER: _ok(json={'data': {'error': {'code': 1, 'name': 'x'}}})},
    )
    add(
        'zhihu error not a dict',
        'zhihu_post.fetch_post',
        {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': 'key'},
        {_ZHIHU_ANSWER: _ok(json={'data': {'error': 'x', 'question': {'title': 'still read'}}})},
    )

    # zhihu_post.resolve_video
    lens = {
        'playlist': {
            'LD': {'width': 476, 'height': 848, 'bitrate': 379.5, 'play_url': 'https://vdn3.vzuu.com/SD/v.mp4'},
            'HD': {'width': 720, 'height': 1280, 'bitrate': 559.1, 'play_url': 'https://vdn3.vzuu.com/HD/v.mp4'},
        },
    }
    add('lens widest entry', 'zhihu_post.resolve_video', {'video': {'id': '1'}}, {_LENS.format(1): _ok(json=lens)})
    add(
        'lens refused uses the fallback',
        'zhihu_post.resolve_video',
        {'video': {'id': '2', 'fallback_url': 'https://vdn3.vzuu.com/HD/p.mp4'}},
        {_LENS.format(2): _ok(403, json={'error': {'code': 40362}})},
    )
    add(
        'lens refused without fallback',
        'zhihu_post.resolve_video',
        {'video': {'id': '2'}},
        {_LENS.format(2): _ok(403, json={'error': {'code': 40362}})},
    )
    add('lens not json', 'zhihu_post.resolve_video', {'video': {'id': '3', 'fallback_url': 'f'}}, {_LENS.format(3): _ok(text='x')})
    add('lens list', 'zhihu_post.resolve_video', {'video': {'id': '3'}}, {_LENS.format(3): _ok(json=[1])})
    add(
        'lens empty playlist',
        'zhihu_post.resolve_video',
        {'video': {'id': '3', 'fallback_url': 'f'}},
        {_LENS.format(3): _ok(json={'playlist': {}})},
    )
    add(
        'lens bad width',
        'zhihu_post.resolve_video',
        {'video': {'id': '3'}},
        {_LENS.format(3): _ok(json={'playlist': [{'width': 'x', 'url': 'https://a'}]})},
    )

    # zhihu_post.fetch_page
    two_videos = _ANSWER_HTML + _ANSWER_HTML.replace('2091194612627121396', '1')
    add(
        'zhihu page drops a video without a play url',
        'zhihu_post.fetch_page',
        {'kind': 'answer', 'id': '9', 'api_url': _TIKHUB, 'token': 'key'},
        {
            _ZHIHU_ANSWER: _ok(json={'data': {'content': two_videos, 'author': {'name': 'a'}, 'question': {'title': 'Q'}}}),
            _LENS.format('2091194612627121396'): _ok(json=_LENS_OK),
            _LENS.format(1): _ok(403, json={'error': {'code': 40362}}),
        },
    )
    add(
        'zhihu pin page uses its own playlist',
        'zhihu_post.fetch_page',
        {'kind': 'pin', 'id': '7', 'api_url': _TIKHUB, 'token': 'key'},
        {_ZHIHU_PIN: _ok(json={'data': {'content': PIN_ITEMS[0], 'author': {'name': 'p'}, 'created': 1793533212}})},
    )

    # post_links.fetch_page
    article = {'url': _ARTICLE, 'title': 'card title'}
    add(
        'article page',
        'post_links.fetch_page',
        {'link': article},
        {_ARTICLE: _ok(text=_ARTICLE_PAGE, headers={'content-type': 'text/html; charset=utf-8'})},
    )
    add(
        'article behind the captcha',
        'post_links.fetch_page',
        {'link': article},
        {_ARTICLE: _ok(302, headers={'location': _CAPTCHA}), _CAPTCHA: _ok(text='captcha')},
    )
    add(
        'article captcha answered with an error',
        'post_links.fetch_page',
        {'link': article},
        {_ARTICLE: _ok(302, headers={'location': _CAPTCHA})},
    )
    add('article gone', 'post_links.fetch_page', {'link': article}, {_ARTICLE: _ok(404, text='gone')})
    add(
        'article redirect to the page',
        'post_links.fetch_page',
        {'link': article},
        {_ARTICLE: _ok(301, headers={'location': '/s/abc'}), 'http://mp.weixin.qq.com/s/abc': _ok(text='var ct = "x";')},
    )
    add(
        'article short link fails',
        'post_links.fetch_page',
        {'link': {'url': 'http://mp.weixin.qq.com/s/abc?scene=1', 'title': ''}},
        {'http://mp.weixin.qq.com/s/abc?scene=1': _ok(500)},
    )
    add('article out-of-range time', 'post_links.fetch_page', {'link': article}, {_ARTICLE: _ok(text='var ct = "1000000000000";')})
    add('article not utf-8', 'post_links.fetch_page', {'link': article}, {_ARTICLE: _ok(body=b"var msg_title = '\xff\xfe';")})
    add('tieba through post links', 'post_links.fetch_page', {'link': tieba}, {_TIEBA_API: _ok(json=_tieba_page())})
    add('tieba refused through post links', 'post_links.fetch_page', {'link': tieba}, {_TIEBA_API: _ok(json={'error_code': '4'})})
    add('tieba server error through post links', 'post_links.fetch_page', {'link': tieba}, {_TIEBA_API: _ok(503)})
    add('nga through post links', 'post_links.fetch_page', {'link': _NGA_LINK, 'login': _NGA_LOGIN}, {_NGA_READ: _ok(json=_nga_thread())})
    add('nga without a login', 'post_links.fetch_page', {'link': _NGA_LINK, 'login': None}, {})
    add('nga with half a login', 'post_links.fetch_page', {'link': _NGA_LINK, 'login': {'ngaPassportCid': 'session-cid'}}, {})
    add(
        'nga login expired',
        'post_links.fetch_page',
        {'link': _NGA_LINK, 'login': _NGA_LOGIN},
        {_NGA_READ: _ok(403, json={'error': ['1:未登录']})},
    )
    add(
        'nga deleted through post links',
        'post_links.fetch_page',
        {'link': _NGA_LINK, 'login': _NGA_LOGIN},
        {_NGA_READ: _ok(json={'error': ['帖子不存在']})},
    )
    add(
        'zhihu through post links',
        'post_links.fetch_page',
        {'link': {'url': 'https://zhuanlan.zhihu.com/p/42', 'title': ''}, 'tikhub': {'api_url': _TIKHUB, 'token': 'service-key'}},
        {_ZHIHU_ARTICLE: _ok(json={'data': {'title': 'T', 'content': '<img data-original="https://pic1.zhimg.com/v2-a_r.jpg"/>'}})},
    )
    add(
        'zhihu without a key through post links',
        'post_links.fetch_page',
        {'link': {'url': 'https://www.zhihu.com/pin/7', 'title': ''}, 'tikhub': {'api_url': _TIKHUB, 'token': ''}},
        {},
    )
    add(
        'zhihu refused through post links',
        'post_links.fetch_page',
        {'link': {'url': 'https://www.zhihu.com/pin/7', 'title': ''}, 'tikhub': {'api_url': _TIKHUB, 'token': 'key'}},
        {_ZHIHU_PIN: _ok(402, json={})},
    )
    add(
        'zhihu deleted through post links',
        'post_links.fetch_page',
        {'link': {'url': 'https://www.zhihu.com/pin/7', 'title': ''}, 'tikhub': {'api_url': _TIKHUB, 'token': 'key'}},
        {_ZHIHU_PIN: _ok(404, json={'detail': 'gone'})},
    )

    # post_links.fetch_image
    jpeg = {'content-type': 'image/jpeg'}
    add('image at full size', 'post_links.fetch_image', {'url': _IMAGE}, {_IMAGE_ORIGINAL: _ok(body=_JPEG, headers=jpeg)})
    add(
        'image falls back to the page size',
        'post_links.fetch_image',
        {'url': _IMAGE},
        {_IMAGE_ORIGINAL: _ok(404), _IMAGE: _ok(body=_PNG, headers={'content-type': 'image/jpeg'})},
    )
    add('image at neither size', 'post_links.fetch_image', {'url': _IMAGE}, {_IMAGE_ORIGINAL: _ok(500), _IMAGE: _ok(403)})
    add(
        'image refused with error-message',
        'post_links.fetch_image',
        {'url': 'http://tiebapic.baidu.com/expired.jpg'},
        {
            'http://tiebapic.baidu.com/expired.jpg': _ok(
                body=b'\x89PNGlogo', headers={'content-type': 'image/png', 'error-message': 'expired'}
            )
        },
    )
    add(
        'image that is html',
        'post_links.fetch_image',
        {'url': 'http://pic1.zhimg.com/a.jpg'},
        {'http://pic1.zhimg.com/a.jpg': _ok(text='<html>', headers={'content-type': 'text/html'})},
    )
    add(
        'image without content type',
        'post_links.fetch_image',
        {'url': 'http://pic1.zhimg.com/a.jpg'},
        {'http://pic1.zhimg.com/a.jpg': _ok(body=_JPEG)},
    )
    add(
        'image empty',
        'post_links.fetch_image',
        {'url': 'http://pic1.zhimg.com/a.jpg'},
        {'http://pic1.zhimg.com/a.jpg': _ok(body=b'', headers={'content-type': 'image/jpeg'})},
    )
    add(
        'image named by its content type',
        'post_links.fetch_image',
        {'url': 'http://mmbiz.qpic.cn/a/0?wx_fmt=png'},
        {'http://mmbiz.qpic.cn/a/0?wx_fmt=png': _ok(body=b'BMxxxx', headers={'content-type': 'Image/WebP'})},
    )
    add(
        'image named by its wx_fmt',
        'post_links.fetch_image',
        {'url': 'http://mmbiz.qpic.cn/a/0?wx_fmt=gif'},
        {'http://mmbiz.qpic.cn/a/0?wx_fmt=gif': _ok(body=b'????', headers={'content-type': 'image/x-icon'})},
    )
    add(
        'image redirected',
        'post_links.fetch_image',
        {'url': 'http://pic1.zhimg.com/r.jpg'},
        {
            'http://pic1.zhimg.com/r.jpg': _ok(302, headers={'location': 'http://pic2.zhimg.com/r.jpg'}),
            'http://pic2.zhimg.com/r.jpg': _ok(body=_JPEG, headers=jpeg),
        },
    )

    # post_links.fetch_video
    video = 'http://vdn3.vzuu.com/HD/v.mp4'
    add('video saved', 'post_links.fetch_video', {'video': {'url': video}}, {video: _ok(body=_MP4, headers={'content-type': 'video/mp4'})})
    add('video not an mp4', 'post_links.fetch_video', {'video': {'url': video}}, {video: _ok(body=b'<html>nope</html>')})
    add('video refused', 'post_links.fetch_video', {'video': {'url': video, 'cover_url': 'c'}}, {video: _ok(401)})
    add('video empty', 'post_links.fetch_video', {'video': {'url': video}}, {video: _ok(body=b'')})

    # tikhub.check_account
    add('tikhub not configured', 'tikhub.check_account', {'api_url': _TIKHUB, 'token': ''}, {})
    add(
        'tikhub account',
        'tikhub.check_account',
        {'api_url': _TIKHUB, 'token': 'key'},
        {_USER_INFO: _ok(json={'user_data': {'balance': 12.5, 'free_credit': 0}, 'api_key_data': {'api_key_name': 'fav'}})},
    )
    add(
        'tikhub odd numbers',
        'tikhub.check_account',
        {'api_url': _TIKHUB, 'token': 'key'},
        {_USER_INFO: _ok(json={'user_data': {'balance': ' 1_000.5 ', 'free_credit': 'abc'}, 'api_key_data': {'api_key_name': None}})},
    )
    add(
        'tikhub more numbers',
        'tikhub.check_account',
        {'api_url': _TIKHUB, 'token': 'key'},
        {_USER_INFO: _ok(json={'user_data': {'balance': True, 'free_credit': []}, 'api_key_data': []})},
    )
    add(
        'tikhub string numbers',
        'tikhub.check_account',
        {'api_url': _TIKHUB, 'token': 'key'},
        {_USER_INFO: _ok(json={'user_data': {'balance': '١٢.5', 'free_credit': '-1e3'}, 'api_key_data': {'api_key_name': 7}})},
    )
    add(
        'tikhub missing numbers',
        'tikhub.check_account',
        {'api_url': _TIKHUB, 'token': 'key'},
        {_USER_INFO: _ok(json={'user_data': {}})},
    )
    add(
        'tikhub float edge',
        'tikhub.check_account',
        {'api_url': _TIKHUB, 'token': 'key'},
        {_USER_INFO: _ok(json={'user_data': {'balance': '1__0', 'free_credit': '_1'}})},
    )
    for status in (401, 402, 403):
        add(
            f'tikhub refused {status}',
            'tikhub.check_account',
            {'api_url': _TIKHUB, 'token': 'key'},
            {_USER_INFO: _ok(status, json={'detail': 'x'})},
        )
    add(
        'tikhub server error',
        'tikhub.check_account',
        {'api_url': _TIKHUB, 'token': 'key'},
        {_USER_INFO: _ok(500, json={'user_data': {'balance': 1}})},
    )
    add('tikhub not json', 'tikhub.check_account', {'api_url': _TIKHUB, 'token': 'key'}, {_USER_INFO: _ok(text='<html>')})
    add('tikhub no user data', 'tikhub.check_account', {'api_url': _TIKHUB, 'token': 'key'}, {_USER_INFO: _ok(json={'user_data': None})})
    add('tikhub list', 'tikhub.check_account', {'api_url': _TIKHUB, 'token': 'key'}, {_USER_INFO: _ok(json=[1])})
    add(
        'tikhub redirect not followed',
        'tikhub.check_account',
        {'api_url': _TIKHUB, 'token': 'key'},
        {_USER_INFO: _ok(307, headers={'location': '/x'})},
    )

    for case in out:
        case.update(_run(case))
    return out


def _article_link(args: dict[str, Any]) -> ArticleLink:
    return _link(**args['link'])


async def _call(case: dict[str, Any], web: _Web, tmp: Path) -> Any:  # noqa: C901, PLR0911 - one branch per call under test
    args = case['args']
    call = case['call']
    transport = httpx.MockTransport(web)
    if call == 'nga_post.verify_session':
        cookies = tuple(StoredCookie(name=name, value=value, domain='.nga.cn') for name, value in args['cookies'].items())
        return await nga_post.verify_session(cookies, transport=transport)
    if call == 'tikhub.check_account':
        return await tikhub.check_account(settings.TikHub(api_url=args['api_url'], token=args['token']), transport=transport)
    async with _post_client(web) as client:
        if call == 'tieba_post.fetch_tieba_post':
            return await tieba_post.fetch_tieba_post(client, _article_link(args))
        if call == 'nga_post.fetch_nga_post':
            return await nga_post.fetch_nga_post(client, _article_link(args), args['cookies'])
        if call == 'zhihu_post.fetch_post':
            return await zhihu_post.fetch_post(client, ZhihuLink(args['kind'], args['id']), api_url=args['api_url'], token=args['token'])
        if call == 'zhihu_post.fetch_page':
            return await zhihu_post.fetch_page(client, ZhihuLink(args['kind'], args['id']), api_url=args['api_url'], token=args['token'])
        if call == 'zhihu_post.resolve_video':
            return await zhihu_post.resolve_video(client, ZhihuVideo(**args['video']))
        if call == 'post_links.fetch_page':
            tikhub_cfg = settings.load().services.tikhub
            tikhub_cfg.api_url, tikhub_cfg.token = args.get('tikhub', {}).get('api_url', _TIKHUB), args.get('tikhub', {}).get('token', '')
            try:
                return await post_links.fetch_page(client, _article_link(args))
            finally:
                tikhub_cfg.api_url, tikhub_cfg.token = settings.TikHub().api_url, ''
        if call == 'post_links.fetch_image':
            return await post_links.fetch_image(client, args['url'])
        if call == 'post_links.fetch_video':
            partial = tmp / 'video.mp4.part'
            ok = await post_links.fetch_video(client, PostVideo(**args['video']), partial)
            return {'ok': ok, 'file': partial.read_bytes() if partial.exists() else None}
    msg = f'unknown call {call}'
    raise AssertionError(msg)


def _run(case: dict[str, Any]) -> dict[str, Any]:
    web = _Web(case['routes'])
    sessions = _Sessions(case['args'].get('login'))
    logs = _Logs()
    root = logging.getLogger('embyx')
    root.addHandler(logs)
    previous_level = root.level
    root.setLevel(logging.DEBUG)
    original_sessions = post_links.site_sessions
    post_links.site_sessions = sessions  # type: ignore[assignment]
    try:
        with tempfile.TemporaryDirectory() as tmp:
            try:
                result = _plain(asyncio.run(_call(case, web, Path(tmp))))
            except Exception as exc:  # noqa: BLE001 - the error is the recorded answer
                result = _error(exc)
                if isinstance(exc, PostCaptchaError):
                    result['captcha'] = True
    finally:
        post_links.site_sessions = original_sessions  # type: ignore[assignment]
        root.removeHandler(logs)
        root.setLevel(previous_level)
    return {'requests': web.requests, 'result': result, 'logs': logs.records, 'checks': sessions.checks}


def _constants() -> list[dict[str, Any]]:
    return [
        {
            'post_links.USER_AGENT': post_links.USER_AGENT,
            'post_links.VIDEO_READ_SECONDS': post_links.VIDEO_READ_SECONDS,
            'post_links._REQUEST_TIMEOUT_SECONDS': post_links._REQUEST_TIMEOUT_SECONDS,  # noqa: SLF001
            'zhihu_post.USER_AGENT': zhihu_post.USER_AGENT,
            'nga_post.NGA_HOSTS': sorted(nga_post.NGA_HOSTS),
            'nga_post.READ_URL': nga_post.READ_URL,
            'nga_post._SESSION_CHECK_URL': nga_post._SESSION_CHECK_URL,  # noqa: SLF001
            'nga_post.LOGIN_COOKIES': list(nga_post.LOGIN_COOKIES),
            'nga_post.SESSION_SITE': nga_post.SESSION_SITE,
            'nga_post.LOGIN_URL': nga_post.LOGIN_URL,
            'nga_post.SESSION_COOKIE_URLS': list(nga_post.SESSION_COOKIE_URLS),
            'nga_post.SESSION_COOKIE': nga_post.SESSION_COOKIE,
            'nga_post.RELOGIN_HINT': nga_post.RELOGIN_HINT,
            'wechat_article.LINK_SHARE_TYPE': wechat_article.LINK_SHARE_TYPE,
            'wechat_article.VIDEO_SHARE_TYPE': wechat_article.VIDEO_SHARE_TYPE,
            'wechat_article.IMAGE_POST': wechat_article.IMAGE_POST,
            'wechat_article.CAPTCHA_PATHS': list(wechat_article.CAPTCHA_PATHS),
            'tikhub.ACCOUNT_STATUSES': sorted(tikhub.ACCOUNT_STATUSES),
        },
    ]


GOLDENS = {
    'post_links/parsing.json': _parsing,
    'post_links/fetch.json': _scenarios,
    'post_links/constants.json': _constants,
}

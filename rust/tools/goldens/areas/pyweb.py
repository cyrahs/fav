"""Python's text helpers for scraped pages (WP6): `html.unescape`, the `HTMLParser`
tokenizer, `urllib.parse` and `pathlib` arithmetic.

Stella Sora parses the wiki with `HTMLParser` subclasses and builds URLs, titles and
file names with `urljoin`, `unquote` and `Path`; later sources unescape with
`html.unescape`. These goldens record what CPython 3.12 does on markup and links chosen to
hit the tokenizer's corner cases, so `fav_core::pyhtml`, `pyurl` and `pypath` can be held
to it.
"""

from __future__ import annotations

from html import unescape
from html.entities import html5
from html.parser import HTMLParser
from pathlib import PurePosixPath
from typing import Any
from urllib.parse import unquote, urljoin, urlsplit

import httpx

UNESCAPE_CASES: list[str] = [
    'plain',
    '&amp; &lt; &gt; &quot; &#39;',
    '&amp &lt &gt &copy',  # legacy names without `;`
    '&ampx &notit; &notin; &noti &not',  # longest-prefix rule
    '&#65;&#x41;&#X41;&#0065',
    '&#0; &#13; &#128; &#129; &#150; &#159;',
    '&#xD800; &#xDFFF; &#x110000; &#99999999999999999999;',
    '&#1; &#11; &#x7f; &#xfdd0; &#xfffe; &#x1ffff; &#x10ffff;',
    '&#; &#x; &#xg; &# &',
    '&unknown; &zz &a',
    '&ThisNameIsLongerThanThirtyTwoCharactersSurely;',
    '&amp\n&amp\t&amp<&amp&&amp#',
    '&lt;script&gt;&amp;lt;',
    '中&amp;文 &中; &#x4e2d;',
    '&nbsp;&NewLine;&Tab;&bne;&nGt;',
    '&AMP;&Amp;&aMp;',
]

HTML_CASES: list[str] = [
    # MediaWiki-shaped markup like the Stella Sora pages.
    ('<table id="trekker-table" class="wikitable sortable"><tr><td><a href="/wiki/Freesia" title="Freesia">Freesia</a></td></tr></table>'),
    (
        '<div class="mw-heading"><h2 id="CGs">CGs</h2><span class="mw-editsection">'
        '[<a href="/w/index.php?title=X&amp;action=edit&amp;section=1">edit</a>]</span></div>'
    ),
    (
        '<li class="gallerybox"><a href="/wiki/File:Disc_Good_Night.png" class="mw-file-description">'
        '<img src="//static.wikitide.net/x.png" decoding="async" width="120"></a></li>'
    ),
    (
        '<aside class="portable-infobox pi-theme-default"><figure><a href="//static.wikitide.net/a/b/F.png" '
        'class="image image-thumbnail" title="Profile Image"><img/></a></figure></aside>'
    ),
    # Attribute syntax.
    '<a href=/wiki/Bare title=bare>x</a>',
    "<a href='/wiki/Single' title='it&#39;s'>x</a>",
    '<a HREF="/wiki/Upper" Title="T">x</a>',
    '<A href="/x">Upper tag</A>',
    '<a href="/x" href="/y" disabled>dup</a>',
    '<a href = "/spaced" title =  bare >x</a>',
    '<a href=  >empty after spaces</a>',
    '<a href=>empty</a>',
    '<a href="/x"title="no space">x</a>',
    '<a href="&amp;&lt;&#x41;&bogus;">refs</a>',
    "<a b='x>y</a> after",
    "<a b= 'unclosed>text</a>",
    "<a b ='unclosed>text</a>",
    '<a "quoted-name"=1>x</a>',
    '<a =eq>x</a>',
    '<a b="1"/>selfclose',
    '<br/><br /><img src=x/>',
    '<a/b/c>slashes</a>',
    '<a\tb="1"\nc=\'2\'\rd=3\x0ce>ws</a>',
    '<a href="/x"<b>',
    '<a b="c" d e=f g="h"i="j">x</a>',
    '<div data-x="a>b">gt in value</div>',
    '<p class=a/>bare slash value</p>',
    # Text, stray `<` and character references in text.
    'text &amp; more &lt;tag&gt; &copy 2024 &#169;',
    'a<3b <  c < d',
    '<p>&amp;lt; double</p>',
    'tail without tags &amp; ok',
    'tail cut &am',
    'tail cut &amp;x &#12',
    # Comments, declarations, processing instructions.
    '<!DOCTYPE html><html><!-- a <a href="/hidden">x</a> --><a href="/shown">y</a></html>',
    '<!-- unterminated <a href="/never">',
    '<!--> abrupt <b>x</b>',
    '<!---> abrupt <b>y</b>',
    '<!-- ok --!> bang <b>z</b>',
    '<!-- a -- b --> <i>c</i>',
    '<![CDATA[ <a href="/in-cdata"> ]]><b>after</b>',
    '<![if !IE]><b>cond</b><![endif]>',
    '<!bogus decl><b>after bogus</b>',
    '<?xml version="1.0"?><b>after pi</b>',
    '</><b>empty end</b></ b>bogus end</1>x',
    '</a',
    '<a href="/x"',
    # Raw text and RCDATA elements.
    '<script>if (a < b && c > d) { x = "</div>"; }</script><a href="/after-script">x</a>',
    '<SCRIPT type="x">a</Script ><b>y</b>',
    '<script>never closed <a href="/no">',
    '<style>p{color:red}</style><b>s</b>',
    '<title>A &amp; B <b>not a tag</b></title><b>t</b>',
    '<textarea>&lt;x&gt;</textarea>',
    '<script>a</scriptx>b</script><b>c</b>',
    '<xmp><b></xmp><i>i</i>',
    '<plaintext><a href="/x">raw to the end',
    '<script/>self-closed script<b>x</b>',
    # Non-ASCII.
    '<a href="/wiki/%E4%B8%AD%E6%96%87" title="中文">中文</a>',
    '<a title="😀">emoji &#x1F600;</a>',
    '<tağ attr="v">unicode tag</tağ>',
]

UNQUOTE_CASES: list[str] = [
    'File:Disc_Good_Night.png',
    'File:Disc%20Good%20Night.png',
    '%E4%B8%AD%E6%96%87',
    '%e4%b8%ad',
    '%E4%B8',
    '%FF%FE%41',
    '%C3%28',
    '%ED%A0%80',
    '%F0%9F%98%80',
    '%zz%4',
    '%',
    '100%',
    '中文%20混合',
    '%2Fslash%3Aname%23',
    '+plus+',
]

URLJOIN_CASES: list[tuple[str, str]] = [
    ('https://stellasora.miraheze.org', '/wiki/Freesia'),
    ('https://stellasora.miraheze.org', '//static.wikitide.net/stellasorawiki/2/2f/Freesia.png'),
    ('https://stellasora.miraheze.org', 'https://other.example/x'),
    ('https://stellasora.miraheze.org', 'relative/path'),
    ('https://stellasora.miraheze.org/wiki/Page', 'File:X.png'),
    ('https://stellasora.miraheze.org/wiki/Page', '../up'),
    ('https://stellasora.miraheze.org/wiki/Page', './here/./x/..'),
    ('https://stellasora.miraheze.org/wiki/Page', '?q=1'),
    ('https://stellasora.miraheze.org/wiki/Page?a=b', '#frag'),
    ('https://stellasora.miraheze.org/wiki/Page;p', ''),
    ('https://stellasora.miraheze.org/a/b/c', 'g;x?y#s'),
    ('https://stellasora.miraheze.org/a/b/c', '../../../../g'),
    ('https://stellasora.miraheze.org/a//b/c', 'd//e'),
    ('https://stellasora.miraheze.org', 'mailto:x@y'),
    ('https://stellasora.miraheze.org', 'javascript:void(0)'),
    ('https://stellasora.miraheze.org', ' /wiki/Leading_space'),
    ('https://stellasora.miraheze.org', '/wiki/Tab\there'),
    ('https://stellasora.miraheze.org', 'HTTPS://UPPER.example/X'),
    ('https://stellasora.miraheze.org', 'http:/wiki/x'),
    ('https://stellasora.miraheze.org', '//[::1]:8080/x'),
    ('https://stellasora.miraheze.org', '//[::1/x'),
    ('https://stellasora.miraheze.org', '//[1.2.3.4]/x'),
    ('https://stellasora.miraheze.org', '//[v1.fe]/x'),
    ('https://stellasora.miraheze.org', '//a[::1]/x'),
    ('https://stellasora.miraheze.org', '//host℀x/p'),
    ('https://stellasora.miraheze.org', '//ｈｏｓｔ/full-width'),  # noqa: RUF001 - NFKC changes it, but not into URL syntax
    ('', '/x'),
    ('http://a/b', 'c:d'),
]

SPLIT_CASES: list[str] = [
    'https://i.pximg.net/img-original/img/2024/01/01/00/00/00/1_p0.jpg',
    'https://i.pximg.net/no-extension',
    'https://i.pximg.net/a.b/c?x=1.png#y.gif',
    '/wiki/File:A.png',
    'no-scheme.jpg',
    '1:2/x.png',
    'HTTP://Host/Path.PNG',
]

PATH_CASES: list[str] = [
    '/img/x/1_p0.jpg',
    '/no-extension',
    'a/b.tar.gz',
    '.hidden',
    'trailing.',
    'dir/',
    'dir/.',
    '/',
    '',
    'A/B.png',
    'Disc_Good_Night.png',
    '..',
    'a/..',
    'x.',
    'x..png',
    '中文.PNG',
]

JOIN_CASES: list[tuple[str, str]] = [
    ('collection/pixiv', 'artist'),
    ('./collection/stellasora', 'disc'),
    ('/data', ''),
    ('/data', '/abs'),
    ('.', 'x'),
    ('a/', 'b//c/'),
    ('collection', 'a/./b'),
    ('//net', 'x'),
]


class _Recorder(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.events: list[list[Any]] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        self.events.append(['start', tag, [list(pair) for pair in attrs]])

    def handle_endtag(self, tag: str) -> None:
        self.events.append(['end', tag])

    def handle_data(self, data: str) -> None:
        self.events.append(['data', data])


def _events() -> list[dict[str, Any]]:
    cases = []
    for html in HTML_CASES:
        parser = _Recorder()
        parser.feed(html)
        # No close(): fav's parsers never call it, so unfinished input stays unreported.
        cases.append({'html': html, 'events': parser.events})
    return cases


def _unescape() -> list[dict[str, str]]:
    return [{'text': text, 'result': unescape(text)} for text in UNESCAPE_CASES]


def _entities() -> list[list[str]]:
    return [[name, value] for name, value in sorted(html5.items())]


def _unquote() -> list[dict[str, str]]:
    return [{'text': text, 'result': unquote(text)} for text in UNQUOTE_CASES]


def _outcome(call: Any) -> dict[str, str]:
    try:
        return {'result': call()}
    except ValueError as exc:
        return {'error': str(exc)}


def _urljoin() -> list[dict[str, Any]]:
    return [{'base': base, 'url': url, **_outcome(lambda base=base, url=url: urljoin(base, url))} for base, url in URLJOIN_CASES]


def _urlsplit() -> list[dict[str, Any]]:
    cases = []
    for url in SPLIT_CASES:
        parts = urlsplit(url)
        cases.append({'url': url, 'parts': list(parts)})
    return cases


def _paths() -> list[dict[str, Any]]:
    parts = [
        {'path': path, 'name': PurePosixPath(path).name, 'suffix': PurePosixPath(path).suffix, 'stem': PurePosixPath(path).stem}
        for path in PATH_CASES
    ]
    joins = [{'base': base, 'part': part, 'joined': str(PurePosixPath(base) / part)} for base, part in JOIN_CASES]
    return [{'parts': parts, 'joins': joins}]


def _reason_phrases() -> list[list[Any]]:
    # `raise_for_status()` names the status with httpx's table (no status line to read it from).
    return [[code, httpx.codes.get_reason_phrase(code)] for code in range(100, 600)]


STATUS_ERROR_CASES: list[tuple[int, str, str | None]] = [
    (200, 'https://example.com/ok', None),
    (204, 'https://example.com/empty', None),
    (101, 'https://example.com/upgrade', None),
    (301, 'https://example.com/old', 'https://example.com/new'),
    (302, 'https://example.com/a?b=1', '/relative'),
    (304, 'https://example.com/cached', 'https://example.com/elsewhere'),
    (307, 'https://example.com/tmp', None),
    (400, 'https://www.pixiv.net/ajax/user/1/illusts/bookmarks?tag=&offset=0&limit=48&rest=show', None),
    (403, 'https://i.pximg.net/img-original/img/x/1_p0.jpg', None),
    (404, 'https://stellasora.miraheze.org/wiki/Characters', None),
    (418, 'https://example.com/teapot', None),
    (429, 'https://example.com/slow', 'https://example.com/ignored'),
    (499, 'https://example.com/odd', None),
    (500, 'https://example.com/boom', None),
    (503, 'https://example.com/busy', None),
    (599, 'https://example.com/unknown', None),
]


def _status_errors() -> list[dict[str, Any]]:
    # `str(exc)` of `raise_for_status()`; `None` when the status passes.
    cases = []
    for status, url, location in STATUS_ERROR_CASES:
        headers = {'location': location} if location is not None else {}
        response = httpx.Response(status, headers=headers, request=httpx.Request('GET', url))
        try:
            response.raise_for_status()
            message = None
        except httpx.HTTPStatusError as exc:
            message = str(exc)
        cases.append({'status': status, 'url': url, 'location': location, 'message': message})
    return cases


GOLDENS = {
    'pyweb/httpx_reason_phrases.json': _reason_phrases,
    'pyweb/httpx_status_errors.json': _status_errors,
    'pyweb/unescape.json': _unescape,
    'pyweb/entities.json': _entities,
    'pyweb/html_events.json': _events,
    'pyweb/unquote.json': _unquote,
    'pyweb/urljoin.json': _urljoin,
    'pyweb/urlsplit.json': _urlsplit,
    'pyweb/paths.json': _paths,
}

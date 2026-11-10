# ruff: noqa: RUF001
# (The full-width punctuation and look-alike letters in this file are the inputs under test.)
"""Hanime1 (WP8): the t2s conversion and every page parser and pure helper.

`t2s.json` runs `opencc-python-reimplemented`'s own `OpenCC('t2s')` (through
`runtime_config.to_simplified_chinese` and the converter's `convert`) over every key of its
two dictionaries, every phrase in context, and deterministic mixes of keys, values,
separators, whitespace, kana, ASCII and characters outside the BMP. `t2s_dictionaries.json`
holds the sha256 and the parsed shape (key lengths, entry count) of the two dictionary files
the Rust build embeds, so a copy that drifts from the package fails.

`parsing.json` calls the real parsers of `src/web/hanime1.py`, `src/tool/runtime_config.py`
and `src/tool/hanime1_author.py` on the HTML of the Python tests plus awkward variants
(attribute order, quotes, case, entities, escaped URLs, nesting, unclosed blocks,
Cloudflare pages), and the small helpers (quality labels, file sizes, ids, seeds, URLs,
the yt-dlp command, output directories). An exception is recorded as `Class: message`.
"""

from __future__ import annotations

import hashlib
from dataclasses import asdict, is_dataclass
from pathlib import Path
from typing import Any

import opencc
from src.tool import hanime1_author, runtime_config
from src.web import hanime1
from src.web.hanime1 import Hanime1, HanimeRecord, WatchSeries, WatchSeriesVideo

_CONVERTER = opencc.OpenCC('t2s')
_DICTIONARY_DIR = Path(opencc.__file__).resolve().parent / 'dictionary'
_DICTIONARIES = ('TSPhrases.txt', 'TSCharacters.txt')


def _attempt(call: Any) -> Any:
    try:
        return _plain(call())
    except Exception as exc:  # noqa: BLE001 - the error is the recorded answer
        return {'error': f'{exc.__class__.__name__}: {exc}'}


def _plain(value: Any) -> Any:
    if is_dataclass(value) and not isinstance(value, type):
        return {key: _plain(item) for key, item in asdict(value).items()}
    if isinstance(value, tuple | list):
        return [_plain(item) for item in value]
    if isinstance(value, dict):
        return {key: _plain(item) for key, item in value.items()}
    if isinstance(value, Path):
        return str(value)
    return value


# ---------- t2s ----------


def _loaded() -> list[tuple[int, int, dict[str, str]]]:
    _CONVERTER._init_dict()  # noqa: SLF001
    [group] = _CONVERTER._dict_chain_data  # noqa: SLF001
    return list(group)


def _t2s_dictionaries() -> list[dict[str, Any]]:
    cases = []
    for name, (max_len, min_len, entries) in zip(_DICTIONARIES, _loaded(), strict=True):
        data = (_DICTIONARY_DIR / name).read_bytes()
        cases.append(
            {
                'file': name,
                'sha256': hashlib.sha256(data).hexdigest(),
                'max_len': max_len,
                'min_len': min_len,
                'entries': len(entries),
                'multi_valued': sum(1 for value in entries.values() if ' ' in value),
            },
        )
    return cases


_SEPARATORS = [' ', '  ', '\t', '\n', '\x1c', '\x1f', '\x85', '\xa0', '　', ' ', '-', ',', '.', '?', '!', '*']
_SEPARATORS += list(
    '，。、；：？！…“”‘’『』「」﹁﹂—－（）《》〈〉～．／＼︒︑︔︓︿﹀︹︺︙︐［﹇］﹈︕︖︰︳︴︽︾︵︶｛︷｝︸﹃﹄【︻】︼'
)
_EXTRA = ['', 'a', 'OVA', 'Hanime1', '123', 'ぁあ', 'カタカナ', 'ﾊﾝｶｸ', '😀', '𠀀', '\u200b', '﻿', '[新番預告]', '{id-12488}', '\x00']


def _t2s_inputs() -> list[str]:
    phrases, characters = (sorted(entries) for _, _, entries in _loaded())
    phrase_values = sorted(set(_loaded()[0][2].values()))
    character_values = sorted(set(_loaded()[1][2].values()))
    inputs: list[str] = ['', ' ', '\x1c', '　屈辱　', ' \t\n', '屈辱', '戀愛始於辣妹的命運 2', '射精深陷主義 1']
    inputs += characters
    inputs += phrases
    inputs += [f'{phrase}{phrase}' for phrase in phrases]
    inputs += [f'動{phrase}後' for phrase in phrases]
    inputs += [f'{phrase[:-1]}' for phrase in phrases]
    inputs += [f'{phrases[index]}{phrases[(index * 7 + 3) % len(phrases)]}' for index in range(len(phrases))]
    pool = phrases + phrase_values + characters[::5] + character_values[::7] + _SEPARATORS + _EXTRA
    for index in range(1500):
        pieces = 2 + (index * 37) % 11
        inputs.append(''.join(pool[(index * 7919 + step * 104729 + step * step * 31) % len(pool)] for step in range(pieces)))
    # Long runs: a phrase match splits a segment and later lengths are capped by it.
    for index in range(30):
        inputs.append(
            ''.join(
                phrases[(index * 13 + step * 17) % len(phrases)] + characters[(index * 101 + step * 59) % len(characters)]
                for step in range(40)
            )
        )
    seen: set[str] = set()
    unique = []
    for text in inputs:
        if text not in seen:
            seen.add(text)
            unique.append(text)
    return unique


def _t2s() -> list[dict[str, Any]]:
    return [
        {'input': text, 'convert': _CONVERTER.convert(text), 'simplified': runtime_config.to_simplified_chinese(text)}
        for text in _t2s_inputs()
    ]


# ---------- pages ----------

_CURRENT_PLAYLIST_HTML = """
<div class="desktop video-playlist-wrapper">
  <h4><span>清單</span><a href="/playlist?list=1">○○交配</a></h4>
  <div class="video-item-container">
    <div class="thumb-container"><a href="/watch?v=407017"><span>Preview 407017</span></a></div>
    <div class="video-info-container"></div>
  </div>
</div>
<div class="video-playlist-wrapper mobile">
  <h4><span>清單</span><a href="/playlist?list=1">○○交配</a></h4>
  <div class="playlist-video-card video-item-container">
    <div class="thumb-container"><a href="/watch?v=407017"></a></div>
    <div class="video-info-container"><h4 class="video-title">援助交配 11</h4></div>
  </div>
  <div class="playlist-video-card video-item-container">
    <div class="thumb-container"><a href="/watch?v=37176"></a></div>
    <div class="video-info-container"><h4 class="video-title">援助交配 1</h4></div>
  </div>
</div>
"""

_LINK_TITLE_PLAYLIST_HTML = """
<div class="video-playlist-wrapper">
  <h4><span>清單</span><a href="/playlist?list=2">ヌきヌき ずっぽしイズム</a></h4>
  <div class="video-item-container">
    <a href="/watch?v=407018"><span>射精深陷主義 1</span></a>
  </div>
</div>
"""

_LEGACY_PLAYLIST_HTML = """
<div id="video-playlist-wrapper">
  <h4>屈辱</h4>
  <div class="related-watch-wrap multiple-link-wrapper">
    <a class="overlay" href="https://hanime1.me/watch?v=13253"></a>
    <div class="card-mobile-title">屈辱 1</div>
  </div>
  <div class="related-watch-wrap multiple-link-wrapper">
    <a class="overlay" href="/watch?v=12488"></a>
    <img src="/thumbnail/12488.jpg" alt="屈辱 2" />
  </div>
  <a style="text-decoration: none;" href="https://hanime1.me/search?query=%E5%B1%88%E8%BE%B1">
    <div class="load-more-related-link related-watch-wrap">More studio videos</div>
  </a>
</div>
<div id="video-playlist-wrapper">
  <h4>屈辱</h4>
  <a class="overlay" href="https://hanime1.me/watch?v=12488"></a>
  <a class="overlay" href="https://hanime1.me/watch?v=12496"></a>
</div>
"""

_WATCH_PAGE_HTML = """
<html>
  <head>
    <title>クール de M ～崩れないオンナ～ [中文字幕]&nbsp;-&nbsp;H動漫/裏番/線上看&nbsp;-&nbsp;Hanime1.me</title>
    <meta property="og:title" content="Demo Title - Hanime1.me" />
    <meta property="og:image" content="//cdn.example.com/covers/demo.jpg" />
  </head>
  <body>
    <nav><a href="/search?genre=裏番">裏番</a><a href="/search?genre=MMD">MMD</a></nav>
    <h3 id="shareBtn-title" class="video-details-wrapper">クール de M ～崩れないオンナ～ [中文字幕]</h3>
    <div>觀看次數:152.2萬次&nbsp;&nbsp;2026-03-27</div>
    <a id="video-artist-name" href="/search?query=nur">nur</a>
    <a href="https://hanime1.me/search?genre=%E8%A3%8F%E7%95%AA&amp;x=1">裏番</a>
    <div>高冷的抖M</div>
    <div class="video-caption-text caption-ellipsis">劇情介紹。<br>第二行</div>
  </body>
</html>
"""

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

_CF_BLOCKED = '<html><title>Attention Required! | Cloudflare</title><div id="cf-error-details"></div></html>'

STREAM_PAGES = [
    r"""
    <script>
      const v = "https:\/\/video.example.com\/master.m3u8?token=abc&quality=1080";
      const backup = "//cdn.example.com/path/video.mp4?x=1";
    </script>
    """,
    '<source src="HTTPS://CDN.EXAMPLE.COM/A.MP4"><source src="https://cdn.example.com/a.mp4"><source src="https://cdn.example.com/b.M3U8?x=1">',
    '<a href="https://cdn.example.com/a.mp4?x=1&amp;y=2">x</a> <a href="//cdn.example.com/c.m3u8#frag">y</a>',
    "<video src='//cdn.example.com/q.mp4'></video> https://cdn.example.com/plain.mp4 http://x.example/y.m3u8?",
    'no streams here https://example.com/a.mp3',
    'https://[::1/a.mp4',
    'https://cdn.example.com/a　b.mp4 //cdn.example.com/c\x1cd.mp4 //cdn.example.com/e\x85f.mp4',
    'https://x.example/watch.mp4.m3u8 https://x.example/one.mp4.mp4?z',
    '',
]

TITLE_PAGES = [
    """
    <html>
      <head>
        <meta property="og:title" content="Sample &amp; Title" />
        <title>Fallback Title</title>
      </head>
    </html>
    """,
    '<meta content="Reversed &lt;b&gt;" property="og:title"><title>T</title>',
    "<META PROPERTY='og:title' CONTENT='  '><TITLE>  Only Title  </TITLE>",
    '<title>\n  Multi\n  line  </title>',
    '<meta property="og:title" content="">',
    '<title></title>',
    '<html>nothing</html>',
    '<meta property="og:title" content="下載 Demo - Hanime1.me">',
]

DOWNLOAD_PAGES = [
    """
    <a data-url="https://vdownload.example.com/18580-sc-480p.mp4?token=a&amp;expires=1">low</a>
    <a data-url="https://vdownload.example.com/18580-sc-720p.mp4?token=b&amp;expires=1">high</a>
    """,
    """
    <a data-url="https://vdownload.example.com/18580-sc-1080p.mp4?token=a&amp;expires=1">1080p</a>
    <a data-url="https://vdownload.example.com/18580-sc-4k.mp4?token=b&amp;expires=1">4k</a>
    <a data-url="https://vdownload.example.com/18580-sc-8K.mp4?token=c&amp;expires=1">8k</a>
    """,
    """
    <a data-url='https://v.example.com/a-3k.mp4'>3k</a>
    <a DATA-URL="https://v.example.com/a-720p.mp4">dup</a>
    <a data-url="https://V.EXAMPLE.COM/A-720P.MP4">dup</a>
    <a data-url="https://v.example.com/a-12k.mp4?x">12k</a>
    <a data-url="https://v.example.com/plain.mp4">plain</a>
    <a data-url="ftp://v.example.com/no.mp4">no</a>
    <a data-url="https:\\/\\/v.example.com\\/esc-480p.mp4">esc</a>
    """,
    '<a data-url="https://v.example.com/a-0p.mp4">x</a><a data-url="https://v.example.com/b-99999p.mp4">y</a>',
    '',
    _CF_BLOCKED,
]

SEARCH_PAGES = [
    """
    <a href="/watch?v=18580">a</a>
    <a href="watch?v=18580">dup</a>
    <a href="//hanime1.me/watch?v=20000&amp;src=1">b</a>
    <a href="https://hanime1.me/watch?v=30000">c</a>
    <a href="/download?v=18580">ignore</a>
    """,
    '<a HREF=\'/watch?v=ABC\'>a</a><a href=\'/watch?v=abc\'>dup</a><a href="/watch?v=">empty</a><a href="/watch?x=1&v=2">x</a>',
    '<a href="https://hanime1.me/search/watch?v=5">x</a><a href="https://other.example/watch?v=6&v=7">y</a>',
    '<a href="/watch?v=%E4%B8%AD+1">enc</a><a href="/watch?v=a/b:c">slash</a><a href="/watch?v=  9  ">spaces</a>',
    '<a href="https://[::1/watch?v=1">bad</a>',
    '<a href="watch?v=1 2">space</a>',
    '',
]

PLAYLIST_PAGES = [
    _CURRENT_PLAYLIST_HTML,
    _LINK_TITLE_PLAYLIST_HTML,
    _LEGACY_PLAYLIST_HTML,
    """
    <div id="video-playlist-wrapper">
      <h4>混合系列</h4>
      <div class="related-watch-wrap multiple-link-wrapper">
        <a class="overlay" href="https://hanime1.me/watch?v=10001"></a>
        <div class="card-mobile-title">混合系列 1</div>
      </div>
      <a class="overlay" href="https://hanime1.me/watch?v=10002"></a>
    </div>
    """,
    """
    <div class="video-playlist-wrapper mobile">
      <h4><span>清單</span><a href="/playlist?list=1">屈辱</a></h4>
      <div class="related-watch-wrap"><a href="/watch?v=13253"></a><div class="card-mobile-title">屈辱 1</div></div>
      <div class="related-watch-wrap"><a href="/watch?v=12488"></a><div class="card-mobile-title">屈辱 2</div></div>
    </div>
    <div class="desktop video-playlist-wrapper">
      <h4><span>清单</span><a href="/playlist?list=1">屈辱</a></h4>
      <a href="/watch?v=13253"></a>
      <a href="/watch?v=12488"></a>
    </div>
    """,
    """
    <div class="video-playlist-wrapper">
      <h4><a href="/playlist?list=1">Series</a></h4>
      <div class="playlist-video-card">
        <a href="/watch?v=12488"></a>
        <div class="video-info-container"><h4 class="renamed-title">Episode 1</h4></div>
      </div>
    </div>
    """,
    """
    <div class="video-playlist-wrapper">
      <h4><a href="/playlist?list=1">Series</a></h4>
      <div class="video-item-container">
        <a href="/watch?v=12488"></a>
        <div class="video-info-container"><h4 class="renamed-title">Episode 1</h4></div>
      </div>
    </div>
    """,
    '<div class="not-video-playlist-wrapper-copy"><h4>Series</h4><a href="/watch?v=12488"></a></div>',
    '<div class="video-playlist-wrapper"><h4>Series</h4></div>',
    '<div class="video-playlist-wrapper"><h4>Series &amp; More</h4></div>',
    """
    <div class="video-playlist-wrapper">
      <h4><span>清單</span><a href="/playlist?list=1">屈辱</a></h4>
      <a href="/watch?v=13253"></a>
      <a href="/watch?v=12488"></a>
    </div>
    """,
    """
    <div id="video-playlist-wrapper">
      <h4>Studio Name</h4>
      <a style="text-decoration: none;" href="https://hanime1.me/search?query=studio">
        <div class="load-more-related-link related-watch-wrap">More studio videos</div>
      </a>
    </div>
    """,
    '<div>no playlist</div>',
    # Upper case, single quotes, attribute order, nesting, entities, numbering.
    """
    <DIV data-x='1' CLASS='Video-Playlist-Wrapper extra' id="p">
      <H4 class='x'>  清單   <b>後宮</b> &amp; 學園 - Hanime1.me </H4>
      <div class='playlist-video-card'><div><div><a href='/watch?v=0042'></a></div></div>
        <h4 class="video-title  big">第一話 &lt;前編&gt;</h4></div>
      <div class="playlist-video-card"><a href="/watch?v=00043&amp;list=1"></a><h4 class="video-title">第二話</h4></div>
      <div class="playlist-video-card"><a href="/watch?v=0"></a><h4 class="video-title">零</h4></div>
    </DIV>
    """,
    # An unclosed wrapper is not a block; the second one is.
    '<div class="video-playlist-wrapper"><h4>Open</h4><a href="/watch?v=1"></a>',
    """
    <div class="video-playlist-wrapper"><h4>Outer</h4>
      <div class="video-playlist-wrapper"><h4>Inner</h4><a href="/watch?v=7"></a></div>
      <a href="/watch?v=8"></a>
    </div>
    """,
    # Escaped JSON-ish links, ids with leading zeros, Unicode digits, empty title.
    r"""
    <div class="video-playlist-wrapper">
      <h4>  </h4><h4>Second</h4>
      <a href="https:\/\/hanime1.me\/watch?v=777&x=1"></a>
      <a href="/watch?v=٣٤">arabic</a>
      <a href="/watch?v=12a">bad</a>
    </div>
    <div class="video-playlist-wrapper"><h4>Real Title - H動漫/裏番/線上看 - Hanime1.me</h4><a href="/watch?v=900"></a></div>
    """,
    # Cards whose titles come from mobile titles and image alts, link fallbacks on two ids.
    """
    <div class="video-playlist-wrapper">
      <h4><a href="/playlist?list=5">ＡＢＣ　系列</a></h4>
      <div class="video-item-container"><a href="/watch?v=501"><span>A</span></a><a href="/watch?v=502">B</a></div>
      <div class="video-item-container"><a href="/watch?v=503"><span>  </span></a><a href="/watch?v=503"><b>Third 第三</b></a></div>
      <div class="related-watch-wrap"><a href="/watch?v=504"></a><img alt='Img 圖' src="x"></div>
      <div class="related-watch-wrap"><a href="/watch?v=505"></a><div class="card-mobile-title"> Mobile 手機 </div></div>
    </div>
    """,
    """
    <div class="video-playlist-wrapper">
      <h4><a href="/playlist?list=1">Dup</a></h4>
      <div class="playlist-video-card"><a href="/watch?v=1"></a><a href="/watch?v=2"></a><h4 class="video-title">Both</h4></div>
      <div class="playlist-video-card"><a href="/watch?v=2"></a><h4 class="video-title">Second wins?</h4></div>
    </div>
    """,
    '<div class="video-playlist-wrapper"><h4><a href="/playlist?list=1"></a></h4><a href="/watch?v=5"></a></div>',
    '<div class="video-playlist-wrapper"><h4>清單</h4><a href="/watch?v=5"></a></div>',
]

WATCH_PAGES = [
    _WATCH_PAGE_HTML,
    """
    <html>
      <head>
        <meta property="og:title" content="Demo Title - Hanime1.me" />
        <meta property="og:image" content="//cdn.example.com/covers/demo.jpg" />
      </head>
      <body>
        <div>觀看次數:301.5萬次&nbsp;&nbsp;2013-06-28</div>
        <a id="video-artist-name" href="/search?query=測試工坊">測試工坊</a>
        <div>公開測試 2</div>
        <div class="video-caption-text caption-ellipsis">
          這是學生們的課程介紹。
        </div>
      </body>
    </html>
    """,
    """
    <html>
      <body>
        <div>觀看次數:43.4萬次&nbsp;&nbsp;2026-08-12</div>
        <a id="video-artist-name" href="https://hanime1.me/search?query=Maplestar">Maplestar</a>
        <a href="https://hanime1.me/search?genre=2D動畫">2D動畫</a>
        <div>SAKAMOTO DAYS FULL ANIMATION</div>
        <div class="video-caption-text">Title: SAKAMOTO DAYS FULL ANIMATION</div>
      </body>
    </html>
    """,
    """
    <meta name="title" content="Meta &amp; Name - Hanime1.me">
    <meta content="https://img.example/x.jpg" name="twitter:image">
    <div>观看次数 2020-1-1 then 2021-02-03</div>
    """,
    """
    <meta content="Reversed - H動漫/裏番/線上看" name="title">
    <meta property="og:image" content="data:image/png;base64,AAAA">
    <meta property="og:image" content="/relative/cover.jpg">
    <a id='video-artist-name' href='#'>  <b>Bold</b>
      Artist  </a>
    """,
    """
    <div id="shareBtn-title">  <span>Span</span>  Title  </div>
    <h2 id="shareBtn-title">Mismatched</h3>
    <div class="video-caption-text">   </div>
    <title>Page Title - Hanime1.me</title>
    """,
    """
    <H1 ID="shareBtn-title">Upper</H1>
    <div>觀看次數:1萬次</div><div>2019-12-31</div><div>Plot context title</div>
    <div class="x video-caption-text y">plot</div>
    """,
    """
    <div>2019-12-31</div><div>观看次数:5</div>
    <div class="video-caption-text">plot only</div>
    <title></title>
    """,
    """
    <a id="video-artist-name" href="/search?query=a">A</a>
    <a href='/search?genre=%E8%A3%8F%E7%95%AA'>x</a>
    <div class="video-caption-text">caption</div>
    """,
    """
    <a id="video-artist-name">Bad &amp; Genre</a><a href="/search?query=1&genre=&#x88CF;&#x756A;#top">y</a>
    """,
    """
    <a id="video-artist-name">G</a><a href="/search?genre=%20%20">blank</a>
    """,
    '<a id="video-artist-name" href="/search?query=Maplestar">Maplestar</a><span>'
    + 'x' * 1200
    + '</span><a href="/search?genre=2D動畫">2D動畫</a>',
    '<a id="video-artist-name" href="/search?query=Maplestar">Maplestar</a><span>'
    + '中' * 960
    + '</span><a href="/search?genre=近">近</a>',
    '<nav><a href="/search?genre=裏番">裏番</a><a href="/search?genre=MMD">MMD</a></nav>',
    '<div>' + '觀' * 1300 + '</div><div>far 2001-01-01</div>' + 'y' * 1300 + '<div class="video-caption-text">p</div>',
    '',
]

AUTHOR_PAGES = [
    _AUTHOR_PAGE_HTML,
    '<html><head><title>Maplestar - 影片 - H動漫/裏番/線上看 - Hanime1.me</title></head><body></body></html>',
    '<html><head><title>搜尋 - Hanime1.me</title></head></html>',
    '',
    '<h1 class="profile-display-name">動畫社團</h1>',
    """
    <a data-x="1" class="card video-link extra" href="https://hanime1.me/watch?v=100">
      <div class="thumb-container"></div>
    </a>
    <a href="/watch?v=100" class="video-link"><div class="title">dup</div></a>
    <a href="/watch?v=abc" class="video-link"><div class="title">bad id</div></a>
    <a href="/playlist?list=1" class="video-link"><div class="title">not a watch link</div></a>
    """,
    '<a href="/watch?v=407657">no video-link class</a>',
    """
    <H1 Class='x profile-display-name'>  <span>Name</span> &amp; Co </H1>
    <A CLASS='video-link' HREF='/watch?v=0'><div class='title'>zero</div></A>
    <a class="video-links" href="/watch?v=5"><div class="title">plural class</div></a>
    <a class="video-link" href="/watch?v=0005&amp;x=1"><div class="title t2">Leading &lt;zeros&gt;</div></a>
    <a class="video-link" href="/watch?v=٣"><div class="title">arabic</div></a>
    <a class="video-link" href="/watch?v=6"><div class="subtitle">sub</div><div class="title">real</div></a>
    """,
    '<h1 class="profile-display-name"> </h1><title>Fallback - 影片 - x</title>',
    '<title>  Spaced   Name  -  影片 - </title>',
]

AUTHOR_INPUTS = [
    '202534',
    ' 202534 ',
    'https://hanime1.me/user/202534/uploaded',
    'https://hanime1.me/user/202534/uploaded?page=2',
    'https://hanime1.me/user/202534',
    '/user/202534/uploaded',
    '0',
    '-5',
    'https://hanime1.me/watch?v=202534',
    'garbage',
    '',
    '007',
    '٣٤',
    'https://hanime1.me/user/0/uploaded',
    'https://hanime1.me/user/abc/user/12',
    'https://hanime1.me/user//12',
    'https://[::1/user/1',
    'user/5',
    '1' * 4301,
]

SEEDS = [
    '12488',
    '屈辱 {id-12488}',
    '  屈辱   {ID-012488}  ',
    '{id-12488}',
    '{id-0}',
    '0',
    '000',
    '٣٤',
    '  ',
    '',
    '屈辱',
    '後宮 {id-5} {id-6}',
    'Title{id-7}',
    '戀愛 {id-8}x',
    '{id-}',
    '　標題　{id-9}',
    '1' * 4301,
]

STREAM_URLS = [
    'https://video.example.com/18580-sc-720p.mp4',
    'https://video.example.com/18580-sc-4k.mp4?token=1',
    'https://video.example.com/18580-sc-8K.mp4',
    'https://video.example.com/18580-sc-2k.mp4',
    'https://video.example.com/18580-sc-3k.mp4',
    'https://video.example.com/18580-sc-12k.mp4&x',
    'https://video.example.com/18580-sc-123k.mp4',
    'https://video.example.com/master.m3u8',
    'https://video.example.com/1080p/master.m3u8',
    'https://video.example.com/a-12345p.mp4',
    'https://video.example.com/a-1080P.MP4',
    'https://video.example.com/a-1080p.mp4x',
    'https://video.example.com/a-٧٢٠p.mp4',
    '',
]

SIZES = [
    None,
    -1,
    0,
    1,
    999,
    1023,
    1024,
    1280,
    1331,
    1587,
    1024 * 1024,
    1024**3 * 5 + 1,
    1024**4 * 2,
    1024**5 * 3,
    2**63 - 1,
    10239,
    1048575,
]

RUNTIME_IDS: list[Any] = [
    12488,
    0,
    -1,
    True,
    False,
    None,
    1.5,
    '12488',
    ' 12488 ',
    '0012',
    '0',
    '-3',
    'abc',
    '١٢',
    '²',
    'https://hanime1.me/watch?v=12488',
    'https://hanime1.me/watch?v=abc',
    '',
]

TITLES = [
    'OVA Demo [新番預告]',
    'OVA Demo [新番预告]',
    'OVA Demo [中字後補]',
    'OVA Demo [中文後補]',
    'OVA Demo [中文后补]',
    'OVA Demo 新番预告',
    'OVA Demo 预告',
    None,
    '',
    '  ',
]

ITEMS = [
    {'id': 'video-123', 'title': '', 'page_url': 'https://hanime1.me/v/demo', 'stream_url': None},
    {'id': '  ', 'title': '', 'page_url': 'https://hanime1.me/a/b/slug-x/', 'stream_url': None},
    {'id': '', 'title': '', 'page_url': None, 'stream_url': 'https://cdn.example/x/y.mp4'},
    {'id': '', 'title': '', 'page_url': 'https://hanime1.me/', 'stream_url': None},
    {'id': '', 'title': '', 'page_url': None, 'stream_url': None},
    {'id': '12488', 'title': '', 'page_url': 'https://hanime1.me/watch?v=12488', 'stream_url': None},
    {'id': '12488', 'title': '', 'page_url': 'https://hanime1.me/download?v=12488', 'stream_url': None},
    {'id': '12488', 'title': '', 'page_url': 'https://other.example/v?v=999', 'stream_url': None},
    {'id': 'a/b:c', 'title': '', 'page_url': None, 'stream_url': None},
]

PAGE_URLS = [
    '//cdn.example/x',
    '/watch?v=1',
    'watch?v=1',
    '//',
    'https://a.example/',
    'http://b.example',
    ' &amp;x ',
    'HTTPS://upper.example',
]

UPLOADER_PREFIXES = [
    ('[Maplestar] SAKAMOTO DAYS', 'Maplestar'),
    ('[maplestar] SAKAMOTO DAYS', 'Maplestar'),
    ('[Other] SAKAMOTO DAYS', 'Maplestar'),
    ('SAKAMOTO DAYS', None),
    ('[Maplestar]', 'Maplestar'),
    ('[ Map.le*star ]  Title', ' Map.le*star '),
    ('[MAPLESTAR]Title', 'maplestar'),
    ('[Maplestar] [Maplestar] Twice', 'Maplestar'),
    ('Title', ''),
]


def _record(item: dict[str, Any]) -> HanimeRecord:
    return HanimeRecord(**item)


def _videos(titles: tuple[str | None, ...]) -> tuple[WatchSeriesVideo, ...]:
    return tuple(WatchSeriesVideo(video_id=str(position), title=title, position=position) for position, title in enumerate(titles, 1))


def _parsing() -> list[dict[str, Any]]:  # noqa: C901, PLR0912 - one block per function under test
    cases: list[dict[str, Any]] = []

    def add(fn: str, args: list[Any], call: Any) -> None:
        cases.append({'fn': fn, 'args': args, 'result': _attempt(call)})

    for page in STREAM_PAGES + TITLE_PAGES:
        add('extract_stream_urls', [page], lambda page=page: Hanime1.extract_stream_urls(page))
    for page in TITLE_PAGES + WATCH_PAGES + AUTHOR_PAGES[:3]:
        add('parse_title', [page], lambda page=page: Hanime1.parse_title(page))
    for title in [
        None,
        '',
        '  ',
        'Demo - Hanime1.me',
        '下載 Demo',
        '下载 下載 Demo',
        '  下載  Demo - Hanime1.me ',
        ' - Hanime1.me',
        'X - hanime1.me',
    ]:
        add('_normalize_download_title', [title], lambda title=title: Hanime1._normalize_download_title(title))  # noqa: SLF001
        add('_normalize_watch_title', [title], lambda title=title: Hanime1._normalize_watch_title(title))  # noqa: SLF001
    for title in [
        'a <b>b</b>  c - H動漫/裏番/線上看 - Hanime1.me',
        '&lt;b&gt;x&lt;/b&gt; - HANIME1.ME',
        'T -H動漫/裏番/線上看',
        'x\x1cy\x85z',
    ]:
        add('_normalize_watch_title', [title], lambda title=title: Hanime1._normalize_watch_title(title))  # noqa: SLF001
        add('normalize_hanime1_watch_title', [title], lambda title=title: runtime_config.normalize_hanime1_watch_title(title))
    for url in STREAM_URLS:
        add('_stream_quality', [url], lambda url=url: Hanime1._stream_quality(url))  # noqa: SLF001
        add('_stream_resolution_label', [url], lambda url=url: Hanime1._stream_resolution_label(url))  # noqa: SLF001
    for size in SIZES:
        add('_format_file_size', [size], lambda size=size: Hanime1._format_file_size(size))  # noqa: SLF001
    for page in DOWNLOAD_PAGES:
        add('extract_download_urls', [page], lambda page=page: Hanime1.extract_download_urls(page))
    for url in [
        None,
        '',
        'https://hanime1.me/watch?v=12488',
        'https://hanime1.me/watch?x=1&v=%20a%2Fb%20&v=2',
        '/watch?v=',
        'https://[::1/watch?v=1',
        'watch?v=' + 'x' * 100,
        'v=1',
    ]:
        add('_extract_video_code', [url], lambda url=url: Hanime1._extract_video_code(url))  # noqa: SLF001
    for item in ITEMS:
        add('_build_download_page_url', [item], lambda item=item: Hanime1._build_download_page_url(_record(item)))  # noqa: SLF001
        add('_build_watch_page_url', [item], lambda item=item: Hanime1._build_watch_page_url(_record(item)))  # noqa: SLF001
        add('_derive_item_id', [item], lambda item=item: Hanime1._derive_item_id(_record(item)))  # noqa: SLF001
    for url in PAGE_URLS:
        add('_normalize_page_url', [url], lambda url=url: Hanime1._normalize_page_url(url))  # noqa: SLF001
    for page in SEARCH_PAGES + PLAYLIST_PAGES[:3]:
        add('extract_search_video_ids', [page], lambda page=page: Hanime1.extract_search_video_ids(page))
    for page in PLAYLIST_PAGES:
        add('extract_watch_series', [page], lambda page=page: Hanime1.extract_watch_series(page))
        for require in (False, True):
            add(
                'extract_hanime1_playlist_entries',
                [page, require],
                lambda page=page, require=require: runtime_config.extract_hanime1_playlist_entries(page, require_current_titles=require),
            )
            for seed_id in (None, '12488', '407017'):
                add(
                    'parse_hanime1_playlist',
                    [page, seed_id, require],
                    lambda page=page, seed_id=seed_id, require=require: runtime_config.parse_hanime1_playlist(
                        page,
                        seed_id=seed_id,
                        require_current_titles=require,
                    ),
                )
    for titles in [
        ('First', 'Second'),
        ('First', None),
        ('Title A', ' title   a '),
        ('後編', '后编'),
        (),
        ('', 'x'),
        ('A',),
        ('Ａ', 'a'),
        ('ß', 'ss'),
    ]:
        add(
            '_playlist_item_titles_are_distinct',
            [list(titles)],
            lambda titles=titles: Hanime1._playlist_item_titles_are_distinct(_videos(titles)),  # noqa: SLF001
        )
    for series_name, title, position, total, use in [
        ('ラブはギャルから始まる運命', '戀愛始於辣妹的命運 2', 1, 2, True),
        ('Series', None, 1, 1, False),
        ('Series', 'Item', 3, 12, False),
        ('Series', 'Item', 1, 120, False),
        (None, '項目', 1, 2, False),
        (None, None, 2, 2, False),
        ('', '', 1, 1, True),
        ('系列', '', 1, 3, True),
    ]:
        add(
            '_format_playlist_archive_title',
            [series_name, title, position, total, use],
            lambda series_name=series_name, title=title, position=position, total=total, use=use: Hanime1._format_playlist_archive_title(  # noqa: SLF001
                series_name=series_name,
                video=WatchSeriesVideo(video_id='vid', title=title, position=position),
                total=total,
                use_item_title=use,
            ),
        )
    for title in TITLES:
        add('_ignored_title_marker', [title], lambda title=title: Hanime1._ignored_title_marker(title))  # noqa: SLF001
    for page in WATCH_PAGES + PLAYLIST_PAGES[:2]:
        add('extract_watch_metadata', [page], lambda page=page: Hanime1.extract_watch_metadata(page))
        add('_extract_genre_from_watch_html', [page], lambda page=page: Hanime1._extract_genre_from_watch_html(page))  # noqa: SLF001
    for title, uploader in UPLOADER_PREFIXES:
        add(
            '_strip_uploader_prefix',
            [title, uploader],
            lambda title=title, uploader=uploader: Hanime1._strip_uploader_prefix(title, uploader),  # noqa: SLF001
        )
    for genre, group in [('里番', '公開便所'), (None, 'Maplestar'), ('  ', None), ('a/b', ' c:d '), ('2D动画', ''), ('.', '..')]:
        add(
            '_resolve_output_dir',
            [genre, group],
            lambda genre=genre, group=group: Hanime1._resolve_output_dir(item_id='1', genre=genre, group=group),  # noqa: SLF001
        )
    for period, page in [('weekly', 1), ('monthly', 2), ('weekly', 0), ('daily', 1)]:
        add(
            '_build_ranking_page_url',
            [period, page],
            lambda period=period, page=page: Hanime1._build_ranking_page_url(period=period, page=page),  # noqa: SLF001
        )
    for raw in RUNTIME_IDS:
        add('_normalize_runtime_video_id', [raw], lambda raw=raw: Hanime1._normalize_runtime_video_id(raw))  # noqa: SLF001
    for seed_video_id, ids in [('100', ('100', '101')), ('0300', ('200', '300', '١٢', 'x')), ('5', ())]:
        add(
            '_series_member_ids_from_watch_series',
            [seed_video_id, list(ids)],
            lambda seed_video_id=seed_video_id, ids=ids: Hanime1._series_member_ids_from_watch_series(  # noqa: SLF001
                seed_video_id=seed_video_id,
                series=WatchSeries(name='x', video_ids=ids),
            ),
        )
    for ids in [[], ['12', '3'], ['007', '7'], ['١٢', '5'], ['x'], ['99999999999999999999999999', '100000000000000000000000000']]:
        add('select_hanime1_canonical_id', [ids], lambda ids=ids: runtime_config.select_hanime1_canonical_id(ids))
    for seed_id, title in [('12488', '屈辱'), ('1', '戀愛'), ('1', ''), ('1', None), ('1', '  ')]:
        add(
            'build_hanime1_seed',
            [seed_id, title],
            lambda seed_id=seed_id, title=title: runtime_config.build_hanime1_seed(seed_id=seed_id, title=title),
        )
    for seed in [*SEEDS, 12488, None]:
        add('parse_runtime_series_seed', [seed], lambda seed=seed: runtime_config.parse_runtime_series_seed(seed))
    for seed in SEEDS:
        add('normalize_hanime1_seed', [seed], lambda seed=seed: runtime_config.normalize_hanime1_seed(seed, allow_id_only=True))
        add('extract_hanime1_seed_id', [seed], lambda seed=seed: runtime_config.extract_hanime1_seed_id(seed))
    for page in AUTHOR_PAGES:
        add('extract_author_display_name', [page], lambda page=page: hanime1_author.extract_author_display_name(page))
        add('extract_author_video_entries', [page], lambda page=page: hanime1_author.extract_author_video_entries(page))
    for raw in AUTHOR_INPUTS:
        add('extract_hanime1_author_id', [raw], lambda raw=raw: hanime1_author.extract_hanime1_author_id(raw))
    for referer, cookie in [('https://hanime1.me/v/demo', 'cf_clearance=token'), (None, None), ('', '')]:
        add(
            '_build_download_command',
            [referer, cookie],
            lambda referer=referer, cookie=cookie: Hanime1._build_download_command(  # noqa: SLF001
                stream_url='https://video.example.com/master.m3u8',
                output_template=Path('/tmp/cache/videos/12488.%(ext)s'),  # noqa: S108 - only a string in the command
                referer=referer,
                cookie_header=cookie,
            ),
        )
    for message in ['boom', '', 'x' * 600, '中' * 600]:
        add('_format_scan_error', [message], lambda message=message: Hanime1._format_scan_error(RuntimeError(message)))  # noqa: SLF001
    return cases


def _constants() -> list[dict[str, Any]]:
    return [
        {
            'user_agent': hanime1.USER_AGENT,
            'cf_block_markers': list(hanime1._CF_BLOCK_MARKERS),  # noqa: SLF001
            'parser_failure_dedupe_key': hanime1._PARSER_FAILURE_DEDUPE_KEY,  # noqa: SLF001
            'ignored_title_markers': list(hanime1._IGNORED_TITLE_MARKERS),  # noqa: SLF001
            'series_user_agent': hanime1_author.USER_AGENT,
        },
    ]


GOLDENS = {
    'hanime1/t2s_dictionaries.json': _t2s_dictionaries,
    'hanime1/t2s.json': _t2s,
    'hanime1/parsing.json': _parsing,
    'hanime1/constants.json': _constants,
}

"""`src/web/stellasora.py` (WP6): the wiki page parsers, title normalization, the
destination map and the API answer reader, over pages and titles chosen to hit their
branches beyond what tests/test_stellasora.py covers.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from typing import Any

import httpx
from src.web import stellasora
from src.web.stellasora import (
    BASE_URL,
    build_download_destination_map,
    dedupe_wiki_file_titles,
    normalize_wiki_file_title,
    parse_character_infobox_images,
    parse_characters_page,
    parse_gallery_awakened_file_titles,
    parse_gallery_cg_file_titles,
    parse_gallery_images_file_titles,
    parse_gallery_memory_snapshot_file_titles,
    parse_gallery_skin_file_titles,
    parse_list_of_discs_page_file_titles,
)

TITLE_CASES: list[str] = [
    'File:Disc_Good_Night.png',
    'File:Disc Good Night.png',
    'file:disc_good_night.png',
    'FILE:Upper.PNG',
    'Image:Not a file prefix.png',
    'Plain name.jpg',
    '  File:Padded.png  ',
    '',
    '   ',
    '/wiki/File:Disc_Good_Night.png',
    '/wiki/File:Encoded%20Name%E4%B8%AD.png',
    'https://stellasora.miraheze.org/wiki/File:Disc_Good_Night.png',
    'https://stellasora.miraheze.org/w/index.php?title=File:X.png',
    'http://example.com/wiki/File:Other_Host.png#frag',
    'File:With#fragment.png',
    'File:With?query=1.png',
    'File:Dots.in.name.tar.gz',
    'File:No_extension',
    'File:.hidden',
    'File:Trailing.',
    'File:中文 名字.png',
    'File:Bad<chars>:here|?.png',
    'File:' + 'long' * 80 + '.png',
    'File:' + '中' * 90 + '.webp',
    'File:%zz-not-escape.png',
    'File:a/b/c.png',
    'https://[::1/wiki/File:Broken.png',
]

DEDUPE_CASES: list[list[str]] = [
    [
        'File:Disc Good Night.png',
        'file:disc_good_night.png',
        '/wiki/File:Disc_Good_Night.png',
        'https://stellasora.miraheze.org/wiki/File:Disc_Good_Night.png',
        'File:Story_tales_07_001.png',
    ],
    ['', '  ', 'File:A.png', 'File:a.PNG', 'File:ß.png', 'File:SS.png', 'File:ss.png'],
    ['File:İ.png', 'File:i̇.png', 'File:ﬁ.png', 'File:fi.png'],
    [],
]

DESTINATION_CASES: list[dict[str, Any]] = [
    {
        'disc_titles': ['File:Disc_Good_Night.png', 'File:Disc_Sunlit_Blossom.png'],
        'character_titles': {
            'Freesia': ['File:Disc_Good_Night.png', 'File:Story_tales_07_001.png'],
            'Chitose': ['File:Disc_Good_Night.png'],
        },
    },
    {
        'disc_titles': ['', 'file:z.png', 'File:Z.png'],
        'character_titles': {'Bad/Name: ?': ['File:z.png'], '': ['File:empty.png'], '..': ['File:dots.png']},
    },
    {'disc_titles': [], 'character_titles': {}},
]

_HEADING = '<div class="mw-heading"><h2 id="{0}">{0}</h2></div>'
_SUBHEADING = '<div class="mw-heading mw-heading3"><h3 id="{0}">{0}</h3></div>'

PAGE_CASES: list[str] = [
    # Characters table: the name column, nested markup, rows without enough cells.
    """
    <table id="trekker-table">
      <tr><th>Profile</th><th>Name</th><th>x</th></tr>
      <tr><td><a href="/wiki/File:Foo.png">Foo</a></td><td><a href="/wiki/Freesia">Free&amp;sia</a></td></tr>
      <tr><td></td><td><b><a href="Relative_Page">  Spaced   Name  </a></b></td></tr>
      <tr><td></td><td><a href="/wiki/No_Text"></a></td></tr>
      <tr><td></td><td>no link</td></tr>
      <tr><td></td><td><a>no href</a></td></tr>
      <tr><td></td><td><a href="https://other.example/wiki/Abs">Absolute</a><a href="/wiki/Second">Second</a></td></tr>
      <TR><TD></TD><TD><A HREF="/wiki/Upper">Upper</A></TD></TR>
    </table>
    <table id="other"><tr><td></td><td><a href="/wiki/Outside">Outside</a></td></tr></table>
    """,
    # Tables nested inside the characters table, and a second one with the same id.
    """
    <table id="trekker-table"><tr><td></td><td>
      <table><tr><td><a href="/wiki/Inner">Inner</a></td></tr></table>
      <a href="/wiki/After_Inner">After</a></td></tr>
    </table>
    <table id="trekker-table"><tr><td></td><td><a href="/wiki/Again">Again</a></td></tr></table>
    """,
    # Infobox figures.
    """
    <aside class="portable-infobox">
      <figure class="pi-item pi-image"><a href="//static.example/a.png" title="Profile Image">x</a></figure>
      <figure><a href="/w/b.png" title="Artwork"></a></figure>
      <figure><a href="c.png">no title</a><a title="no href">x</a></figure>
      <a href="/outside.png" title="Outside">x</a>
      <figure><a href="/first.png" title="Twice"></a><a href="/second.png" title="Twice"></a></figure>
    </aside>
    """,
    # Gallery sections and sprite subsections, with duplicates, odd hrefs and casing.
    _HEADING.format('Intro')
    + '<a href="/wiki/File:Intro.png">x</a>'
    + _HEADING.format('CGs')
    + '<a href="/wiki/File:Cg_1.png">x</a><a href="/wiki/File:Cg_1.png">dup</a>'
    + '<a href="https://stellasora.miraheze.org/wiki/File:Cg_Abs.png">x</a>'
    + '<a href="/wiki/Not_A_File">x</a><a href="/wiki/file:lower.png">x</a>'
    + '<a href="/wiki/File:Encoded%20Space.png">x</a>'
    + _HEADING.format('Images')
    + '<a href="/wiki/File:Snap.png" title="Memory Snapshot">x</a>'
    + '<a href="/wiki/File:Snap_Old.png" title="memory snapshot (old)">x</a>'
    + '<a href="/wiki/File:Intro_EN.jpg" title="English Introduction">x</a>'
    + '<a href="/wiki/File:No_Title.png">x</a>'
    + _HEADING.format('Sprites')
    + _SUBHEADING.format('Default')
    + '<a href="/wiki/File:Default.png">x</a>'
    + _SUBHEADING.format('Awakened')
    + '<a href="/wiki/File:Awake_1.png">x</a>'
    + _SUBHEADING.format('Skin1')
    + '<a href="/wiki/File:Skin1.png">x</a>'
    + _SUBHEADING.format('skin2')
    + '<a href="/wiki/File:Skin2_lower.png">x</a>'
    + _SUBHEADING.format('Skin_Extra')
    + '<a href="/wiki/File:Skin_Extra.png">x</a>'
    + _SUBHEADING.format('Awakened')
    + '<a href="/wiki/File:Awake_2.png">x</a>'
    + _HEADING.format('Other')
    + _SUBHEADING.format('Awakened')
    + '<a href="/wiki/File:Not_Sprites.png">x</a>',
    # Headings in other shapes: ids on spans, h4, missing ids.
    '<h2><span id="CGs">CGs</span></h2><a href="/wiki/File:Span_Cg.png">x</a>'
    '<h2 id="CGs">CGs</h2><a href="/wiki/File:H2_Cg.png">x</a>'
    '<h4 id="Images">Images</h4><a href="/wiki/File:H4.png" title="Memory Snapshot">x</a>'
    '<h2>No id</h2><a href="/wiki/File:After_No_Id.png">x</a>'
    '<h2 id="Sprites">Sprites</h2><h3>No id</h3><a href="/wiki/File:Sprite_No_Id.png">x</a>'
    '<h3 id="Awakened">Awakened</h3><a href="/wiki/File:Awake_H3.png">x</a>'
    '<h2 id="Images">Images</h2><a href="/wiki/File:Late_Snap.png" title="Memory Snapshot">x</a>',
    # The disc table: only image files, in order, de-duplicated; other tables ignored.
    """
    <table id="disc-table">
      <tr><td><a href="/wiki/File:Disc_A.png">D</a></td><td><a href="/wiki/File:BGM_A.ogg">B</a></td></tr>
      <tr><td><a href="/wiki/File:Disc_B.JPG">D</a></td><td><a href="/wiki/File:Disc_A.png">dup</a></td></tr>
      <tr><td><a href="/wiki/File:Disc_C.webp">D</a></td><td><a href="/wiki/File:Disc_D.gif">G</a></td></tr>
      <tr><td><a href="/wiki/File:Disc%20E.jpeg">E</a></td><td><a href="/wiki/Element">Aqua</a></td></tr>
      <tr><td><table><tr><td><a href="/wiki/File:Nested.png">n</a></td></tr></table></td></tr>
      <tr><td><a href="/wiki/File:After_Nested.png">n</a></td></tr>
    </table>
    <table id="disc-table-2"><tr><td><a href="/wiki/File:Other_Table.png">x</a></td></tr></table>
    """,
    # Broken markup: unclosed tags, comments, scripts, entities in attributes.
    '<table id="trekker-table"><tr><td><td><a href="/wiki/A&amp;B">A &lt;B&gt;</a><!-- <a href="/wiki/C">C</a> -->'
    '<script>"<a href=\'/wiki/File:Script.png\'>"</script>'
    '<h2 id=CGs>CGs</h2><a href=/wiki/File:Unquoted.png>u</a>'
    '<a href="/wiki/File:Unclosed.png"',
    '',
]

IMAGEINFO_CASES: list[Any] = [
    {
        'query': {
            'pages': [
                {
                    'title': 'File:A.png',
                    'imageinfo': [{'url': 'https://static.example/a.png', 'descriptionurl': 'https://wiki/A'}],
                },
                {'title': 'File:B.png', 'imageinfo': [{'url': 'https://static.example/b.png'}]},
                {'title': 'File:Missing.png', 'missing': True},
                {'title': '', 'imageinfo': [{'url': 'x'}]},
                {'title': 'File:NoUrl.png', 'imageinfo': [{'descriptionurl': 'd'}]},
                {'title': 'File:EmptyInfo.png', 'imageinfo': []},
                {'title': 'File:NullInfo.png', 'imageinfo': [None]},
                {'title': 12, 'imageinfo': [{'url': 34, 'descriptionurl': ''}]},
            ],
        },
    },
    {},
    {'query': {}},
    {'query': {'pages': []}},
    {'query': {'pages': None}},
    {'query': {'pages': {'1': {'title': 'File:Dict.png'}}}},
    {'query': {'pages': ['not a dict']}},
    {'query': []},
    [],
    {'query': {'pages': [{'title': 'File:X.png', 'imageinfo': 'text'}]}},
    {'query': {'pages': [{'title': 'File:X.png', 'imageinfo': [{'url': None}]}]}},
]


def _outcome(call: Any) -> dict[str, Any]:
    try:
        return {'result': call()}
    except Exception as exc:  # noqa: BLE001 - the error is the recorded answer
        return {'error': [type(exc).__name__, str(exc)]}


def _titles() -> list[dict[str, Any]]:
    return [
        {
            'title': title,
            'normalized': _outcome(lambda title=title: normalize_wiki_file_title(title)),
            'local_name': _outcome(lambda title=title: stellasora._safe_local_filename_from_wiki_title(title)),  # noqa: SLF001
        }
        for title in TITLE_CASES
    ]


def _dedupe() -> list[dict[str, Any]]:
    return [{'titles': titles, 'result': dedupe_wiki_file_titles(titles)} for titles in DEDUPE_CASES]


def _destinations() -> list[dict[str, Any]]:
    cases = []
    for case in DESTINATION_CASES:
        mapping = build_download_destination_map(
            base_path=Path('/data/stellasora'),
            disc_titles=case['disc_titles'],
            character_titles=case['character_titles'],
        )
        cases.append(
            {
                'disc_titles': case['disc_titles'],
                'character_titles': [[name, titles] for name, titles in case['character_titles'].items()],
                'result': [[title, str(path)] for title, path in mapping.items()],
            },
        )
    return cases


def _page(html: str) -> dict[str, Any]:
    return {
        'html': html,
        'characters': _outcome(lambda: [[c.name, c.url] for c in parse_characters_page(html, base_url=BASE_URL)]),
        'infobox': _outcome(lambda: [[k, v] for k, v in parse_character_infobox_images(html, base_url=BASE_URL).items()]),
        'cgs': parse_gallery_cg_file_titles(html),
        'images': parse_gallery_images_file_titles(html),
        'memory_snapshots': parse_gallery_memory_snapshot_file_titles(html),
        'awakened': parse_gallery_awakened_file_titles(html),
        'skins': parse_gallery_skin_file_titles(html),
        'discs': _outcome(lambda: parse_list_of_discs_page_file_titles(html)),
    }


def _pages() -> list[dict[str, Any]]:
    return [_page(html) for html in PAGE_CASES]


def _resolve(payload: Any) -> list[list[Any]]:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=json.dumps(payload).encode())

    async def run() -> list[list[Any]]:
        source = stellasora.StellaSora.__new__(stellasora.StellaSora)
        source.client = httpx.AsyncClient(transport=httpx.MockTransport(handler), base_url=BASE_URL)
        try:
            files = await source.resolve_files(['File:A.png'])
        finally:
            await source.client.aclose()
        return [[item.title, item.url, item.description_url] for item in files]

    return asyncio.run(run())


def _imageinfo() -> list[dict[str, Any]]:
    return [{'payload': payload, **_outcome(lambda payload=payload: _resolve(payload))} for payload in IMAGEINFO_CASES]


GOLDENS = {
    'stellasora/titles.json': _titles,
    'stellasora/dedupe.json': _dedupe,
    'stellasora/destinations.json': _destinations,
    'stellasora/pages.json': _pages,
    'stellasora/imageinfo.json': _imageinfo,
}

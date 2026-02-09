# ruff: noqa: S101

import asyncio

from src.web.stellasora import (
    BASE_URL,
    StellaSora,
    build_download_destination_map,
    dedupe_wiki_file_titles,
    parse_character_infobox_images,
    parse_characters_page,
    parse_gallery_awakened_file_titles,
    parse_gallery_cg_file_titles,
    parse_gallery_skin_file_titles,
    parse_gallery_memory_snapshot_file_titles,
    parse_list_of_discs_page_file_titles,
)


def test_parse_characters_page_extracts_character_links() -> None:
    html = """
    <html><body>
      <table id="trekker-table">
        <tr><th>Profile</th><th>Name</th></tr>
        <tr>
          <td><a href="/wiki/File:Foo.png">Foo</a></td>
          <td><a href="/wiki/Freesia" title="Freesia">Freesia</a></td>
          <td><a href="/wiki/Post_Haste" title="Post Haste">Post Haste</a></td>
        </tr>
        <tr>
          <td>...</td>
          <td><span><a href="/wiki/Snowish_Laru" title="Snowish Laru">Snowish Laru</a></span></td>
          <td>...</td>
        </tr>
      </table>
    </body></html>
    """

    characters = parse_characters_page(html, base_url=BASE_URL)
    assert [c.name for c in characters] == ['Freesia', 'Snowish Laru']
    assert [c.url for c in characters] == [f'{BASE_URL}/wiki/Freesia', f'{BASE_URL}/wiki/Snowish_Laru']


def test_parse_character_infobox_images_extracts_profile_and_artwork() -> None:
    html = """
    <aside class="portable-infobox pi-theme-default">
      <div class="pi-media-collection">
        <figure class="pi-item pi-media pi-image">
          <a href="//static.wikitide.net/stellasorawiki/2/2f/Freesia.png" class="image image-thumbnail" title="Profile Image">
            <img src="//static.wikitide.net/stellasorawiki/thumb/2/2f/Freesia.png/300px-Freesia.png" />
          </a>
        </figure>
        <figure class="pi-item pi-media pi-image">
          <a href="//static.wikitide.net/stellasorawiki/d/d1/Freesia_Artwork.png" class="image image-thumbnail" title="Artwork">
            <img src="//static.wikitide.net/stellasorawiki/thumb/d/d1/Freesia_Artwork.png/300px-Freesia_Artwork.png" />
          </a>
        </figure>
      </div>
    </aside>
    """

    images = parse_character_infobox_images(html, base_url=BASE_URL)
    assert images['Profile Image'] == 'https://static.wikitide.net/stellasorawiki/2/2f/Freesia.png'
    assert images['Artwork'] == 'https://static.wikitide.net/stellasorawiki/d/d1/Freesia_Artwork.png'


def test_parse_gallery_cg_file_titles_limits_to_cgs_section() -> None:
    html = """
    <div class="mw-heading"><h2 id="Intro">Intro</h2></div>
    <a href="/wiki/File:Intro.png">Intro</a>
    <div class="mw-heading"><h2 id="CGs">CGs</h2></div>
    <ul class="gallery">
      <li><a href="/wiki/File:Disc_Good_Night.png">Disc Good Night</a></li>
      <li><a href="/wiki/File:Story_tales_07_001.png">Story</a></li>
      <li><a href="/wiki/File:Disc_Good_Night.png">duplicate</a></li>
    </ul>
    <div class="mw-heading"><h2 id="Sprites">Sprites</h2></div>
    <a href="/wiki/File:Sprite.png">Sprite</a>
    """

    titles = parse_gallery_cg_file_titles(html)
    assert titles == ['File:Disc_Good_Night.png', 'File:Story_tales_07_001.png']


def test_parse_gallery_memory_snapshot_file_titles_limits_to_images_section() -> None:
    html = """
    <div class="mw-heading"><h2 id="Images">Images</h2></div>
    <ul class="gallery">
      <li><a href="/wiki/File:Foo_Intro_EN.jpg" title="Foo's English Introduction">Intro</a></li>
      <li><a href="/wiki/File:Foo_Memory_Snapshot.png" title="Memory Snapshot">Memory Snapshot</a></li>
      <li><a href="/wiki/File:Foo_Memory_Snapshot_Old.png" title="Memory Snapshot (Old)">Memory Snapshot (Old)</a></li>
    </ul>
    <div class="mw-heading"><h2 id="CGs">CGs</h2></div>
    <a href="/wiki/File:Disc_Good_Night.png">Disc Good Night</a>
    """

    titles = parse_gallery_memory_snapshot_file_titles(html)
    assert titles == ['File:Foo_Memory_Snapshot.png', 'File:Foo_Memory_Snapshot_Old.png']


def test_parse_gallery_awakened_file_titles_limits_to_sprites_awakened_subsection() -> None:
    html = """
    <div class="mw-heading"><h2 id="Sprites">Sprites</h2></div>
    <div class="mw-heading mw-heading3"><h3 id="Default">Default</h3></div>
    <a href="/wiki/File:Foo_a_02.png">Default</a>
    <div class="mw-heading mw-heading3"><h3 id="Awakened">Awakened</h3></div>
    <ul class="gallery">
      <li><a href="/wiki/File:Foo_awakened_02.png">Awakened 1</a></li>
      <li><a href="/wiki/File:Foo_awakened_03.png">Awakened 2</a></li>
    </ul>
    <div class="mw-heading mw-heading3"><h3 id="Variant_B">Variant B</h3></div>
    <a href="/wiki/File:Foo_b_02.png">Variant</a>
    <div class="mw-heading"><h2 id="Other">Other</h2></div>
    <a href="/wiki/File:Other.png">Other</a>
    """

    titles = parse_gallery_awakened_file_titles(html)
    assert titles == ['File:Foo_awakened_02.png', 'File:Foo_awakened_03.png']


def test_parse_gallery_skin_file_titles_limits_to_sprites_skin_subsections() -> None:
    html = """
    <div class="mw-heading"><h2 id="Sprites">Sprites</h2></div>
    <div class="mw-heading mw-heading3"><h3 id="Default">Default</h3></div>
    <a href="/wiki/File:Foo_a_02.png">Default</a>
    <div class="mw-heading mw-heading3"><h3 id="Skin1">Skin1</h3></div>
    <ul class="gallery">
      <li><a href="/wiki/File:Foo_skin1_02.png">Skin1 1</a></li>
      <li><a href="/wiki/File:Foo_skin1_03.png">Skin1 2</a></li>
    </ul>
    <div class="mw-heading mw-heading3"><h3 id="Variant_B">Variant B</h3></div>
    <a href="/wiki/File:Foo_b_02.png">Variant</a>
    <div class="mw-heading mw-heading3"><h3 id="Skin2">Skin2</h3></div>
    <a href="/wiki/File:Foo_skin2_02.png">Skin2</a>
    <div class="mw-heading"><h2 id="Other">Other</h2></div>
    <div class="mw-heading mw-heading3"><h3 id="Skin3">Skin3</h3></div>
    <a href="/wiki/File:Foo_skin3_02.png">Other</a>
    """

    titles = parse_gallery_skin_file_titles(html)
    assert titles == ['File:Foo_skin1_02.png', 'File:Foo_skin1_03.png', 'File:Foo_skin2_02.png']


def test_parse_list_of_discs_page_file_titles_extracts_disc_images() -> None:
    html = """
    <table id="disc-table">
      <tr>
        <td><a href="/wiki/File:Disc_Good_Night.png">Disc</a></td>
        <td><a href="/wiki/File:BGM_Good_Night.ogg">BGM</a></td>
        <td><a href="/wiki/Element">Aqua</a></td>
      </tr>
    </table>
    """

    titles = parse_list_of_discs_page_file_titles(html)
    assert titles == ['File:Disc_Good_Night.png']


def test_dedupe_wiki_file_titles_normalizes_spaces_and_case() -> None:
    titles = [
        'File:Disc Good Night.png',
        'file:disc_good_night.png',
        '/wiki/File:Disc_Good_Night.png',
        'https://stellasora.miraheze.org/wiki/File:Disc_Good_Night.png',
        'File:Story_tales_07_001.png',
    ]

    assert dedupe_wiki_file_titles(titles) == ['File:Disc_Good_Night.png', 'File:Story_tales_07_001.png']


def test_build_download_destination_map_disc_prefers_character_dir(tmp_path) -> None:  # noqa: ANN001
    base = tmp_path / 'stellasora'
    character_titles = {
        'Freesia': ['File:Disc_Good_Night.png', 'File:Story_tales_07_001.png'],
        'Chitose': ['File:Disc_Good_Night.png'],
    }
    disc_titles = ['File:Disc_Good_Night.png', 'File:Disc_Sunlit_Blossom.png']

    title_to_dir = build_download_destination_map(
        base_path=base,
        disc_titles=disc_titles,
        character_titles=character_titles,
    )

    assert title_to_dir['File:Disc_Good_Night.png'] == base / 'Freesia'
    assert title_to_dir['File:Disc_Sunlit_Blossom.png'] == base / 'disc'


def test_download_targets_skips_when_path_missing(tmp_path, monkeypatch) -> None:  # noqa: ANN001
    ss = StellaSora()
    ss.path = tmp_path / 'missing'
    assert not ss.path.exists()

    async def _boom() -> dict[str, object]:
        raise AssertionError('build_target_destination_map should not be called when path is missing')

    monkeypatch.setattr(ss, 'build_target_destination_map', _boom)

    async def _run() -> None:
        await ss.download_targets()
        await ss.client.aclose()

    asyncio.run(_run())
    assert not ss.path.exists()

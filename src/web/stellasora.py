"""Stella Sora Wiki scraper.

This module is intentionally interface-first:
- Parse the Characters page to get the canonical character list.
- Provide helpers to extract character artwork and gallery assets (CGs, Memory Snapshot, Awakened sprites).
- Provide a MediaWiki API helper to resolve file titles to direct image URLs.

Download and dedupe can be built on top of these interfaces.
"""

from __future__ import annotations

import asyncio
import shutil
from dataclasses import dataclass
from html import unescape
from html.parser import HTMLParser
from pathlib import Path
from typing import Any
from urllib.parse import unquote, urljoin, urlsplit

import httpx
from tqdm import tqdm

from src.core import config, logger
from src.tool import sanitize
from src.tool.notifications import enqueue_notification

log = logger.get('stellasora')
cfg = config.web.stellasora

BASE_URL = 'https://stellasora.miraheze.org'
API_PATH = '/w/api.php'
CHARACTERS_PATH = '/wiki/Characters'
LIST_OF_DISCS_PATH = '/wiki/List_of_Discs'
_CHARACTERS_NAME_COL_INDEX = 2


@dataclass(frozen=True, slots=True)
class CharacterPage:
    """A wiki character page entry."""

    name: str
    url: str


@dataclass(frozen=True, slots=True)
class WikiFile:
    """A resolved MediaWiki file entry."""

    title: str
    url: str
    description_url: str | None = None


@dataclass(slots=True)
class DownloadStats:
    skipped: int = 0
    moved: int = 0
    downloaded: int = 0


def normalize_wiki_file_title(title: str) -> str:
    """Normalize a MediaWiki File title to a stable form for dedupe.

    Returns a string like: "File:Disc_Good_Night.png".
    """
    t = title.strip()
    if not t:
        return ''

    # Remove URL fragments/query.
    t = t.split('#', 1)[0].split('?', 1)[0]

    # Accept full URLs or /wiki/ links.
    if t.startswith(('http://', 'https://')):
        path = urlsplit(t).path
        if path.startswith('/wiki/'):
            t = path
    if t.startswith('/wiki/'):
        t = t.removeprefix('/wiki/')

    t = unquote(t).replace(' ', '_')
    if t.lower().startswith('file:'):
        return f'File:{t[5:]}'
    return f'File:{t}'


def dedupe_wiki_file_titles(titles: list[str]) -> list[str]:
    """Dedupe file titles by normalized name (stable order, first occurrence wins)."""
    seen: set[str] = set()
    out: list[str] = []
    for raw in titles:
        normalized = normalize_wiki_file_title(raw)
        if not normalized:
            continue
        key = normalized.casefold()
        if key in seen:
            continue
        out.append(normalized)
        seen.add(key)
    return out


def build_download_destination_map(
    *,
    base_path: Path,
    disc_titles: list[str],
    character_titles: dict[str, list[str]],
) -> dict[str, Path]:
    """Build the final download destination dir for each wiki file title.

    Rules:
    - Character-owned files go to `base_path/<character name>/`.
    - Disc files go to `base_path/disc/`.
    - If a disc file also appears in any character list, it is treated as character-owned
      and will be placed under the first character encountered in `character_titles` order.
    """
    title_to_dir: dict[str, Path] = {}

    for character_name, titles in character_titles.items():
        character_dir = base_path / sanitize(character_name, max_bytes=120)
        for raw in titles:
            normalized = normalize_wiki_file_title(raw)
            if not normalized:
                continue
            title_to_dir.setdefault(normalized, character_dir)

    disc_dir = base_path / 'disc'
    for raw in disc_titles:
        normalized = normalize_wiki_file_title(raw)
        if not normalized:
            continue
        title_to_dir.setdefault(normalized, disc_dir)

    return title_to_dir


def _safe_local_filename_from_wiki_title(title: str) -> str:
    normalized = normalize_wiki_file_title(title)
    name = normalized.removeprefix('File:')
    p = Path(name)
    suffix = p.suffix
    stem = p.stem if suffix else name

    # Keep the extension intact, trim stem by bytes.
    max_stem_bytes = 200 - len(suffix.encode('utf-8'))
    safe_stem = sanitize(stem, max_bytes=max_stem_bytes)
    return f'{safe_stem}{suffix}'


class _CharactersTableParser(HTMLParser):
    def __init__(self, base_url: str) -> None:
        super().__init__(convert_charrefs=True)
        self._base_url = base_url
        self._in_table = False
        self._in_row = False
        self._td_index = 0

        self._capture_anchor = False
        self._in_anchor = False
        self._current_href: str | None = None
        self._text_parts: list[str] = []

        self.characters: list[CharacterPage] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attrs_d = dict(attrs)
        if tag == 'table' and attrs_d.get('id') == 'trekker-table':
            self._in_table = True
            return

        if not self._in_table:
            return

        if tag == 'tr':
            self._in_row = True
            self._td_index = 0
            return

        if tag == 'td' and self._in_row:
            self._td_index += 1
            self._capture_anchor = False
            self._in_anchor = False
            self._current_href = None
            self._text_parts = []
            return

        if tag == 'a' and self._in_row and self._td_index == _CHARACTERS_NAME_COL_INDEX:
            href = attrs_d.get('href')
            if href and href.startswith('/wiki/'):
                self._capture_anchor = True
                self._in_anchor = True
                self._current_href = href

    def handle_endtag(self, tag: str) -> None:
        if tag == 'table' and self._in_table:
            self._in_table = False
            return

        if not self._in_table:
            return

        if tag == 'tr':
            self._in_row = False
            return

        if tag == 'a' and self._in_anchor:
            if self._capture_anchor and self._current_href:
                name = unescape(''.join(self._text_parts)).strip()
                if name:
                    self.characters.append(
                        CharacterPage(
                            name=name,
                            url=urljoin(self._base_url, self._current_href),
                        ),
                    )
            self._capture_anchor = False
            self._in_anchor = False
            self._current_href = None
            self._text_parts = []

    def handle_data(self, data: str) -> None:
        if self._in_anchor and self._capture_anchor:
            self._text_parts.append(data)


class _InfoboxImagesParser(HTMLParser):
    def __init__(self, base_url: str) -> None:
        super().__init__(convert_charrefs=True)
        self._base_url = base_url
        self._infobox_depth = 0
        self.images: dict[str, str] = {}

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attrs_d = dict(attrs)
        if tag == 'aside':
            classes = (attrs_d.get('class') or '').split()
            if 'portable-infobox' in classes:
                self._infobox_depth += 1
            return

        if self._infobox_depth <= 0:
            return

        if tag == 'a':
            href = attrs_d.get('href')
            title = attrs_d.get('title')
            cls = attrs_d.get('class') or ''
            if not href or not title:
                return
            if 'image-thumbnail' not in cls:
                return
            # The wiki uses protocol-relative URLs for static images.
            self.images[title] = urljoin(self._base_url, href)

    def handle_endtag(self, tag: str) -> None:
        if tag == 'aside' and self._infobox_depth > 0:
            self._infobox_depth -= 1


class _GallerySectionFilesParser(HTMLParser):
    def __init__(self, section_id: str) -> None:
        super().__init__(convert_charrefs=True)
        self._section_id = section_id
        self._in_cgs = False
        self._started = False
        self._titles: list[str] = []

    @property
    def titles(self) -> list[str]:
        seen: set[str] = set()
        out: list[str] = []
        for t in self._titles:
            if t in seen:
                continue
            out.append(t)
            seen.add(t)
        return out

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attrs_d = dict(attrs)

        if tag == 'h2':
            h_id = attrs_d.get('id') or ''
            if not self._started and h_id == self._section_id:
                self._in_cgs = True
                self._started = True
                return
            if self._started and self._in_cgs and h_id and h_id != self._section_id:
                self._in_cgs = False
                return

        if not self._in_cgs:
            return

        if tag != 'a':
            return

        href = attrs_d.get('href')
        if not href:
            return

        if not href.startswith('/wiki/File:'):
            return

        # href is like: /wiki/File:Disc_Good_Night.png
        title = unquote(href.removeprefix('/wiki/'))
        self._titles.append(title)


def parse_characters_page(html: str, *, base_url: str = BASE_URL) -> list[CharacterPage]:
    """Parse /wiki/Characters HTML and return the character page list."""
    parser = _CharactersTableParser(base_url)
    parser.feed(html)
    return parser.characters


def parse_character_infobox_images(html: str, *, base_url: str = BASE_URL) -> dict[str, str]:
    """Extract direct image URLs from the character infobox.

    The infobox typically contains at least:
    - "Profile Image"
    - "Artwork"
    """
    parser = _InfoboxImagesParser(base_url)
    parser.feed(html)
    return parser.images


def parse_gallery_cg_file_titles(html: str) -> list[str]:
    """Extract MediaWiki File titles in the 'CGs' section of a character gallery page."""
    parser = _GallerySectionFilesParser('CGs')
    parser.feed(html)
    return parser.titles


def parse_gallery_images_file_titles(html: str) -> list[str]:
    """Extract MediaWiki File titles in the 'Images' section of a character gallery page."""
    parser = _GallerySectionFilesParser('Images')
    parser.feed(html)
    return parser.titles


def parse_gallery_memory_snapshot_file_titles(html: str) -> list[str]:
    """Extract MediaWiki File titles for 'Memory Snapshot' images from the 'Images' section."""
    titles = parse_gallery_images_file_titles(html)
    return [t for t in titles if 'memory_snapshot' in t.lower().replace(' ', '_')]


class _SpritesSubsectionFilesParser(HTMLParser):
    def __init__(self, subsection_id: str) -> None:
        super().__init__(convert_charrefs=True)
        self._subsection_id = subsection_id
        self._in_sprites = False
        self._sprites_started = False
        self._in_subsection = False
        self._subsection_started = False
        self._titles: list[str] = []

    @property
    def titles(self) -> list[str]:
        seen: set[str] = set()
        out: list[str] = []
        for t in self._titles:
            if t in seen:
                continue
            out.append(t)
            seen.add(t)
        return out

    def _update_sprites_state(self, h_id: str) -> None:
        if not self._sprites_started:
            if h_id == 'Sprites':
                self._in_sprites = True
                self._sprites_started = True
            return

        if self._in_sprites and h_id and h_id != 'Sprites':
            self._in_sprites = False
            self._in_subsection = False

    def _update_subsection_state(self, h_id: str) -> None:
        if not self._subsection_started:
            if h_id == self._subsection_id:
                self._in_subsection = True
                self._subsection_started = True
            return

        if self._in_subsection and h_id and h_id != self._subsection_id:
            self._in_subsection = False

    def _maybe_add_file(self, href: str | None) -> None:
        if not href or not href.startswith('/wiki/File:'):
            return
        self._titles.append(unquote(href.removeprefix('/wiki/')))

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attrs_d = dict(attrs)

        if tag == 'h2':
            self._update_sprites_state(attrs_d.get('id') or '')
        if not self._in_sprites:
            return

        if tag == 'h3':
            self._update_subsection_state(attrs_d.get('id') or '')
        if not self._in_subsection:
            return

        if tag == 'a':
            self._maybe_add_file(attrs_d.get('href'))


def parse_gallery_awakened_file_titles(html: str) -> list[str]:
    """Extract MediaWiki File titles in the 'Sprites' -> 'Awakened' subsection of a character gallery page."""
    parser = _SpritesSubsectionFilesParser('Awakened')
    parser.feed(html)
    return parser.titles


class _SpritesSubsectionPrefixFilesParser(HTMLParser):
    def __init__(self, subsection_prefix: str) -> None:
        super().__init__(convert_charrefs=True)
        self._prefix = subsection_prefix.casefold()
        self._in_sprites = False
        self._sprites_started = False
        self._in_subsection = False
        self._titles: list[str] = []

    @property
    def titles(self) -> list[str]:
        seen: set[str] = set()
        out: list[str] = []
        for t in self._titles:
            if t in seen:
                continue
            out.append(t)
            seen.add(t)
        return out

    def _update_sprites_state(self, h_id: str) -> None:
        if not self._sprites_started:
            if h_id == 'Sprites':
                self._in_sprites = True
                self._sprites_started = True
            return

        if self._in_sprites and h_id and h_id != 'Sprites':
            self._in_sprites = False
            self._in_subsection = False

    def _update_subsection_state(self, h_id: str) -> None:
        if not h_id:
            self._in_subsection = False
            return
        self._in_subsection = h_id.casefold().startswith(self._prefix)

    def _maybe_add_file(self, href: str | None) -> None:
        if not href or not href.startswith('/wiki/File:'):
            return
        self._titles.append(unquote(href.removeprefix('/wiki/')))

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attrs_d = dict(attrs)

        if tag == 'h2':
            self._update_sprites_state(attrs_d.get('id') or '')
        if not self._in_sprites:
            return

        if tag == 'h3':
            self._update_subsection_state(attrs_d.get('id') or '')
        if not self._in_subsection:
            return

        if tag == 'a':
            self._maybe_add_file(attrs_d.get('href'))


def parse_gallery_skin_file_titles(html: str) -> list[str]:
    """Extract MediaWiki File titles in the 'Sprites' -> 'Skin*' subsections of a character gallery page."""
    parser = _SpritesSubsectionPrefixFilesParser('Skin')
    parser.feed(html)
    return parser.titles


class _TableFilesParser(HTMLParser):
    def __init__(self, table_id: str) -> None:
        super().__init__(convert_charrefs=True)
        self._table_id = table_id
        self._in_table = False
        self._titles: list[str] = []

    @property
    def titles(self) -> list[str]:
        # Preserve order, keep duplicates (caller can dedupe later).
        return self._titles

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attrs_d = dict(attrs)

        if tag == 'table' and attrs_d.get('id') == self._table_id:
            self._in_table = True
            return

        if not self._in_table:
            return

        if tag != 'a':
            return

        href = attrs_d.get('href')
        if not href or not href.startswith('/wiki/File:'):
            return

        self._titles.append(unquote(href.removeprefix('/wiki/')))

    def handle_endtag(self, tag: str) -> None:
        if tag == 'table' and self._in_table:
            self._in_table = False


def parse_list_of_discs_page_file_titles(html: str) -> list[str]:
    """Extract disc image file titles from /wiki/List_of_Discs."""
    parser = _TableFilesParser('disc-table')
    parser.feed(html)
    return [t for t in parser.titles if normalize_wiki_file_title(t).casefold().startswith('file:disc_')]


def _chunked(items: list[str], size: int) -> list[list[str]]:
    if size <= 0:
        msg = 'size must be positive'
        raise ValueError(msg)
    return [items[i : i + size] for i in range(0, len(items), size)]


class StellaSora:
    """Scraper for the Stella Sora Wiki."""

    def __init__(self, *, base_url: str = BASE_URL) -> None:
        self.cfg = cfg
        self.path = self.cfg.path
        self.base_url = base_url.rstrip('/')
        self.client = httpx.AsyncClient(
            base_url=self.base_url,
            timeout=60,
            follow_redirects=True,
            limits=httpx.Limits(max_keepalive_connections=10, max_connections=10),
            proxy=config.proxy or None,
            headers={
                'User-Agent': 'fav/0.1 (stellasora)',
            },
        )

    async def aclose(self) -> None:
        await self.client.aclose()

    def _notify_relpath(self, saved_path: Path) -> str:
        base_path = getattr(self, 'path', None)
        if not isinstance(base_path, Path):
            return saved_path.as_posix()
        try:
            relpath = saved_path.resolve().relative_to(base_path.resolve())
        except ValueError:
            relpath = saved_path
        return relpath.as_posix()

    async def _notify_download(self, *, title: str, image_url: str, saved_path: Path, link_url: str = '') -> None:
        relpath = self._notify_relpath(saved_path)

        try:
            await enqueue_notification(
                kind='download_completed',
                source='stellasora',
                title=f'StellaSora: {title.removeprefix("File:")}',
                body=relpath,
                link_url=link_url,
                image_url=image_url,
                payload={
                    'saved_path': str(saved_path),
                    'image_path': str(saved_path),
                },
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue stellasora download notification for %s: %s', title, exc)

    def _disc_dir(self) -> Path:
        return self.path / 'disc'

    def _handle_existing_file(self, *, title: str, dst_dir: Path, existing_index: dict[str, list[Path]]) -> tuple[bool, int, int]:
        """Return (needs_download, moved, skipped)."""
        local_name = _safe_local_filename_from_wiki_title(title)
        dst_path = dst_dir / local_name

        if dst_path.exists():
            return False, 0, 1

        existing_paths = existing_index.get(local_name.casefold(), [])
        if not existing_paths:
            return True, 0, 0

        # Prefer moving from disc/ to character/ if needed (single-copy policy).
        disc_dir = self._disc_dir()
        if dst_dir != disc_dir:
            disc_candidates = [p for p in existing_paths if p.parent == disc_dir]
            if disc_candidates and not dst_path.exists():
                dst_dir.mkdir(parents=True, exist_ok=True)
                shutil.move(str(disc_candidates[0]), dst_path)
                existing_index.setdefault(local_name.casefold(), []).append(dst_path)
                return False, 1, 0

        return False, 0, 1

    async def _get_html(self, url: str) -> str:
        res = await self.client.get(url)
        res.raise_for_status()
        return res.text

    async def list_characters(self) -> list[CharacterPage]:
        """List character pages from the Characters index page."""
        html = await self._get_html(CHARACTERS_PATH)
        characters = parse_characters_page(html, base_url=self.base_url)
        log.info('Found %d characters', len(characters))
        return characters

    async def get_character_infobox_images(self, character: CharacterPage) -> dict[str, str]:
        """Get direct URLs for images shown in the character infobox (Profile Image/Artwork)."""
        html = await self._get_html(character.url)
        return parse_character_infobox_images(html, base_url=self.base_url)

    async def _get_gallery_html(self, character: CharacterPage) -> str:
        return await self._get_html(f'{character.url}/gallery')

    async def list_disc_image_files(self) -> list[str]:
        """List MediaWiki file titles for disc images from List of Discs."""
        html = await self._get_html(LIST_OF_DISCS_PATH)
        return parse_list_of_discs_page_file_titles(html)

    async def list_character_cg_files(self, character: CharacterPage) -> list[str]:
        """List MediaWiki file titles under the 'CGs' section for a character."""
        html = await self._get_gallery_html(character)
        return parse_gallery_cg_file_titles(html)

    async def list_character_image_files(self, character: CharacterPage) -> list[str]:
        """List MediaWiki file titles under the 'Images' section for a character."""
        html = await self._get_gallery_html(character)
        return parse_gallery_images_file_titles(html)

    async def list_character_memory_snapshot_files(self, character: CharacterPage) -> list[str]:
        """List MediaWiki file titles for 'Memory Snapshot' images for a character."""
        html = await self._get_gallery_html(character)
        return parse_gallery_memory_snapshot_file_titles(html)

    async def list_character_awakened_files(self, character: CharacterPage) -> list[str]:
        """List MediaWiki file titles for 'Sprites' -> 'Awakened' images for a character."""
        html = await self._get_gallery_html(character)
        return parse_gallery_awakened_file_titles(html)

    async def list_character_skin_files(self, character: CharacterPage) -> list[str]:
        """List MediaWiki file titles for 'Sprites' -> 'Skin*' images for a character."""
        html = await self._get_gallery_html(character)
        return parse_gallery_skin_file_titles(html)

    async def list_character_target_files(self, character: CharacterPage) -> list[str]:
        """List target file titles (CGs + Memory Snapshot + Awakened + Skins) for a character, using a single gallery fetch."""
        html = await self._get_gallery_html(character)
        return (
            parse_gallery_cg_file_titles(html)
            + parse_gallery_memory_snapshot_file_titles(html)
            + parse_gallery_awakened_file_titles(html)
            + parse_gallery_skin_file_titles(html)
        )

    async def list_target_file_titles(self) -> list[str]:
        """Build the final file-title list to download (deduped by MediaWiki file name)."""
        disc_files = await self.list_disc_image_files()
        characters = await self.list_characters()

        # Keep concurrency conservative to avoid hammering the wiki.
        sem = asyncio.Semaphore(5)

        async def _worker(ch: CharacterPage) -> list[str]:
            async with sem:
                try:
                    return await self.list_character_target_files(ch)
                except httpx.HTTPError as exc:
                    log.warning('Failed to fetch gallery for %s: %s', ch.name, exc)
                    return []

        per_character = await asyncio.gather(*[_worker(ch) for ch in characters])
        character_files = [t for sub in per_character for t in sub]

        combined = disc_files + character_files
        deduped = dedupe_wiki_file_titles(combined)

        log.info(
            'Files: discs=%d, character(cgs+memory_snapshot+awakened+skin)=%d, combined=%d, unique=%d',
            len(disc_files),
            len(character_files),
            len(combined),
            len(deduped),
        )
        return deduped

    async def _list_character_target_files_map(self) -> dict[str, list[str]]:
        characters = await self.list_characters()

        # Keep concurrency conservative to avoid hammering the wiki.
        sem = asyncio.Semaphore(5)

        async def _worker(ch: CharacterPage) -> list[str]:
            async with sem:
                try:
                    return await self.list_character_target_files(ch)
                except httpx.HTTPError as exc:
                    log.warning('Failed to fetch gallery for %s: %s', ch.name, exc)
                    return []

        per_character = await asyncio.gather(*[_worker(ch) for ch in characters])
        return {ch.name: titles for ch, titles in zip(characters, per_character, strict=True)}

    async def build_target_destination_map(self) -> dict[str, Path]:
        """Build the final deduped file list and decide the destination directory for each."""
        disc_files = await self.list_disc_image_files()
        character_map = await self._list_character_target_files_map()
        return build_download_destination_map(
            base_path=self.path,
            disc_titles=disc_files,
            character_titles=character_map,
        )

    async def resolve_files(self, file_titles: list[str]) -> list[WikiFile]:
        """Resolve MediaWiki file titles into direct file URLs via the API."""
        if not file_titles:
            return []

        results: list[WikiFile] = []
        for batch in _chunked(file_titles, 50):
            params = {
                'action': 'query',
                'format': 'json',
                'formatversion': '2',
                'prop': 'imageinfo',
                'iiprop': 'url',
                'titles': '|'.join(batch),
                'redirects': '1',
            }
            res = await self.client.get(API_PATH, params=params)
            res.raise_for_status()
            payload: dict[str, Any] = res.json()
            pages: list[dict[str, Any]] = payload.get('query', {}).get('pages', [])
            for page in pages:
                title = str(page.get('title') or '')
                if not title:
                    continue
                imageinfo = page.get('imageinfo') or []
                if not imageinfo:
                    log.debug('No imageinfo for %s', title)
                    continue
                info = imageinfo[0] or {}
                url = info.get('url')
                if not url:
                    continue
                description_url = info.get('descriptionurl')
                results.append(
                    WikiFile(
                        title=title,
                        url=str(url),
                        description_url=str(description_url) if description_url else None,
                    ),
                )

        return results

    async def _download_file(self, url: str, dst_path: Path, *, desc: str, max_attempts: int = 3) -> None:
        dst_path.parent.mkdir(parents=True, exist_ok=True)
        tmp_path = dst_path.with_suffix(f'{dst_path.suffix}.part')

        last_exc: Exception | None = None
        for attempt in range(1, max_attempts + 1):
            if tmp_path.exists():
                tmp_path.unlink()
            try:
                with tqdm(total=0, unit='B', unit_scale=True, desc=desc, dynamic_ncols=True) as pbar:
                    async with self.client.stream('GET', url) as res:
                        res.raise_for_status()
                        total = int(res.headers.get('content-length', 0))
                        if total > 0:
                            pbar.total = total
                        with tmp_path.open('wb') as f:
                            async for chunk in res.aiter_bytes():
                                f.write(chunk)
                                pbar.update(len(chunk))
            except (OSError, httpx.HTTPError) as exc:
                last_exc = exc
                log.warning('Download failed (%d/%d) %s: %s', attempt, max_attempts, dst_path.name, exc)
                await asyncio.sleep(min(10, 2**attempt))
            else:
                tmp_path.replace(dst_path)
                return

        msg = f'Failed to download after {max_attempts} attempts: {dst_path.name}'
        raise RuntimeError(msg) from last_exc

    def _index_existing_files(self) -> dict[str, list[Path]]:
        index: dict[str, list[Path]] = {}
        if not self.path.exists():
            return index

        for p in self.path.rglob('*'):
            if not p.is_file():
                continue
            # Ignore partial files from interrupted downloads.
            if p.name.endswith('.part'):
                continue
            index.setdefault(p.name.casefold(), []).append(p)

        dup_count = sum(1 for paths in index.values() if len(paths) > 1)
        if dup_count:
            log.warning('Found %d duplicate filenames under %s (same name in multiple locations)', dup_count, self.path)
        return index

    def _build_download_queue(
        self,
        *,
        title_to_dir: dict[str, Path],
        existing_index: dict[str, list[Path]],
        stats: DownloadStats,
    ) -> list[str]:
        to_download: list[str] = []
        for title, dst_dir in title_to_dir.items():
            needs_download, moved, skipped = self._handle_existing_file(
                title=title,
                dst_dir=dst_dir,
                existing_index=existing_index,
            )
            stats.moved += moved
            stats.skipped += skipped
            if needs_download:
                to_download.append(title)
        return to_download

    async def _download_resolved_files(
        self,
        *,
        resolved_by_key: dict[str, WikiFile],
        title_to_dir: dict[str, Path],
        existing_index: dict[str, list[Path]],
        stats: DownloadStats,
    ) -> None:
        total = len(resolved_by_key)
        for idx, (title, item) in enumerate(resolved_by_key.items(), start=1):
            dst_dir = title_to_dir[title]
            local_name = _safe_local_filename_from_wiki_title(title)
            dst_path = dst_dir / local_name

            needs_download, moved, skipped = self._handle_existing_file(
                title=title,
                dst_dir=dst_dir,
                existing_index=existing_index,
            )
            stats.moved += moved
            stats.skipped += skipped
            if not needs_download:
                continue

            desc = f'[{idx}/{total}] {dst_dir.name}/{local_name}'
            await self._download_file(item.url, dst_path, desc=desc)
            stats.downloaded += 1
            existing_index.setdefault(local_name.casefold(), []).append(dst_path)
            await self._notify_download(
                title=title,
                image_url=item.url,
                saved_path=dst_path,
                link_url=item.description_url or '',
            )

    async def download_targets(self) -> None:
        """Download all target images to disk according to the destination rules."""
        if not self.path.exists():
            log.warning(
                'Stellasora path does not exist: %s (skipping; create the directory to enable downloads)',
                self.path,
            )
            return
        if not self.path.is_dir():
            log.warning('Stellasora path is not a directory: %s (skipping)', self.path)
            return

        title_to_dir = await self.build_target_destination_map()
        existing_index = self._index_existing_files()

        stats = DownloadStats()
        to_download = self._build_download_queue(
            title_to_dir=title_to_dir,
            existing_index=existing_index,
            stats=stats,
        )
        resolved = await self.resolve_files(to_download)

        # Normalize API-returned titles and keep only the ones we can map.
        resolved_by_key: dict[str, WikiFile] = {}
        for item in resolved:
            key = normalize_wiki_file_title(item.title)
            if not key:
                continue
            if key in title_to_dir:
                resolved_by_key[key] = item

        missing = [t for t in to_download if t not in resolved_by_key]
        if missing:
            log.warning('Missing %d files from API resolve', len(missing))

        await self._download_resolved_files(
            resolved_by_key=resolved_by_key,
            title_to_dir=title_to_dir,
            existing_index=existing_index,
            stats=stats,
        )

        total_targets = len(title_to_dir)
        log.info(
            'Download summary: targets=%d resolved=%d to_download=%d downloaded=%d moved=%d skipped=%d',
            total_targets,
            len(resolved_by_key),
            len(to_download),
            stats.downloaded,
            stats.moved,
            stats.skipped,
        )

    async def update(self) -> None:
        """Update job: download Stella Sora images (characters + discs) to disk."""
        await self.download_targets()

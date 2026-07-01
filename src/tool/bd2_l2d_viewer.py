from __future__ import annotations

import asyncio
import json
import re
from collections import defaultdict
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Literal
from urllib.parse import quote, unquote, urljoin, urlsplit

import httpx

VIEWER_PAGE_URL = 'https://jelosus2.github.io/BD2-L2D-Viewer/'
VIEWER_CHARACTER_LIST_URL = 'https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/utils/character_list.ts'
VIEWER_REPO_TREE_URL = 'https://api.github.com/repos/Jelosus2/BD2-L2D-Viewer/git/trees/main?recursive=1'
VIEWER_SPINES_TREE_PREFIX = 'src/assets/spines/'
VIEWER_RAW_SPINES_BASE_URL = 'https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/'

GAMEKEE_BASE_URL = 'https://www.gamekee.com'
GAMEKEE_ALIAS = 'zsca2'
GAMEKEE_TREE_ROOT_PID = 194491
GAMEKEE_CHARACTER_GROUPS = {122323, 122322, 122318}
DEFAULT_USER_AGENT = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36'

ResourceCategory = Literal['character', 'ultimate', 'dating', 'unknown']

_RESOURCE_SUFFIXES = {'.atlas', '.json', '.skel', '.png', '.webp', '.jpg', '.jpeg'}
_SKELETON_SUFFIXES = ('.skel', '.json')
_VIEWER_CORE_SUFFIXES = ('.atlas', '.skel')
_GAMEKEE_URL_FIELDS = ('atlas', 'skel', 'json', 'bg')
_EXPORT_DEFAULT_RE = re.compile(r'\bexport\s+default\b')


@dataclass(frozen=True, slots=True)
class ViewerResource:
    entry_id: str
    category: ResourceCategory
    stem: str
    char_name: str
    costume_name: str
    files: tuple[str, ...] = ()
    missing_core_files: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class GameKeeResource:
    content_id: int
    title: str
    category: ResourceCategory
    stem: str
    model_key: str = ''
    urls: tuple[str, ...] = ()
    source_path: str = ''


@dataclass(frozen=True, slots=True)
class ComparisonResult:
    viewer_resources: tuple[ViewerResource, ...]
    gamekee_resources: tuple[GameKeeResource, ...]
    matched_stems: tuple[str, ...]
    viewer_only: tuple[ViewerResource, ...]
    gamekee_only: tuple[GameKeeResource, ...]

    def summary(self) -> dict[str, int]:
        viewer_stems = {_resource_key(resource.stem) for resource in self.viewer_resources}
        gamekee_stems = {_resource_key(resource.stem) for resource in self.gamekee_resources}
        return {
            'viewer_resource_count': len(self.viewer_resources),
            'viewer_unique_stem_count': len(viewer_stems),
            'gamekee_resource_count': len(self.gamekee_resources),
            'gamekee_unique_stem_count': len(gamekee_stems),
            'matched_unique_stem_count': len(self.matched_stems),
            'viewer_only_resource_count': len(self.viewer_only),
            'gamekee_only_resource_count': len(self.gamekee_only),
            'viewer_missing_core_file_count': sum(len(resource.missing_core_files) for resource in self.viewer_resources),
        }

    def to_dict(self) -> dict[str, Any]:
        return {
            'summary': self.summary(),
            'matched_stems': list(self.matched_stems),
            'viewer_only': [asdict(resource) for resource in self.viewer_only],
            'gamekee_only': [asdict(resource) for resource in self.gamekee_only],
            'viewer_missing_core_files': [asdict(resource) for resource in self.viewer_resources if resource.missing_core_files],
        }


def normalize_resource_url(raw_url: str) -> str:
    stripped = raw_url.strip()
    if not stripped:
        return ''
    if stripped.startswith('//'):
        return f'https:{stripped}'
    return urljoin(GAMEKEE_BASE_URL, stripped)


def viewer_asset_url(path: str) -> str:
    cleaned = path.strip().strip('/')
    if not cleaned:
        return ''
    quoted_path = '/'.join(quote(part, safe='') for part in cleaned.split('/') if part)
    return f'{VIEWER_RAW_SPINES_BASE_URL}{quoted_path}'


def resource_stem_from_url(url: str) -> str:
    path = unquote(urlsplit(url).path)
    name = Path(path).name
    suffix = Path(name).suffix
    if not name or suffix.casefold() not in _RESOURCE_SUFFIXES:
        return ''
    stem = Path(name).stem
    while Path(stem).suffix.casefold() in _SKELETON_SUFFIXES:
        stem = Path(stem).stem
    return stem


def category_from_stem(stem: str) -> ResourceCategory:
    normalized = stem.casefold()
    if normalized.startswith('cutscene_'):
        return 'ultimate'
    if normalized.startswith('illust_dating'):
        return 'dating'
    if normalized:
        return 'character'
    return 'unknown'


def parse_viewer_character_list(source: str) -> dict[str, dict[str, Any]]:
    match = _EXPORT_DEFAULT_RE.search(source)
    search_start = match.end() if match else 0
    object_source = _extract_balanced_object(source, search_start)
    parsed = json.loads(_strip_trailing_commas(object_source))
    if not isinstance(parsed, dict):
        msg = 'BD2 L2D Viewer character list did not contain an object'
        raise TypeError(msg)

    characters: dict[str, dict[str, Any]] = {}
    for entry_id, value in parsed.items():
        if isinstance(value, dict):
            characters[str(entry_id)] = value
    return characters


def viewer_asset_paths_from_tree_payload(payload: dict[str, Any]) -> frozenset[str]:
    tree = payload.get('tree')
    if not isinstance(tree, list):
        msg = 'GitHub tree payload did not contain a tree list'
        raise TypeError(msg)

    paths: set[str] = set()
    for item in tree:
        if not isinstance(item, dict) or item.get('type') != 'blob':
            continue
        path = item.get('path')
        if isinstance(path, str) and path.startswith(VIEWER_SPINES_TREE_PREFIX):
            paths.add(path.removeprefix(VIEWER_SPINES_TREE_PREFIX))
    return frozenset(paths)


def viewer_resources_from_character_list(
    characters: dict[str, dict[str, Any]],
    *,
    asset_paths: frozenset[str] | None = None,
) -> tuple[ViewerResource, ...]:
    resources: list[ViewerResource] = []
    for entry_id, character in characters.items():
        char_name = _string_value(character.get('charName'))
        costume_name = _string_value(character.get('costumeName'))
        for category, field_name in (('character', 'spine'), ('ultimate', 'cutscene'), ('dating', 'dating')):
            stem = _string_value(character.get(field_name))
            if not stem:
                continue
            files = _viewer_resource_files(entry_id=entry_id, category=category, stem=stem, asset_paths=asset_paths)
            missing = _viewer_missing_core_files(entry_id=entry_id, category=category, stem=stem, asset_paths=asset_paths)
            resources.append(
                ViewerResource(
                    entry_id=entry_id,
                    category=category,
                    stem=stem,
                    char_name=char_name,
                    costume_name=costume_name,
                    files=files,
                    missing_core_files=missing,
                ),
            )
    return tuple(sorted(resources, key=_viewer_sort_key))


def load_gamekee_resources_from_manifests(root: Path) -> tuple[tuple[GameKeeResource, ...], int]:
    resources: list[GameKeeResource] = []
    manifest_count = 0
    for manifest_path in sorted(root.rglob('manifest.json')):
        try:
            manifest = json.loads(manifest_path.read_text(encoding='utf-8'))
        except (OSError, json.JSONDecodeError):
            continue
        if not isinstance(manifest, dict):
            continue
        manifest_count += 1
        resources.extend(gamekee_resources_from_manifest(manifest, source_path=manifest_path.as_posix()))
    return tuple(sorted(resources, key=_gamekee_sort_key)), manifest_count


def gamekee_resources_from_manifest(manifest: dict[str, Any], *, source_path: str = '') -> tuple[GameKeeResource, ...]:
    content_id = _to_int(manifest.get('content_id')) or 0
    title = _string_value(manifest.get('title'))
    live2d_models = manifest.get('live2d_models')
    if not isinstance(live2d_models, list):
        return ()

    resources: list[GameKeeResource] = []
    for model in live2d_models:
        if not isinstance(model, dict):
            continue
        urls = _urls_from_model(model.get('urls'))
        stem = _resource_stem_from_urls(urls)
        if not stem:
            continue
        resources.append(
            GameKeeResource(
                content_id=content_id,
                title=title,
                category=category_from_stem(stem),
                stem=stem,
                model_key=_string_value(model.get('live2d_key')),
                urls=tuple(urls),
                source_path=source_path,
            ),
        )
    return tuple(resources)


def gamekee_resources_from_detail_payload(
    payload: dict[str, Any],
    *,
    tree_row: dict[str, Any] | None = None,
) -> tuple[GameKeeResource, ...]:
    detail = payload.get('data')
    if not isinstance(detail, dict):
        msg = 'GameKee detail payload did not contain data'
        raise TypeError(msg)

    content_id = _to_int(detail.get('content_id')) or _to_int(detail.get('id')) or _to_int((tree_row or {}).get('content_id')) or 0
    title = _string_value(detail.get('title') or detail.get('name') or (tree_row or {}).get('name'))
    content_json = _coerce_content_json(detail.get('content_json'))

    resources: list[GameKeeResource] = []
    for value in _iter_live2d_values(content_json):
        urls = _urls_from_live2d_value(value)
        stem = _resource_stem_from_urls(urls)
        if not stem:
            continue
        resources.append(
            GameKeeResource(
                content_id=content_id,
                title=title,
                category=category_from_stem(stem),
                stem=stem,
                model_key=_string_value(value.get('live2dKey')),
                urls=tuple(urls),
            ),
        )
    return tuple(sorted(_dedupe_gamekee_resources(resources), key=_gamekee_sort_key))


async def fetch_gamekee_resources(
    *,
    content_ids: set[int] | None = None,
    limit: int | None = None,
    concurrency: int = 2,
    request_timeout: float = 60.0,
) -> tuple[tuple[GameKeeResource, ...], int]:
    headers = gamekee_headers()
    async with httpx.AsyncClient(base_url=GAMEKEE_BASE_URL, headers=headers, follow_redirects=True, timeout=request_timeout) as client:
        rows = await fetch_gamekee_character_rows(client)
        selected_rows = _select_gamekee_rows(rows=rows, content_ids=content_ids, limit=limit)
        semaphore = asyncio.Semaphore(max(1, concurrency))

        async def fetch_row(row: dict[str, Any]) -> tuple[GameKeeResource, ...]:
            content_id = _to_int(row.get('content_id')) or 0
            async with semaphore:
                response = await client.get(f'/v1/content/detail/{content_id}', headers=gamekee_headers(content_id))
                response.raise_for_status()
                payload = response.json()
                if not isinstance(payload, dict):
                    msg = f'GameKee returned a non-object detail payload for content_id={content_id}'
                    raise TypeError(msg)
                return gamekee_resources_from_detail_payload(payload, tree_row=row)

        groups = await asyncio.gather(*(fetch_row(row) for row in selected_rows))
    resources = [resource for group in groups for resource in group]
    return tuple(sorted(_dedupe_gamekee_resources(resources), key=_gamekee_sort_key)), len(selected_rows)


async def fetch_gamekee_character_rows(client: httpx.AsyncClient) -> tuple[dict[str, Any], ...]:
    response = await client.get(f'/v1/entry/treesByPid?pid={GAMEKEE_TREE_ROOT_PID}', headers=gamekee_headers())
    response.raise_for_status()
    payload = response.json()
    if not isinstance(payload, dict):
        msg = 'GameKee returned a non-object tree payload'
        raise TypeError(msg)
    return tuple(_filter_gamekee_character_rows(payload.get('data')))


def gamekee_headers(content_id: int | None = None) -> dict[str, str]:
    referer = f'{GAMEKEE_BASE_URL}/{GAMEKEE_ALIAS}/'
    if content_id:
        referer = f'{GAMEKEE_BASE_URL}/{GAMEKEE_ALIAS}/tj/{content_id}.html'
    return {
        'Accept': 'application/json, text/plain, */*',
        'Game-Alias': GAMEKEE_ALIAS,
        'Lang': 'zh-cn',
        'Referer': referer,
        'User-Agent': DEFAULT_USER_AGENT,
        'X-Requested-With': 'XMLHttpRequest',
    }


def fetch_viewer_resources(
    *,
    character_list_url: str = VIEWER_CHARACTER_LIST_URL,
    tree_url: str = VIEWER_REPO_TREE_URL,
    timeout: float = 60.0,
) -> tuple[ViewerResource, ...]:
    with httpx.Client(follow_redirects=True, timeout=timeout) as client:
        character_response = client.get(character_list_url)
        character_response.raise_for_status()
        tree_response = client.get(tree_url)
        tree_response.raise_for_status()

    characters = parse_viewer_character_list(character_response.text)
    tree_payload = tree_response.json()
    if not isinstance(tree_payload, dict):
        msg = 'GitHub tree response did not contain an object'
        raise TypeError(msg)
    asset_paths = viewer_asset_paths_from_tree_payload(tree_payload)
    return viewer_resources_from_character_list(characters, asset_paths=asset_paths)


def compare_resources(
    *,
    gamekee_resources: tuple[GameKeeResource, ...],
    viewer_resources: tuple[ViewerResource, ...],
) -> ComparisonResult:
    gamekee_by_stem = _group_by_stem(gamekee_resources)
    viewer_by_stem = _group_by_stem(viewer_resources)
    matched_stems = tuple(sorted(set(gamekee_by_stem) & set(viewer_by_stem)))
    viewer_only = tuple(resource for resource in viewer_resources if _resource_key(resource.stem) not in gamekee_by_stem)
    gamekee_only = tuple(resource for resource in gamekee_resources if _resource_key(resource.stem) not in viewer_by_stem)
    return ComparisonResult(
        viewer_resources=viewer_resources,
        gamekee_resources=gamekee_resources,
        matched_stems=matched_stems,
        viewer_only=viewer_only,
        gamekee_only=gamekee_only,
    )


def _extract_balanced_object(source: str, search_start: int) -> str:  # noqa: C901
    object_start = source.find('{', search_start)
    if object_start < 0:
        msg = 'Could not find object start in BD2 L2D Viewer character list'
        raise ValueError(msg)

    depth = 0
    quote = ''
    escaped = False
    for index, char in enumerate(source[object_start:], start=object_start):
        if quote:
            if escaped:
                escaped = False
            elif char == '\\':
                escaped = True
            elif char == quote:
                quote = ''
            continue
        if char in {'"', "'"}:
            quote = char
        elif char == '{':
            depth += 1
        elif char == '}':
            depth -= 1
            if depth == 0:
                return source[object_start : index + 1]

    msg = 'Could not find object end in BD2 L2D Viewer character list'
    raise ValueError(msg)


def _strip_trailing_commas(source: str) -> str:
    output: list[str] = []
    quote = ''
    escaped = False
    index = 0
    while index < len(source):
        char = source[index]
        if quote:
            output.append(char)
            if escaped:
                escaped = False
            elif char == '\\':
                escaped = True
            elif char == quote:
                quote = ''
            index += 1
            continue
        if char in {'"', "'"}:
            quote = char
            output.append(char)
            index += 1
            continue
        if char == ',':
            next_index = index + 1
            while next_index < len(source) and source[next_index].isspace():
                next_index += 1
            if next_index < len(source) and source[next_index] in {'}', ']'}:
                index += 1
                continue
        output.append(char)
        index += 1
    return ''.join(output)


def _viewer_resource_dirs(entry_id: str, category: ResourceCategory) -> tuple[str, ...]:
    if category == 'ultimate':
        return (f'{entry_id}/cutscene',)
    if category == 'dating':
        return (f'{entry_id}/dating', f'{entry_id}/dating_nobg')
    return (entry_id,)


def _viewer_resource_files(
    *,
    entry_id: str,
    category: ResourceCategory,
    stem: str,
    asset_paths: frozenset[str] | None,
) -> tuple[str, ...]:
    if asset_paths is None:
        directory = _viewer_resource_dirs(entry_id, category)[0]
        return tuple(f'{directory}/{stem}{suffix}' for suffix in _VIEWER_CORE_SUFFIXES)

    files = [path for path in asset_paths if _viewer_path_matches_resource(path=path, entry_id=entry_id, category=category, stem=stem)]
    return tuple(sorted(files))


def _viewer_missing_core_files(
    *,
    entry_id: str,
    category: ResourceCategory,
    stem: str,
    asset_paths: frozenset[str] | None,
) -> tuple[str, ...]:
    if asset_paths is None:
        return ()

    existing_files = _viewer_resource_files(entry_id=entry_id, category=category, stem=stem, asset_paths=asset_paths)
    existing_dirs = {_dirname(path) for path in existing_files}
    if not existing_dirs:
        existing_dirs = {_viewer_resource_dirs(entry_id, category)[0]}

    missing: list[str] = []
    for directory in sorted(existing_dirs):
        if f'{directory}/{stem}.atlas' not in asset_paths:
            missing.append(f'{directory}/{stem}.atlas')
        if not any(f'{directory}/{stem}{suffix}' in asset_paths for suffix in _SKELETON_SUFFIXES):
            missing.append(f'{directory}/{stem}.skel')
    return tuple(missing)


def _viewer_path_matches_resource(*, path: str, entry_id: str, category: ResourceCategory, stem: str) -> bool:
    directory = _dirname(path)
    if directory not in _viewer_resource_dirs(entry_id, category):
        return False

    name = Path(path).name
    suffix = Path(name).suffix.casefold()
    if suffix not in _RESOURCE_SUFFIXES:
        return False

    path_stem = Path(name).stem
    return path_stem == stem or path_stem.startswith(f'{stem}_')


def _dirname(path: str) -> str:
    if '/' not in path:
        return ''
    return path.rsplit('/', 1)[0]


def _urls_from_model(raw_urls: Any) -> list[str]:
    if not isinstance(raw_urls, dict):
        return []

    urls: list[str] = []
    for field_name in (*_GAMEKEE_URL_FIELDS, 'image', 'atlas_textures'):
        value = raw_urls.get(field_name)
        urls.extend(_coerce_url_list(value))
    return _dedupe_strings(urls)


def _urls_from_live2d_value(value: dict[str, Any]) -> list[str]:
    urls: list[str] = []
    for field_name in (*_GAMEKEE_URL_FIELDS, 'image'):
        urls.extend(_coerce_url_list(value.get(field_name)))
    return _dedupe_strings(urls)


def _coerce_url_list(value: Any) -> list[str]:
    if isinstance(value, list):
        raw_values = [item for item in value if isinstance(item, str)]
    elif isinstance(value, str):
        raw_values = value.split(',')
    else:
        raw_values = []
    return [normalized for raw in raw_values if (normalized := normalize_resource_url(raw))]


def _resource_stem_from_urls(urls: list[str]) -> str:
    for suffix in ('.atlas', '.skel', '.json', '.png', '.webp'):
        for url in urls:
            stem = resource_stem_from_url(url)
            if stem and urlsplit(url).path.casefold().endswith(suffix):
                return stem
    return ''


def _coerce_content_json(value: Any) -> dict[str, Any]:
    if isinstance(value, dict):
        return value
    if not isinstance(value, str) or not value.strip():
        return {}
    parsed = json.loads(value)
    return parsed if isinstance(parsed, dict) else {}


def _iter_live2d_values(node: Any) -> tuple[dict[str, Any], ...]:
    values: list[dict[str, Any]] = []

    def walk(current: Any) -> None:
        if isinstance(current, dict):
            value = current.get('value')
            if current.get('type') == 'live2d' and isinstance(value, dict):
                values.append(value)
            for child in current.values():
                walk(child)
        elif isinstance(current, list):
            for child in current:
                walk(child)

    walk(node)
    return tuple(values)


def _filter_gamekee_character_rows(rows: Any) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    seen: set[int] = set()

    def walk(items: Any, *, in_character_group: bool = False) -> None:
        if not isinstance(items, list):
            return
        for item in items:
            if not isinstance(item, dict):
                continue
            item_id = _to_int(item.get('id')) or 0
            child_in_character_group = in_character_group or item_id in GAMEKEE_CHARACTER_GROUPS
            content_id = _to_int(item.get('content_id'))
            if child_in_character_group and content_id and content_id not in seen:
                seen.add(content_id)
                row = dict(item)
                row.pop('child', None)
                out.append(row)
            walk(item.get('child'), in_character_group=child_in_character_group)

    walk(rows)
    return out


def _select_gamekee_rows(
    *,
    rows: tuple[dict[str, Any], ...],
    content_ids: set[int] | None,
    limit: int | None,
) -> tuple[dict[str, Any], ...]:
    if content_ids:
        selected = [row for row in rows if (_to_int(row.get('content_id')) or 0) in content_ids]
        known_ids = {_to_int(row.get('content_id')) or 0 for row in selected}
        selected.extend({'content_id': content_id, 'name': str(content_id)} for content_id in sorted(content_ids - known_ids))
    else:
        selected = list(rows)
    if limit is not None:
        selected = selected[: max(0, limit)]
    return tuple(selected)


def _dedupe_gamekee_resources(resources: list[GameKeeResource]) -> list[GameKeeResource]:
    deduped: dict[tuple[int, str, ResourceCategory], GameKeeResource] = {}
    for resource in resources:
        deduped[(resource.content_id, _resource_key(resource.stem), resource.category)] = resource
    return list(deduped.values())


def _dedupe_strings(values: list[str]) -> list[str]:
    out: list[str] = []
    seen: set[str] = set()
    for value in values:
        if value in seen:
            continue
        seen.add(value)
        out.append(value)
    return out


def _group_by_stem(resources: tuple[GameKeeResource, ...] | tuple[ViewerResource, ...]) -> dict[str, list[Any]]:
    grouped: dict[str, list[Any]] = defaultdict(list)
    for resource in resources:
        grouped[_resource_key(resource.stem)].append(resource)
    return grouped


def _resource_key(stem: str) -> str:
    return stem.casefold()


def _to_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return None


def _string_value(value: Any) -> str:
    return value.strip() if isinstance(value, str) else ''


def _viewer_sort_key(resource: ViewerResource) -> tuple[str, str, str]:
    return (resource.entry_id, resource.category, resource.stem)


def _gamekee_sort_key(resource: GameKeeResource) -> tuple[int, str, str]:
    return (resource.content_id, resource.category, resource.stem)

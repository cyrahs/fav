from __future__ import annotations

import json
import re
import unicodedata
from dataclasses import asdict, dataclass, field, replace
from datetime import UTC, datetime
from hashlib import blake2b
from typing import Any, Literal
from urllib.parse import urlsplit

import httpx

L2D_SU_CATALOG_URL = 'https://l2d.su/json/live2dMaster.json'
NAGAMI_MAPPING_BUNDLE_URL = 'https://azurlane.nagami.moe/_app/immutable/chunks/l2d_mapping.oLieetCb.js'
NAGAMI_LIVE2D_BASE_URL = 'https://cdn.nagami.moe/live2d'
AZUR_LANE_GAME_ID = 1
DEFAULT_USER_AGENT = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36'
HTTP_ERROR_MIN = 400
CATALOG_ENTRY_ID_PREFIX = 'azurlane'
CATALOG_VARIANT_TOKEN_SIZE = 6

SourceErrorKind = Literal['network', 'parse', 'schema']
L2DSuModelKind = Literal['live2d', 'spine']
CatalogSource = Literal['l2d.su', 'nagami', 'merged']

_MODEL_TYPES: tuple[L2DSuModelKind, ...] = ('live2d', 'spine')
_CATALOG_SOURCES: tuple[CatalogSource, ...] = ('l2d.su', 'nagami', 'merged')
_NAGAMI_COSTUME_SUFFIX_RE = re.compile(r'_\d+$')


class SourceParseError(ValueError):
    pass


class SourceSchemaError(TypeError):
    pass


@dataclass(frozen=True, slots=True)
class SourceFetchMetadata:
    url: str
    http_status: int | None
    etag: str
    last_modified: str
    fetched_at: str

    @classmethod
    def for_url(
        cls,
        url: str,
        *,
        http_status: int | None = None,
        etag: str = '',
        last_modified: str = '',
        fetched_at: str | None = None,
    ) -> SourceFetchMetadata:
        return cls(
            url=url,
            http_status=http_status,
            etag=etag,
            last_modified=last_modified,
            fetched_at=fetched_at or _utc_now_iso(),
        )


@dataclass(frozen=True, slots=True)
class SourceSnapshotError:
    kind: SourceErrorKind
    message: str
    url: str
    http_status: int | None = None


@dataclass(frozen=True, slots=True)
class L2DSuModelSnapshot:
    kind: L2DSuModelKind
    costume_id: int
    costume_name: str
    costume_name_en: str
    path: str


@dataclass(frozen=True, slots=True)
class L2DSuCharacterSnapshot:
    char_id: int
    char_key: str
    char_name: str
    char_name_en: str
    live2d: tuple[L2DSuModelSnapshot, ...] = ()
    spine: tuple[L2DSuModelSnapshot, ...] = ()


@dataclass(frozen=True, slots=True)
class L2DSuCatalogData:
    game_id: int
    game_name: str
    source_game_count: int
    characters: tuple[L2DSuCharacterSnapshot, ...]

    def summary(self) -> dict[str, int]:
        return {
            'character_count': len(self.characters),
            'live2d_count': sum(len(character.live2d) for character in self.characters),
            'spine_count': sum(len(character.spine) for character in self.characters),
        }


@dataclass(frozen=True, slots=True)
class L2DSuSourceSnapshot:
    metadata: SourceFetchMetadata
    game_id: int = 0
    game_name: str = ''
    source_game_count: int = 0
    characters: tuple[L2DSuCharacterSnapshot, ...] = ()
    errors: tuple[SourceSnapshotError, ...] = ()
    source: Literal['l2d.su'] = 'l2d.su'

    def summary(self) -> dict[str, int]:
        return {
            'character_count': len(self.characters),
            'live2d_count': sum(len(character.live2d) for character in self.characters),
            'spine_count': sum(len(character.spine) for character in self.characters),
            'error_count': len(self.errors),
        }

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class NagamiMappingEntry:
    key: str
    name: str


@dataclass(frozen=True, slots=True)
class NagamiMappingData:
    entries: tuple[NagamiMappingEntry, ...]

    def summary(self) -> dict[str, int]:
        return {'entry_count': len(self.entries)}


@dataclass(frozen=True, slots=True)
class NagamiSourceSnapshot:
    metadata: SourceFetchMetadata
    entries: tuple[NagamiMappingEntry, ...] = ()
    errors: tuple[SourceSnapshotError, ...] = ()
    source: Literal['nagami'] = 'nagami'

    def summary(self) -> dict[str, int]:
        return {
            'entry_count': len(self.entries),
            'error_count': len(self.errors),
        }

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class AzurLaneSourceSnapshots:
    l2d_su: L2DSuSourceSnapshot
    nagami: NagamiSourceSnapshot

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class ModelCharacter:
    key: str
    id: int | None = None
    name_zh: str = ''
    name_en: str = ''


@dataclass(frozen=True, slots=True)
class ModelCostume:
    key: str
    id: int | None = None
    name_zh: str = ''
    name_en: str = ''


@dataclass(frozen=True, slots=True)
class ModelResources:
    primary_url: str
    fallback_url: str = ''
    display_info_url: str = ''


@dataclass(frozen=True, slots=True)
class ModelCapabilities:
    motions: tuple[str, ...] = ()
    expressions: tuple[str, ...] = ()
    has_audio: bool = False
    has_text: bool = False
    has_display_info: bool = False


@dataclass(frozen=True, slots=True)
class ModelLayout:
    mode: Literal['auto-fit'] = 'auto-fit'
    anchor: tuple[float, float] = (0.5, 0.5)
    scale_override: float | None = None
    offset_x: float | None = None
    offset_y: float | None = None


@dataclass(frozen=True, slots=True)
class ModelEntry:
    id: str
    type: L2DSuModelKind
    source: CatalogSource
    character: ModelCharacter
    costume: ModelCostume
    resources: ModelResources
    capabilities: ModelCapabilities = field(default_factory=ModelCapabilities)
    layout: ModelLayout = field(default_factory=ModelLayout)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class NagamiFallbackCandidate:
    key: str
    character_key: str
    costume_key: str
    name: str
    character_name_en: str
    costume_name_en: str
    url: str


@dataclass(frozen=True, slots=True)
class AzurLaneModelCatalog:
    entries: tuple[ModelEntry, ...]
    nagami_fallback_candidates: tuple[NagamiFallbackCandidate, ...] = ()

    def summary(self) -> dict[str, Any]:
        by_type = dict.fromkeys(_MODEL_TYPES, 0)
        by_source = dict.fromkeys(_CATALOG_SOURCES, 0)
        by_type_source = {model_type: dict.fromkeys(_CATALOG_SOURCES, 0) for model_type in _MODEL_TYPES}

        for entry in self.entries:
            by_type[entry.type] += 1
            by_source[entry.source] += 1
            by_type_source[entry.type][entry.source] += 1

        return {
            'entry_count': len(self.entries),
            'by_type': by_type,
            'by_source': by_source,
            'by_type_source': by_type_source,
            'nagami_fallback_candidate_count': len(self.nagami_fallback_candidates),
        }

    def search(self, query: str, *, model_type: L2DSuModelKind | None = None) -> tuple[ModelEntry, ...]:
        terms = tuple(term for term in _normalize_search_text(query).split() if term)
        entries = self.entries if model_type is None else tuple(entry for entry in self.entries if entry.type == model_type)
        if not terms:
            return entries
        return tuple(entry for entry in entries if _entry_matches_search(entry, terms))

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


def parse_l2d_su_catalog(source: str, *, game_id: int = AZUR_LANE_GAME_ID) -> L2DSuCatalogData:
    try:
        payload = json.loads(source)
    except json.JSONDecodeError as exc:
        msg = f'l2d.su catalog is not valid JSON: {exc.msg}'
        raise SourceParseError(msg) from exc

    if not isinstance(payload, dict):
        msg = 'l2d.su catalog root must be an object'
        raise SourceSchemaError(msg)
    masters = payload.get('Master')
    if not isinstance(masters, list):
        msg = 'l2d.su catalog must contain a Master list'
        raise SourceSchemaError(msg)

    selected = _select_l2d_su_master(masters, game_id=game_id)
    characters = selected.get('character')
    if not isinstance(characters, list):
        msg = f'l2d.su game {game_id} must contain a character list'
        raise SourceSchemaError(msg)

    return L2DSuCatalogData(
        game_id=_required_int(selected, 'gameId', context=f'l2d.su game {game_id}'),
        game_name=_required_str(selected, 'gameName', context=f'l2d.su game {game_id}'),
        source_game_count=len(masters),
        characters=tuple(_parse_l2d_su_character(item, index=index) for index, item in enumerate(characters)),
    )


def parse_nagami_mapping_bundle(source: str) -> NagamiMappingData:
    template_source = _extract_json_parse_template(source)
    json_source = _decode_js_template_literal(template_source)
    try:
        payload = json.loads(json_source)
    except json.JSONDecodeError as exc:
        msg = f'Nagami mapping bundle contains invalid JSON: {exc.msg}'
        raise SourceParseError(msg) from exc

    if not isinstance(payload, dict):
        msg = 'Nagami mapping root must be an object'
        raise SourceSchemaError(msg)

    entries: list[NagamiMappingEntry] = []
    for key, value in payload.items():
        if not isinstance(key, str) or not key.strip():
            msg = 'Nagami mapping keys must be non-empty strings'
            raise SourceSchemaError(msg)
        if not isinstance(value, str) or not value.strip():
            msg = f'Nagami mapping value for {key!r} must be a non-empty string'
            raise SourceSchemaError(msg)
        entries.append(NagamiMappingEntry(key=key, name=value))
    return NagamiMappingData(entries=tuple(sorted(entries, key=lambda entry: entry.key.casefold())))


def fetch_l2d_su_snapshot(
    *,
    url: str = L2D_SU_CATALOG_URL,
    timeout: float = 30.0,
    client: httpx.Client | None = None,
) -> L2DSuSourceSnapshot:
    response, metadata, error = _fetch_source(url=url, timeout=timeout, client=client)
    if error is not None:
        return L2DSuSourceSnapshot(metadata=metadata, errors=(error,))
    if response is None:
        return L2DSuSourceSnapshot(metadata=metadata, errors=(_source_error('network', 'Fetch did not return a response', metadata),))

    try:
        parsed = parse_l2d_su_catalog(response.text)
    except SourceParseError as exc:
        return L2DSuSourceSnapshot(metadata=metadata, errors=(_source_error('parse', str(exc), metadata),))
    except SourceSchemaError as exc:
        return L2DSuSourceSnapshot(metadata=metadata, errors=(_source_error('schema', str(exc), metadata),))

    return L2DSuSourceSnapshot(
        metadata=metadata,
        game_id=parsed.game_id,
        game_name=parsed.game_name,
        source_game_count=parsed.source_game_count,
        characters=parsed.characters,
    )


def fetch_nagami_snapshot(
    *,
    url: str = NAGAMI_MAPPING_BUNDLE_URL,
    timeout: float = 30.0,
    client: httpx.Client | None = None,
) -> NagamiSourceSnapshot:
    response, metadata, error = _fetch_source(url=url, timeout=timeout, client=client)
    if error is not None:
        return NagamiSourceSnapshot(metadata=metadata, errors=(error,))
    if response is None:
        return NagamiSourceSnapshot(metadata=metadata, errors=(_source_error('network', 'Fetch did not return a response', metadata),))

    try:
        parsed = parse_nagami_mapping_bundle(response.text)
    except SourceParseError as exc:
        return NagamiSourceSnapshot(metadata=metadata, errors=(_source_error('parse', str(exc), metadata),))
    except SourceSchemaError as exc:
        return NagamiSourceSnapshot(metadata=metadata, errors=(_source_error('schema', str(exc), metadata),))

    return NagamiSourceSnapshot(metadata=metadata, entries=parsed.entries)


def fetch_source_snapshots(
    *,
    timeout: float = 30.0,
    client: httpx.Client | None = None,
) -> AzurLaneSourceSnapshots:
    if client is not None:
        return AzurLaneSourceSnapshots(
            l2d_su=fetch_l2d_su_snapshot(timeout=timeout, client=client),
            nagami=fetch_nagami_snapshot(timeout=timeout, client=client),
        )

    with httpx.Client(follow_redirects=True, timeout=timeout, headers=_request_headers()) as owned_client:
        return AzurLaneSourceSnapshots(
            l2d_su=fetch_l2d_su_snapshot(timeout=timeout, client=owned_client),
            nagami=fetch_nagami_snapshot(timeout=timeout, client=owned_client),
        )


def build_azurlane_model_catalog(snapshots: AzurLaneSourceSnapshots) -> AzurLaneModelCatalog:
    nagami_candidates = build_nagami_fallback_candidates(snapshots.nagami)
    nagami_by_key = {candidate.key: candidate for candidate in nagami_candidates}
    matched_nagami_keys: set[str] = set()
    entries_by_id: dict[str, ModelEntry] = {}

    for character in sorted(snapshots.l2d_su.characters, key=lambda item: (_catalog_key(item.char_key), item.char_id)):
        l2d_su_models = (*character.live2d, *character.spine)
        for model in sorted(l2d_su_models, key=lambda item: (item.kind, _l2d_su_model_key(item), item.costume_id, item.path)):
            entry, matched_key = _l2d_su_entry(character=character, model=model, nagami_by_key=nagami_by_key)
            _add_catalog_entry(entries_by_id, entry)
            if matched_key:
                matched_nagami_keys.add(matched_key)

    for candidate in nagami_candidates:
        if candidate.key in matched_nagami_keys:
            continue
        _add_catalog_entry(entries_by_id, _nagami_entry(candidate))

    return AzurLaneModelCatalog(
        entries=tuple(entries_by_id[key] for key in sorted(entries_by_id)),
        nagami_fallback_candidates=nagami_candidates,
    )


def build_nagami_fallback_candidates(snapshot: NagamiSourceSnapshot) -> tuple[NagamiFallbackCandidate, ...]:
    return tuple(_nagami_fallback_candidate(entry) for entry in sorted(snapshot.entries, key=lambda item: _catalog_key(item.key)))


def _l2d_su_entry(
    *,
    character: L2DSuCharacterSnapshot,
    model: L2DSuModelSnapshot,
    nagami_by_key: dict[str, NagamiFallbackCandidate],
) -> tuple[ModelEntry, str]:
    model_key = _l2d_su_model_key(model)
    fallback_match_key = _l2d_su_fallback_match_key(model)
    fallback = nagami_by_key.get(fallback_match_key) if fallback_match_key else None
    source: CatalogSource = 'merged' if fallback is not None else 'l2d.su'
    fallback_url = fallback.url if fallback is not None else ''

    return (
        ModelEntry(
            id=_catalog_entry_id(model.kind, character.char_key, model_key),
            type=model.kind,
            source=source,
            character=ModelCharacter(
                id=character.char_id,
                key=_catalog_key(character.char_key),
                name_zh=character.char_name,
                name_en=character.char_name_en,
            ),
            costume=ModelCostume(
                id=model.costume_id,
                key=_catalog_key(model_key),
                name_zh=model.costume_name,
                name_en=model.costume_name_en,
            ),
            resources=ModelResources(primary_url=model.path, fallback_url=fallback_url),
        ),
        fallback.key if fallback is not None else '',
    )


def _nagami_entry(candidate: NagamiFallbackCandidate) -> ModelEntry:
    return ModelEntry(
        id=_catalog_entry_id('live2d', candidate.character_key, candidate.costume_key),
        type='live2d',
        source='nagami',
        character=ModelCharacter(key=candidate.character_key, name_en=candidate.character_name_en),
        costume=ModelCostume(key=candidate.costume_key, name_en=candidate.costume_name_en),
        resources=ModelResources(primary_url=candidate.url),
    )


def _nagami_fallback_candidate(entry: NagamiMappingEntry) -> NagamiFallbackCandidate:
    key = _catalog_key(entry.key)
    character_name, costume_name = _split_nagami_name(entry.name)
    return NagamiFallbackCandidate(
        key=key,
        character_key=_nagami_character_key(key),
        costume_key=key,
        name=entry.name,
        character_name_en=character_name,
        costume_name_en=costume_name,
        url=_nagami_model_url(key),
    )


def _add_catalog_entry(entries_by_id: dict[str, ModelEntry], entry: ModelEntry) -> None:
    existing = entries_by_id.get(entry.id)
    if existing is None:
        entries_by_id[entry.id] = entry
        return

    if existing.resources.primary_url == entry.resources.primary_url:
        entries_by_id[entry.id] = _merge_same_asset_entries(existing, entry)
        return

    variant_entry = replace(entry, id=_variant_entry_id(entry, entries_by_id))
    variant_existing = entries_by_id.get(variant_entry.id)
    if variant_existing is not None and variant_existing.resources.primary_url == variant_entry.resources.primary_url:
        entries_by_id[variant_entry.id] = _merge_same_asset_entries(variant_existing, variant_entry)
        return
    entries_by_id[variant_entry.id] = variant_entry


def _merge_same_asset_entries(existing: ModelEntry, incoming: ModelEntry) -> ModelEntry:
    fallback_url = existing.resources.fallback_url or incoming.resources.fallback_url
    source: CatalogSource = 'merged' if fallback_url else existing.source
    return replace(
        existing,
        source=source,
        resources=replace(existing.resources, fallback_url=fallback_url),
    )


def _variant_entry_id(entry: ModelEntry, entries_by_id: dict[str, ModelEntry]) -> str:
    token = blake2b(entry.resources.primary_url.encode(), digest_size=CATALOG_VARIANT_TOKEN_SIZE).hexdigest()
    variant_id = f'{entry.id}:asset-{token}'
    index = 2
    while variant_id in entries_by_id and entries_by_id[variant_id].resources.primary_url != entry.resources.primary_url:
        variant_id = f'{entry.id}:asset-{token}-{index}'
        index += 1
    return variant_id


def _catalog_entry_id(model_type: L2DSuModelKind, character_key: str, costume_key: str) -> str:
    return f'{CATALOG_ENTRY_ID_PREFIX}:{model_type}:{_catalog_key(character_key)}:{_catalog_key(costume_key)}'


def _l2d_su_model_key(model: L2DSuModelSnapshot) -> str:
    path_segments = _l2d_su_path_segments(model.path)
    filename = path_segments[-1] if path_segments else ''
    if model.kind == 'live2d':
        parent = path_segments[-2] if len(path_segments) > 1 else ''
        if parent:
            return _catalog_key(parent)
        filename = filename.removesuffix('.model3.json')
    if filename:
        return _catalog_key(filename)
    return f'costume-{model.costume_id}'


def _l2d_su_fallback_match_key(model: L2DSuModelSnapshot) -> str:
    if model.kind != 'live2d':
        return ''

    path_segments = _l2d_su_path_segments(model.path)
    if not path_segments:
        return ''

    *parent_segments, filename = path_segments
    if not parent_segments:
        return ''

    parent = _catalog_key(parent_segments[-1])
    if not filename.endswith('.model3.json'):
        return ''

    model_stem = _catalog_key(filename.removesuffix('.model3.json'))
    if parent != model_stem:
        return ''
    return model_stem


def _l2d_su_path_segments(path: str) -> tuple[str, ...]:
    return tuple(segment for segment in urlsplit(path).path.split('/') if segment)


def _nagami_character_key(key: str) -> str:
    character_key = _NAGAMI_COSTUME_SUFFIX_RE.sub('', key)
    return character_key or key


def _split_nagami_name(name: str) -> tuple[str, str]:
    character_name, separator, costume_name = name.partition(' - ')
    if separator:
        return character_name.strip(), costume_name.strip()
    return name.strip(), ''


def _nagami_model_url(key: str) -> str:
    return f'{NAGAMI_LIVE2D_BASE_URL}/{key}/{key}.model3.json'


def _entry_matches_search(entry: ModelEntry, terms: tuple[str, ...]) -> bool:
    haystack = _entry_search_text(entry)
    return all(term in haystack for term in terms)


def _entry_search_text(entry: ModelEntry) -> str:
    values = (
        entry.id,
        entry.type,
        entry.source,
        entry.character.key,
        entry.character.name_zh,
        entry.character.name_en,
        entry.costume.key,
        entry.costume.name_zh,
        entry.costume.name_en,
    )
    return _normalize_search_text(' '.join(value for value in values if value))


def _normalize_search_text(value: str) -> str:
    return unicodedata.normalize('NFKC', value).casefold()


def _catalog_key(value: str) -> str:
    normalized = unicodedata.normalize('NFKC', value.strip())
    if normalized:
        return normalized
    return 'unknown'


def _fetch_source(
    *,
    url: str,
    timeout: float,
    client: httpx.Client | None,
) -> tuple[httpx.Response | None, SourceFetchMetadata, SourceSnapshotError | None]:
    try:
        response = _client_get(url=url, timeout=timeout, client=client)
    except httpx.HTTPError as exc:
        metadata = SourceFetchMetadata.for_url(url)
        message = f'Failed to fetch {url}: {exc}'
        return None, metadata, SourceSnapshotError(kind='network', message=message, url=url)

    metadata = _metadata_from_response(url=url, response=response)
    if response.status_code >= HTTP_ERROR_MIN:
        message = f'HTTP {response.status_code} while fetching {url}'
        return response, metadata, _source_error('network', message, metadata)
    return response, metadata, None


def _client_get(*, url: str, timeout: float, client: httpx.Client | None) -> httpx.Response:
    if client is not None:
        return client.get(url)

    with httpx.Client(follow_redirects=True, timeout=timeout, headers=_request_headers()) as owned_client:
        return owned_client.get(url)


def _metadata_from_response(*, url: str, response: httpx.Response) -> SourceFetchMetadata:
    return SourceFetchMetadata.for_url(
        str(response.url) or url,
        http_status=response.status_code,
        etag=response.headers.get('etag', ''),
        last_modified=response.headers.get('last-modified', ''),
    )


def _source_error(kind: SourceErrorKind, message: str, metadata: SourceFetchMetadata) -> SourceSnapshotError:
    return SourceSnapshotError(kind=kind, message=message, url=metadata.url, http_status=metadata.http_status)


def _select_l2d_su_master(masters: list[Any], *, game_id: int) -> dict[str, Any]:
    for item in masters:
        if isinstance(item, dict) and item.get('gameId') == game_id:
            return item
    msg = f'l2d.su catalog does not contain gameId={game_id}'
    raise SourceSchemaError(msg)


def _parse_l2d_su_character(item: Any, *, index: int) -> L2DSuCharacterSnapshot:
    context = f'l2d.su character[{index}]'
    if not isinstance(item, dict):
        msg = f'{context} must be an object'
        raise SourceSchemaError(msg)

    return L2DSuCharacterSnapshot(
        char_id=_required_int(item, 'charId', context=context),
        char_key=_required_str(item, 'charKey', context=context),
        char_name=_required_str(item, 'charName', context=context),
        char_name_en=_required_str(item, 'charNameEn', context=context),
        live2d=_parse_l2d_su_models(item, field_name='live2d', context=context),
        spine=_parse_l2d_su_models(item, field_name='spine', context=context),
    )


def _parse_l2d_su_models(item: dict[str, Any], *, field_name: L2DSuModelKind, context: str) -> tuple[L2DSuModelSnapshot, ...]:
    models = item.get(field_name, [])
    if not isinstance(models, list):
        msg = f'{context}.{field_name} must be a list'
        raise SourceSchemaError(msg)

    return tuple(
        L2DSuModelSnapshot(
            kind=field_name,
            costume_id=_required_int(model, 'costumeId', context=f'{context}.{field_name}[{index}]'),
            costume_name=_required_str(model, 'costumeName', context=f'{context}.{field_name}[{index}]'),
            costume_name_en=_required_str(model, 'costumeNameEn', context=f'{context}.{field_name}[{index}]'),
            path=_required_str(model, 'path', context=f'{context}.{field_name}[{index}]'),
        )
        for index, model in enumerate(models)
        if _assert_object(model, context=f'{context}.{field_name}[{index}]')
    )


def _assert_object(value: Any, *, context: str) -> bool:
    if isinstance(value, dict):
        return True
    msg = f'{context} must be an object'
    raise SourceSchemaError(msg)


def _required_int(item: dict[str, Any], field_name: str, *, context: str) -> int:
    value = item.get(field_name)
    if isinstance(value, bool) or not isinstance(value, int):
        msg = f'{context}.{field_name} must be an integer'
        raise SourceSchemaError(msg)
    return value


def _required_str(item: dict[str, Any], field_name: str, *, context: str) -> str:
    value = item.get(field_name)
    if not isinstance(value, str) or not value.strip():
        msg = f'{context}.{field_name} must be a non-empty string'
        raise SourceSchemaError(msg)
    return value


def _extract_json_parse_template(source: str) -> str:
    marker = 'JSON.parse(`'
    start = source.find(marker)
    if start < 0:
        msg = 'Nagami mapping bundle does not contain JSON.parse(`...`)'
        raise SourceParseError(msg)

    template_start = start + len(marker)
    escaped = False
    for index, char in enumerate(source[template_start:], start=template_start):
        if escaped:
            escaped = False
            continue
        if char == '\\':
            escaped = True
            continue
        if char == '`':
            return source[template_start:index]

    msg = 'Nagami mapping bundle template literal is not closed'
    raise SourceParseError(msg)


def _decode_js_template_literal(source: str) -> str:  # noqa: C901, PLR0912
    decoded: list[str] = []
    index = 0
    while index < len(source):
        char = source[index]
        if char != '\\':
            decoded.append(char)
            index += 1
            continue

        index += 1
        if index >= len(source):
            msg = 'Nagami mapping bundle has a dangling template escape'
            raise SourceParseError(msg)

        escaped = source[index]
        if escaped in {'`', '\\', '$', '"', "'"}:
            decoded.append(escaped)
        elif escaped == 'n':
            decoded.append('\n')
        elif escaped == 'r':
            decoded.append('\r')
        elif escaped == 't':
            decoded.append('\t')
        elif escaped == 'b':
            decoded.append('\b')
        elif escaped == 'f':
            decoded.append('\f')
        elif escaped == 'v':
            decoded.append('\v')
        elif escaped == '0':
            decoded.append('\0')
        elif escaped == 'x':
            index = _append_hex_escape(source=source, index=index, length=2, decoded=decoded)
        elif escaped == 'u':
            index = _append_unicode_escape(source=source, index=index, decoded=decoded)
        elif escaped in {'\n', '\r'}:
            if escaped == '\r' and index + 1 < len(source) and source[index + 1] == '\n':
                index += 1
        else:
            decoded.append(escaped)
        index += 1
    return ''.join(decoded)


def _append_hex_escape(*, source: str, index: int, length: int, decoded: list[str]) -> int:
    start = index + 1
    end = start + length
    if end > len(source):
        msg = 'Nagami mapping bundle has an incomplete hex escape'
        raise SourceParseError(msg)
    raw = source[start:end]
    try:
        decoded.append(chr(int(raw, 16)))
    except ValueError as exc:
        msg = f'Nagami mapping bundle has an invalid hex escape: {raw!r}'
        raise SourceParseError(msg) from exc
    return end - 1


def _append_unicode_escape(*, source: str, index: int, decoded: list[str]) -> int:
    if index + 1 < len(source) and source[index + 1] == '{':
        end = source.find('}', index + 2)
        if end < 0:
            msg = 'Nagami mapping bundle has an unclosed unicode escape'
            raise SourceParseError(msg)
        raw = source[index + 2 : end]
        try:
            decoded.append(chr(int(raw, 16)))
        except ValueError as exc:
            msg = f'Nagami mapping bundle has an invalid unicode escape: {raw!r}'
            raise SourceParseError(msg) from exc
        return end
    return _append_hex_escape(source=source, index=index, length=4, decoded=decoded)


def _request_headers() -> dict[str, str]:
    return {
        'Accept': 'application/json, text/javascript, */*',
        'User-Agent': DEFAULT_USER_AGENT,
    }


def _utc_now_iso() -> str:
    return datetime.now(UTC).isoformat()

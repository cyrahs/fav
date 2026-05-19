from __future__ import annotations

import json
import re
import unicodedata
from dataclasses import asdict, dataclass, field, replace
from datetime import UTC, datetime
from hashlib import blake2b
from pathlib import PurePosixPath
from typing import TYPE_CHECKING, Any, Literal
from urllib.parse import urljoin, urlsplit

import httpx

from src.tool.filename import sanitize

if TYPE_CHECKING:
    from collections.abc import Iterable

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
AvailabilityState = Literal['valid', 'fallback-only', 'broken', 'unchecked']
ResourceValidationStatus = Literal['ok', 'missing', 'network', 'parse', 'schema', 'unchecked']
ResourceValidationSource = Literal['primary', 'fallback']
ResourceAssetKind = Literal[
    'live2d.model3',
    'live2d.moc3',
    'live2d.texture',
    'live2d.physics',
    'live2d.pose',
    'live2d.display-info',
    'live2d.expression',
    'live2d.motion',
    'live2d.audio',
    'live2d.text',
    'spine.skel',
    'spine.atlas',
    'spine.texture',
]

_MODEL_TYPES: tuple[L2DSuModelKind, ...] = ('live2d', 'spine')
_CATALOG_SOURCES: tuple[CatalogSource, ...] = ('l2d.su', 'nagami', 'merged')
_AVAILABILITY_STATES: tuple[AvailabilityState, ...] = ('valid', 'fallback-only', 'broken', 'unchecked')
_NAGAMI_COSTUME_SUFFIX_RE = re.compile(r'_\d+$')
_RESOURCE_EXTENSION_BY_KIND: dict[ResourceAssetKind, str] = {
    'live2d.model3': '.model3.json',
    'live2d.moc3': '.moc3',
    'live2d.texture': '.webp',
    'live2d.physics': '.physics3.json',
    'live2d.pose': '.pose3.json',
    'live2d.display-info': '.cdi3.json',
    'live2d.expression': '.exp3.json',
    'live2d.motion': '.motion3.json',
    'live2d.audio': '.wav',
    'live2d.text': '.txt',
    'spine.skel': '.skel',
    'spine.atlas': '.atlas',
    'spine.texture': '.webp',
}


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
class ModelResourceSummary:
    moc3: str = ''
    textures: tuple[str, ...] = ()
    physics: str = ''
    display_info: str = ''
    motions: tuple[str, ...] = ()
    expressions: tuple[str, ...] = ()
    has_audio: bool = False
    has_text: bool = False
    has_display_info: bool = False


@dataclass(frozen=True, slots=True)
class ModelAvailability:
    state: AvailabilityState = 'unchecked'
    validated_url: str = ''
    checked_at: str = ''
    message: str = ''


@dataclass(frozen=True, slots=True)
class ModelEntry:
    id: str
    type: L2DSuModelKind
    source: CatalogSource
    character: ModelCharacter
    costume: ModelCostume
    resources: ModelResources
    resource_summary: ModelResourceSummary = field(default_factory=ModelResourceSummary)
    availability: ModelAvailability = field(default_factory=ModelAvailability)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class AzurLaneEnumeratedResource:
    model_id: str
    model_type: L2DSuModelKind
    character_key: str
    costume_key: str
    kind: ResourceAssetKind
    source_url: str
    fallback_url: str = ''
    local_path: str = ''
    original_filename: str = ''
    contexts: tuple[dict[str, Any], ...] = ()

    @property
    def context(self) -> dict[str, Any]:
        return self.contexts[0] if self.contexts else {}


@dataclass(frozen=True, slots=True)
class ResourceEnumerationIssue:
    model_id: str
    kind: ResourceAssetKind
    message: str
    context: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True, slots=True)
class AzurLaneModelResourceEnumeration:
    model_id: str
    model_type: L2DSuModelKind
    character_key: str
    costume_key: str
    source_url: str
    fallback_url: str
    assets: tuple[AzurLaneEnumeratedResource, ...]
    issues: tuple[ResourceEnumerationIssue, ...] = ()

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class ResourceCheck:
    kind: ResourceAssetKind
    url: str
    ok: bool
    status: ResourceValidationStatus
    http_status: int | None = None
    message: str = ''
    source: ResourceValidationSource | None = None


@dataclass(frozen=True, slots=True)
class ResourceValidationEntry:
    entry_id: str
    entry_type: L2DSuModelKind
    availability: ModelAvailability
    resources: ModelResources
    resource_summary: ModelResourceSummary
    checks: tuple[ResourceCheck, ...] = ()

    def has_available_resources(self) -> bool:
        return self.availability.state in {'valid', 'fallback-only'}


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


@dataclass(frozen=True, slots=True)
class AzurLaneResourceValidationReport:
    checked_at: str
    catalog: AzurLaneModelCatalog
    entries: tuple[ResourceValidationEntry, ...]

    def summary(self) -> dict[str, Any]:
        by_state = dict.fromkeys(_AVAILABILITY_STATES, 0)
        by_type_state = {model_type: dict.fromkeys(_AVAILABILITY_STATES, 0) for model_type in _MODEL_TYPES}

        for entry in self.entries:
            by_state[entry.availability.state] += 1
            by_type_state[entry.entry_type][entry.availability.state] += 1

        return {
            'entry_count': len(self.entries),
            'by_state': by_state,
            'by_type_state': by_type_state,
        }

    def broken_entries(self) -> tuple[ResourceValidationEntry, ...]:
        return tuple(entry for entry in self.entries if entry.availability.state == 'broken')

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class SourceEndpointHealth:
    source: Literal['l2d.su', 'nagami']
    url: str
    http_status: int | None
    etag: str
    last_modified: str
    fetched_at: str
    entry_count: int
    entry_ids: tuple[str, ...] = ()
    errors: tuple[SourceSnapshotError, ...] = ()

    @property
    def ok(self) -> bool:
        return not self.errors and self.http_status is not None and self.http_status < HTTP_ERROR_MIN


@dataclass(frozen=True, slots=True)
class SourceDrift:
    source: Literal['l2d.su', 'nagami']
    previous_entry_count: int | None
    current_entry_count: int
    entry_count_delta: int | None
    added_entry_ids: tuple[str, ...] = ()
    removed_entry_ids: tuple[str, ...] = ()
    etag_changed: bool = False
    last_modified_changed: bool = False
    http_status_changed: bool = False

    @property
    def changed(self) -> bool:
        return bool(
            self.entry_count_delta
            or self.added_entry_ids
            or self.removed_entry_ids
            or self.etag_changed
            or self.last_modified_changed
            or self.http_status_changed,
        )


@dataclass(frozen=True, slots=True)
class SourceHealthReport:
    l2d_su: SourceEndpointHealth
    nagami: SourceEndpointHealth

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class CatalogHealthReport:
    entry_count: int
    by_type: dict[str, int]
    by_source: dict[str, int]
    entry_ids: tuple[str, ...] = ()
    new_entry_ids: tuple[str, ...] = ()
    removed_entry_ids: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class ResourceHealthReport:
    checked_at: str
    entry_count: int
    broken_resource_count: int
    unchecked_resource_count: int
    fallback_only_count: int
    broken_entry_ids: tuple[str, ...] = ()
    newly_broken_entry_ids: tuple[str, ...] = ()
    recovered_entry_ids: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class DriftSummary:
    l2d_su: SourceDrift
    nagami: SourceDrift
    previous_catalog_entry_count: int | None
    current_catalog_entry_count: int
    catalog_entry_count_delta: int | None
    newly_added_resource_count: int
    broken_resource_count_delta: int | None

    @property
    def changed(self) -> bool:
        return bool(
            self.l2d_su.changed
            or self.nagami.changed
            or self.catalog_entry_count_delta
            or self.newly_added_resource_count
            or self.broken_resource_count_delta,
        )


@dataclass(frozen=True, slots=True)
class AzurLaneL2DHealthReport:
    checked_at: str
    source_health: SourceHealthReport
    catalog_health: CatalogHealthReport
    resource_health: ResourceHealthReport
    drift_summary: DriftSummary
    snapshots: AzurLaneSourceSnapshots | None = None
    catalog: AzurLaneModelCatalog | None = None
    resource_validation: AzurLaneResourceValidationReport | None = None

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class ResourceReference:
    kind: ResourceAssetKind
    url: str
    raw_path: str
    context: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True, slots=True)
class Live2DResourceManifest:
    moc3_url: str
    texture_urls: tuple[str, ...]
    physics_url: str = ''
    pose_url: str = ''
    display_info_url: str = ''
    expression_urls: tuple[str, ...] = ()
    motion_urls: tuple[str, ...] = ()
    audio_urls: tuple[str, ...] = ()
    text_urls: tuple[str, ...] = ()
    motion_names: tuple[str, ...] = ()
    expression_names: tuple[str, ...] = ()
    has_audio: bool = False
    has_text: bool = False
    references: tuple[ResourceReference, ...] = ()


@dataclass(frozen=True, slots=True)
class SpineResourceManifest:
    skel_url: str
    atlas_url: str
    texture_urls: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class ResourceCandidateValidation:
    ok: bool
    url: str
    resource_summary: ModelResourceSummary
    display_info_url: str
    checks: tuple[ResourceCheck, ...]
    message: str = ''


@dataclass(frozen=True, slots=True)
class ResourceRequestContext:
    timeout: float
    client: httpx.Client


@dataclass(frozen=True, slots=True)
class _ResourceSpec:
    kind: ResourceAssetKind
    source_url: str
    fallback_url: str
    base_url: str
    context: dict[str, Any]


@dataclass(frozen=True, slots=True)
class _Live2DReferenceSpec:
    kind: ResourceAssetKind
    base_url: str
    metadata: dict[str, Any]
    metadata_key: str = ''


@dataclass(frozen=True, slots=True)
class _Live2DMotionReferences:
    motion: ResourceReference | None = None
    audio: ResourceReference | None = None
    text: ResourceReference | None = None
    name: str = ''


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


def enumerate_azurlane_model_resources(
    entry: ModelEntry,
    *,
    model3_source: str | None = None,
    atlas_source: str | None = None,
) -> AzurLaneModelResourceEnumeration:
    if entry.type == 'live2d':
        if model3_source is None:
            msg = f'Live2D entry {entry.id} requires model3_source for resource enumeration'
            raise SourceSchemaError(msg)
        specs = _live2d_resource_specs(entry, model3_source=model3_source)
    else:
        specs = _spine_resource_specs(entry, atlas_source=atlas_source)

    return AzurLaneModelResourceEnumeration(
        model_id=entry.id,
        model_type=entry.type,
        character_key=entry.character.key,
        costume_key=entry.costume.key,
        source_url=entry.resources.primary_url,
        fallback_url=entry.resources.fallback_url,
        assets=_enumerated_resources_from_specs(entry, specs),
    )


def validate_azurlane_model_catalog_resources(
    catalog: AzurLaneModelCatalog,
    *,
    timeout: float = 30.0,
    client: httpx.Client | None = None,
    entry_ids: Iterable[str] | None = None,
) -> AzurLaneResourceValidationReport:
    checked_at = _utc_now_iso()
    selected_ids = set(entry_ids) if entry_ids is not None else None

    if client is not None:
        return _validate_catalog_resources(catalog, checked_at=checked_at, timeout=timeout, client=client, selected_ids=selected_ids)

    with httpx.Client(follow_redirects=True, timeout=timeout, headers=_request_headers()) as owned_client:
        return _validate_catalog_resources(catalog, checked_at=checked_at, timeout=timeout, client=owned_client, selected_ids=selected_ids)


def fetch_azurlane_l2d_health_report(
    *,
    timeout: float = 30.0,
    client: httpx.Client | None = None,
    validate_entry_ids: Iterable[str] | Literal['all'] | None = None,
    previous_report: AzurLaneL2DHealthReport | None = None,
) -> AzurLaneL2DHealthReport:
    snapshots = fetch_source_snapshots(timeout=timeout, client=client)
    catalog = build_azurlane_model_catalog(snapshots)
    resource_validation = None
    if validate_entry_ids is not None:
        entry_ids = None if validate_entry_ids == 'all' else validate_entry_ids
        resource_validation = validate_azurlane_model_catalog_resources(catalog, timeout=timeout, client=client, entry_ids=entry_ids)
        catalog = resource_validation.catalog
    return build_azurlane_l2d_health_report(
        snapshots=snapshots,
        catalog=catalog,
        resource_validation=resource_validation,
        previous_report=previous_report,
    )


def build_azurlane_l2d_health_report(
    *,
    snapshots: AzurLaneSourceSnapshots,
    catalog: AzurLaneModelCatalog | None = None,
    resource_validation: AzurLaneResourceValidationReport | None = None,
    previous_report: AzurLaneL2DHealthReport | None = None,
) -> AzurLaneL2DHealthReport:
    current_catalog = catalog or build_azurlane_model_catalog(snapshots)
    current_resource_validation = resource_validation
    source_health = SourceHealthReport(
        l2d_su=_l2d_su_endpoint_health(snapshots.l2d_su),
        nagami=_nagami_endpoint_health(snapshots.nagami),
    )
    catalog_health = _catalog_health_report(current_catalog, previous_report=previous_report)
    resource_health = _resource_health_report(
        catalog=current_catalog,
        resource_validation=current_resource_validation,
        previous_report=previous_report,
    )
    drift_summary = _drift_summary(
        source_health=source_health,
        snapshots=snapshots,
        catalog_health=catalog_health,
        resource_health=resource_health,
        previous_report=previous_report,
    )
    return AzurLaneL2DHealthReport(
        checked_at=_utc_now_iso(),
        source_health=source_health,
        catalog_health=catalog_health,
        resource_health=resource_health,
        drift_summary=drift_summary,
        snapshots=snapshots,
        catalog=current_catalog,
        resource_validation=current_resource_validation,
    )


def _l2d_su_endpoint_health(snapshot: L2DSuSourceSnapshot) -> SourceEndpointHealth:
    entry_ids = _l2d_su_source_entry_ids(snapshot)
    return SourceEndpointHealth(
        source='l2d.su',
        url=snapshot.metadata.url,
        http_status=snapshot.metadata.http_status,
        etag=snapshot.metadata.etag,
        last_modified=snapshot.metadata.last_modified,
        fetched_at=snapshot.metadata.fetched_at,
        entry_count=sum(len(character.live2d) + len(character.spine) for character in snapshot.characters),
        entry_ids=entry_ids,
        errors=snapshot.errors,
    )


def _nagami_endpoint_health(snapshot: NagamiSourceSnapshot) -> SourceEndpointHealth:
    entry_ids = _nagami_source_entry_ids(snapshot)
    return SourceEndpointHealth(
        source='nagami',
        url=snapshot.metadata.url,
        http_status=snapshot.metadata.http_status,
        etag=snapshot.metadata.etag,
        last_modified=snapshot.metadata.last_modified,
        fetched_at=snapshot.metadata.fetched_at,
        entry_count=len(snapshot.entries),
        entry_ids=entry_ids,
        errors=snapshot.errors,
    )


def _catalog_health_report(
    catalog: AzurLaneModelCatalog,
    *,
    previous_report: AzurLaneL2DHealthReport | None,
) -> CatalogHealthReport:
    summary = catalog.summary()
    entry_ids = tuple(entry.id for entry in catalog.entries)
    previous_ids = previous_report.catalog_health.entry_ids if previous_report is not None else entry_ids
    return CatalogHealthReport(
        entry_count=len(entry_ids),
        by_type=dict(summary['by_type']),
        by_source=dict(summary['by_source']),
        entry_ids=entry_ids,
        new_entry_ids=_sorted_delta(entry_ids, previous_ids),
        removed_entry_ids=_sorted_delta(previous_ids, entry_ids),
    )


def _resource_health_report(
    *,
    catalog: AzurLaneModelCatalog,
    resource_validation: AzurLaneResourceValidationReport | None,
    previous_report: AzurLaneL2DHealthReport | None,
) -> ResourceHealthReport:
    if resource_validation is None:
        state_by_entry_id = {entry.id: entry.availability.state for entry in catalog.entries}
        checked_at = ''
    else:
        state_by_entry_id = {entry.entry_id: entry.availability.state for entry in resource_validation.entries}
        checked_at = resource_validation.checked_at

    broken_ids = tuple(sorted(entry_id for entry_id, state in state_by_entry_id.items() if state == 'broken'))
    previous_broken_ids = previous_report.resource_health.broken_entry_ids if previous_report is not None else broken_ids
    return ResourceHealthReport(
        checked_at=checked_at,
        entry_count=len(state_by_entry_id),
        broken_resource_count=len(broken_ids),
        unchecked_resource_count=sum(1 for state in state_by_entry_id.values() if state == 'unchecked'),
        fallback_only_count=sum(1 for state in state_by_entry_id.values() if state == 'fallback-only'),
        broken_entry_ids=broken_ids,
        newly_broken_entry_ids=_sorted_delta(broken_ids, previous_broken_ids),
        recovered_entry_ids=_sorted_delta(previous_broken_ids, broken_ids),
    )


def _drift_summary(
    *,
    source_health: SourceHealthReport,
    snapshots: AzurLaneSourceSnapshots,
    catalog_health: CatalogHealthReport,
    resource_health: ResourceHealthReport,
    previous_report: AzurLaneL2DHealthReport | None,
) -> DriftSummary:
    previous_catalog_count = previous_report.catalog_health.entry_count if previous_report is not None else None
    previous_broken_count = previous_report.resource_health.broken_resource_count if previous_report is not None else None
    return DriftSummary(
        l2d_su=_source_drift(
            current=source_health.l2d_su,
            previous=previous_report.source_health.l2d_su if previous_report is not None else None,
            current_entry_ids=_l2d_su_source_entry_ids(snapshots.l2d_su),
        ),
        nagami=_source_drift(
            current=source_health.nagami,
            previous=previous_report.source_health.nagami if previous_report is not None else None,
            current_entry_ids=_nagami_source_entry_ids(snapshots.nagami),
        ),
        previous_catalog_entry_count=previous_catalog_count,
        current_catalog_entry_count=catalog_health.entry_count,
        catalog_entry_count_delta=_optional_delta(catalog_health.entry_count, previous_catalog_count),
        newly_added_resource_count=len(catalog_health.new_entry_ids),
        broken_resource_count_delta=_optional_delta(resource_health.broken_resource_count, previous_broken_count),
    )


def _source_drift(
    *,
    current: SourceEndpointHealth,
    previous: SourceEndpointHealth | None,
    current_entry_ids: tuple[str, ...],
) -> SourceDrift:
    if previous is None:
        return SourceDrift(
            source=current.source,
            previous_entry_count=None,
            current_entry_count=current.entry_count,
            entry_count_delta=None,
            added_entry_ids=(),
            removed_entry_ids=(),
        )

    return SourceDrift(
        source=current.source,
        previous_entry_count=previous.entry_count,
        current_entry_count=current.entry_count,
        entry_count_delta=current.entry_count - previous.entry_count,
        added_entry_ids=_sorted_delta(current_entry_ids, previous.entry_ids),
        removed_entry_ids=_sorted_delta(previous.entry_ids, current_entry_ids),
        etag_changed=current.etag != previous.etag,
        last_modified_changed=current.last_modified != previous.last_modified,
        http_status_changed=current.http_status != previous.http_status,
    )


def _optional_delta(current: int, previous: int | None) -> int | None:
    return None if previous is None else current - previous


def _sorted_delta(current: Iterable[str], previous: Iterable[str]) -> tuple[str, ...]:
    return tuple(sorted(set(current) - set(previous)))


def _l2d_su_source_entry_ids(snapshot: L2DSuSourceSnapshot) -> tuple[str, ...]:
    entry_ids = [
        _catalog_entry_id(model.kind, character.char_key, _l2d_su_model_key(model))
        for character in snapshot.characters
        for model in (*character.live2d, *character.spine)
    ]
    return tuple(sorted(set(entry_ids)))


def _nagami_source_entry_ids(snapshot: NagamiSourceSnapshot) -> tuple[str, ...]:
    entry_ids = (
        _catalog_entry_id('live2d', _nagami_character_key(_catalog_key(entry.key)), _catalog_key(entry.key)) for entry in snapshot.entries
    )
    return tuple(sorted(set(entry_ids)))


def _validate_catalog_resources(
    catalog: AzurLaneModelCatalog,
    *,
    checked_at: str,
    timeout: float,
    client: httpx.Client,
    selected_ids: set[str] | None,
) -> AzurLaneResourceValidationReport:
    updated_entries: list[ModelEntry] = []
    validation_entries: list[ResourceValidationEntry] = []

    for entry in catalog.entries:
        if selected_ids is not None and entry.id not in selected_ids:
            unchecked_entry = _unchecked_catalog_entry(entry, checked_at=checked_at)
            updated_entries.append(unchecked_entry)
            validation_entries.append(_resource_validation_entry(unchecked_entry, checks=()))
            continue

        validated_entry, validation_entry = _validate_catalog_entry_resources(entry, checked_at=checked_at, timeout=timeout, client=client)
        updated_entries.append(validated_entry)
        validation_entries.append(validation_entry)

    validated_catalog = replace(catalog, entries=tuple(updated_entries))
    return AzurLaneResourceValidationReport(checked_at=checked_at, catalog=validated_catalog, entries=tuple(validation_entries))


def _validate_catalog_entry_resources(
    entry: ModelEntry,
    *,
    checked_at: str,
    timeout: float,
    client: httpx.Client,
) -> tuple[ModelEntry, ResourceValidationEntry]:
    primary_validation = _validate_resource_candidate(
        entry,
        url=entry.resources.primary_url,
        source='primary',
        timeout=timeout,
        client=client,
    )
    validations = [primary_validation]
    if primary_validation.ok:
        return _validated_entry_result(
            entry,
            validation=primary_validation,
            all_checks=primary_validation.checks,
            state='valid',
            checked_at=checked_at,
        )

    fallback_url = entry.resources.fallback_url
    if fallback_url:
        fallback_validation = _validate_resource_candidate(entry, url=fallback_url, source='fallback', timeout=timeout, client=client)
        validations.append(fallback_validation)
        if fallback_validation.ok:
            return _validated_entry_result(
                entry,
                validation=fallback_validation,
                all_checks=(*primary_validation.checks, *fallback_validation.checks),
                state='fallback-only',
                checked_at=checked_at,
            )

    checks = tuple(check for validation in validations for check in validation.checks)
    message = '; '.join(validation.message for validation in validations if validation.message)
    broken_entry = replace(
        entry,
        availability=ModelAvailability(state='broken', checked_at=checked_at, message=message or 'No valid resource URL found'),
    )
    return broken_entry, _resource_validation_entry(broken_entry, checks=checks)


def _validate_resource_candidate(
    entry: ModelEntry,
    *,
    url: str,
    source: ResourceValidationSource,
    timeout: float,
    client: httpx.Client,
) -> ResourceCandidateValidation:
    if entry.type == 'live2d':
        return _validate_live2d_resource_candidate(url=url, source=source, timeout=timeout, client=client)
    return _validate_spine_resource_candidate(url=url, source=source, timeout=timeout, client=client)


def _validated_entry_result(
    entry: ModelEntry,
    *,
    validation: ResourceCandidateValidation,
    all_checks: tuple[ResourceCheck, ...],
    state: AvailabilityState,
    checked_at: str,
) -> tuple[ModelEntry, ResourceValidationEntry]:
    resources = replace(entry.resources, display_info_url=validation.display_info_url)
    availability = ModelAvailability(state=state, validated_url=validation.url, checked_at=checked_at)
    updated_entry = replace(entry, resources=resources, resource_summary=validation.resource_summary, availability=availability)
    return updated_entry, _resource_validation_entry(updated_entry, checks=all_checks)


def _unchecked_catalog_entry(entry: ModelEntry, *, checked_at: str) -> ModelEntry:
    return replace(
        entry,
        availability=ModelAvailability(state='unchecked', checked_at=checked_at, message='Entry was not selected for validation'),
    )


def _resource_validation_entry(entry: ModelEntry, *, checks: tuple[ResourceCheck, ...]) -> ResourceValidationEntry:
    return ResourceValidationEntry(
        entry_id=entry.id,
        entry_type=entry.type,
        availability=entry.availability,
        resources=entry.resources,
        resource_summary=entry.resource_summary,
        checks=checks,
    )


def _validate_live2d_resource_candidate(
    *,
    url: str,
    source: ResourceValidationSource,
    timeout: float,
    client: httpx.Client,
) -> ResourceCandidateValidation:
    context = ResourceRequestContext(timeout=timeout, client=client)
    response, model3_check = _request_resource(method='GET', url=url, kind='live2d.model3', source=source, context=context)
    if not model3_check.ok or response is None:
        return _failed_candidate(url=url, check=model3_check)

    try:
        manifest = _parse_live2d_model3(response.text, model3_url=url)
    except SourceParseError as exc:
        failed_check = replace(model3_check, ok=False, status='parse', message=str(exc))
        return _failed_candidate(url=url, check=failed_check)
    except SourceSchemaError as exc:
        failed_check = replace(model3_check, ok=False, status='schema', message=str(exc))
        return _failed_candidate(url=url, check=failed_check)

    checks = [model3_check]
    checks.append(_probe_resource(url=manifest.moc3_url, kind='live2d.moc3', source=source, context=context))
    checks.extend(
        _probe_resource(url=texture_url, kind='live2d.texture', source=source, context=context) for texture_url in manifest.texture_urls
    )

    display_info_url = _validated_display_info_url(
        manifest=manifest,
        model3_url=url,
        source=source,
        context=context,
        checks=checks,
    )
    required_checks_ok = all(check.ok for check in checks if check.kind in {'live2d.model3', 'live2d.moc3', 'live2d.texture'})
    resource_summary = ModelResourceSummary(
        moc3=manifest.moc3_url,
        textures=manifest.texture_urls,
        physics=manifest.physics_url,
        display_info=display_info_url or manifest.display_info_url,
        motions=manifest.motion_names,
        expressions=manifest.expression_names,
        has_audio=manifest.has_audio,
        has_text=manifest.has_text,
        has_display_info=bool(display_info_url),
    )
    if required_checks_ok:
        return ResourceCandidateValidation(
            ok=True,
            url=url,
            resource_summary=resource_summary,
            display_info_url=display_info_url,
            checks=tuple(checks),
        )

    message = _resource_failure_message(checks)
    return ResourceCandidateValidation(
        ok=False,
        url=url,
        resource_summary=resource_summary,
        display_info_url=display_info_url,
        checks=tuple(checks),
        message=message,
    )


def _validate_spine_resource_candidate(
    *,
    url: str,
    source: ResourceValidationSource,
    timeout: float,
    client: httpx.Client,
) -> ResourceCandidateValidation:
    context = ResourceRequestContext(timeout=timeout, client=client)
    manifest = _spine_resource_manifest(url)
    checks = [
        _probe_resource(url=manifest.skel_url, kind='spine.skel', source=source, context=context),
    ]

    response, atlas_check = _request_resource(
        method='GET',
        url=manifest.atlas_url,
        kind='spine.atlas',
        source=source,
        context=context,
    )
    checks.append(atlas_check)

    texture_urls = manifest.texture_urls
    if atlas_check.ok and response is not None:
        try:
            texture_urls = _parse_spine_atlas_texture_urls(response.text, atlas_url=manifest.atlas_url)
        except SourceParseError as exc:
            checks[-1] = replace(atlas_check, ok=False, status='parse', message=str(exc))
            texture_urls = ()
        except SourceSchemaError as exc:
            checks[-1] = replace(atlas_check, ok=False, status='schema', message=str(exc))
            texture_urls = ()

    checks.extend(_probe_resource(url=texture_url, kind='spine.texture', source=source, context=context) for texture_url in texture_urls)
    resource_summary = ModelResourceSummary(textures=texture_urls)
    if all(check.ok for check in checks):
        return ResourceCandidateValidation(ok=True, url=url, resource_summary=resource_summary, display_info_url='', checks=tuple(checks))

    return ResourceCandidateValidation(
        ok=False,
        url=url,
        resource_summary=resource_summary,
        display_info_url='',
        checks=tuple(checks),
        message=_resource_failure_message(checks),
    )


def _live2d_resource_specs(entry: ModelEntry, *, model3_source: str) -> tuple[_ResourceSpec, ...]:
    manifest = _parse_live2d_model3(model3_source, model3_url=entry.resources.primary_url)
    specs = [
        _ResourceSpec(
            kind='live2d.model3',
            source_url=entry.resources.primary_url,
            fallback_url=entry.resources.fallback_url,
            base_url=entry.resources.primary_url,
            context=_asset_context(entry, {'live2d_field': 'model3'}),
        ),
    ]
    specs.extend(
        _ResourceSpec(
            kind=reference.kind,
            source_url=reference.url,
            fallback_url=_fallback_live2d_resource_url(entry.resources.fallback_url, reference.raw_path),
            base_url=entry.resources.primary_url,
            context=_asset_context(entry, reference.context),
        )
        for reference in manifest.references
    )
    if entry.resources.display_info_url and not any(spec.kind == 'live2d.display-info' for spec in specs):
        specs.append(
            _ResourceSpec(
                kind='live2d.display-info',
                source_url=entry.resources.display_info_url,
                fallback_url=_same_name_display_info_url(entry.resources.fallback_url),
                base_url=entry.resources.primary_url,
                context=_asset_context(entry, {'live2d_field': 'display_info', 'inferred': True}),
            ),
        )
    return tuple(specs)


def _spine_resource_specs(entry: ModelEntry, *, atlas_source: str | None) -> tuple[_ResourceSpec, ...]:
    manifest = _spine_resource_manifest(entry.resources.primary_url)
    fallback_manifest = _spine_resource_manifest(entry.resources.fallback_url) if entry.resources.fallback_url else None
    specs = [
        _ResourceSpec(
            kind='spine.skel',
            source_url=manifest.skel_url,
            fallback_url=fallback_manifest.skel_url if fallback_manifest is not None else '',
            base_url=manifest.skel_url,
            context=_asset_context(entry, {'spine_field': 'skel'}),
        ),
        _ResourceSpec(
            kind='spine.atlas',
            source_url=manifest.atlas_url,
            fallback_url=fallback_manifest.atlas_url if fallback_manifest is not None else '',
            base_url=manifest.atlas_url,
            context=_asset_context(entry, {'spine_field': 'atlas'}),
        ),
    ]

    if atlas_source is None:
        return tuple(specs)

    for page_index, texture_path in enumerate(_parse_spine_atlas_texture_paths(atlas_source)):
        specs.append(
            _ResourceSpec(
                kind='spine.texture',
                source_url=_resolve_resource_url(manifest.atlas_url, texture_path),
                fallback_url=_resolve_resource_url(fallback_manifest.atlas_url, texture_path) if fallback_manifest is not None else '',
                base_url=manifest.atlas_url,
                context=_asset_context(
                    entry,
                    {
                        'spine_field': 'texture',
                        'atlas_page': texture_path,
                        'page_index': page_index,
                    },
                ),
            ),
        )
    return tuple(specs)


def _enumerated_resources_from_specs(entry: ModelEntry, specs: tuple[_ResourceSpec, ...]) -> tuple[AzurLaneEnumeratedResource, ...]:
    used_paths: dict[str, str] = {}
    resources: list[AzurLaneEnumeratedResource] = []
    for spec in specs:
        original_filename = _resource_filename(spec.source_url, spec.kind)
        local_path = _resource_local_path(entry, spec, original_filename=original_filename, used_paths=used_paths)
        resources.append(
            AzurLaneEnumeratedResource(
                model_id=entry.id,
                model_type=entry.type,
                character_key=entry.character.key,
                costume_key=entry.costume.key,
                kind=spec.kind,
                source_url=spec.source_url,
                fallback_url=spec.fallback_url,
                local_path=local_path,
                original_filename=original_filename,
                contexts=(spec.context,),
            ),
        )
    return tuple(resources)


def _asset_context(entry: ModelEntry, metadata: dict[str, Any]) -> dict[str, Any]:
    return {
        'model_id': entry.id,
        'model_type': entry.type,
        'character_key': entry.character.key,
        'character_id': entry.character.id,
        'character_name_zh': entry.character.name_zh,
        'character_name_en': entry.character.name_en,
        'costume_key': entry.costume.key,
        'costume_id': entry.costume.id,
        'costume_name_zh': entry.costume.name_zh,
        'costume_name_en': entry.costume.name_en,
        'catalog_source': entry.source,
        'source_model_url': entry.resources.primary_url,
        'fallback_model_url': entry.resources.fallback_url,
        **metadata,
    }


def _fallback_live2d_resource_url(fallback_model3_url: str, raw_path: str) -> str:
    if not fallback_model3_url:
        return ''
    return _resolve_resource_url(fallback_model3_url, raw_path)


def _resource_local_path(
    entry: ModelEntry,
    spec: _ResourceSpec,
    *,
    original_filename: str,
    used_paths: dict[str, str],
) -> str:
    resource_dir = _model_resource_directory(entry)
    relative_path = _relative_resource_path(spec.source_url, base_url=spec.base_url, fallback_filename=original_filename)
    local_path = (resource_dir / relative_path).as_posix()
    collision_key = f'{spec.kind}:{spec.source_url}'
    if local_path in used_paths and used_paths[local_path] != collision_key:
        local_path = _disambiguated_local_path(local_path, collision_key)
    while local_path in used_paths and used_paths[local_path] != collision_key:
        collision_key = f'{collision_key}:{len(used_paths)}'
        local_path = _disambiguated_local_path(local_path, collision_key)
    used_paths[local_path] = collision_key
    return local_path


def _model_resource_directory(entry: ModelEntry) -> PurePosixPath:
    model_key = _safe_path_segment(entry.costume.key, fallback='unknown')
    canonical_id = _catalog_entry_id(entry.type, entry.character.key, entry.costume.key)
    if entry.id != canonical_id:
        token = blake2b(entry.id.encode(), digest_size=4).hexdigest()
        model_key = _safe_path_segment(f'{model_key}-{token}', fallback=model_key)
    return PurePosixPath('assets', entry.type, model_key)


def _relative_resource_path(source_url: str, *, base_url: str, fallback_filename: str) -> PurePosixPath:
    source_path = PurePosixPath(urlsplit(source_url).path)
    base_path = PurePosixPath(urlsplit(base_url).path)
    base_dir = base_path.parent if base_path.name else base_path
    try:
        relative = source_path.relative_to(base_dir)
    except ValueError:
        relative = PurePosixPath(fallback_filename)
    segments = [_safe_path_segment(segment, fallback='asset') for segment in relative.parts if segment not in {'', '/', '.', '..'}]
    if not segments:
        segments = [_safe_path_segment(fallback_filename, fallback='asset')]
    return PurePosixPath(*segments)


def _resource_filename(url: str, kind: ResourceAssetKind) -> str:
    filename = PurePosixPath(urlsplit(url).path).name
    if filename:
        return _safe_path_segment(filename, fallback=f'asset{_RESOURCE_EXTENSION_BY_KIND[kind]}')
    return f'asset{_RESOURCE_EXTENSION_BY_KIND[kind]}'


def _safe_path_segment(value: str, *, fallback: str) -> str:
    sanitized = sanitize(value, max_bytes=120)
    return sanitized or fallback


def _disambiguated_local_path(local_path: str, value: str) -> str:
    path = PurePosixPath(local_path)
    digest = blake2b(value.encode(), digest_size=4).hexdigest()
    return path.with_stem(f'{path.stem}-{digest}').as_posix()


def _failed_candidate(*, url: str, check: ResourceCheck) -> ResourceCandidateValidation:
    return ResourceCandidateValidation(
        ok=False,
        url=url,
        resource_summary=ModelResourceSummary(),
        display_info_url='',
        checks=(check,),
        message=check.message,
    )


def _validated_display_info_url(
    *,
    manifest: Live2DResourceManifest,
    model3_url: str,
    source: ResourceValidationSource,
    context: ResourceRequestContext,
    checks: list[ResourceCheck],
) -> str:
    display_info_url = manifest.display_info_url or _same_name_display_info_url(model3_url)
    if not display_info_url:
        return ''

    check = _probe_resource(url=display_info_url, kind='live2d.display-info', source=source, context=context)
    checks.append(check)
    return display_info_url if check.ok else ''


def _request_resource(
    *,
    method: str,
    url: str,
    kind: ResourceAssetKind,
    source: ResourceValidationSource,
    context: ResourceRequestContext,
) -> tuple[httpx.Response | None, ResourceCheck]:
    try:
        response = context.client.request(method, url, timeout=context.timeout)
    except httpx.HTTPError as exc:
        message = f'Failed to fetch {url}: {exc}'
        return None, ResourceCheck(kind=kind, url=url, ok=False, status='network', message=message, source=source)

    if response.status_code >= HTTP_ERROR_MIN:
        message = f'HTTP {response.status_code} while fetching {url}'
        return response, ResourceCheck(
            kind=kind,
            url=url,
            ok=False,
            status='missing',
            http_status=response.status_code,
            message=message,
            source=source,
        )

    return response, ResourceCheck(kind=kind, url=str(response.url), ok=True, status='ok', http_status=response.status_code, source=source)


def _probe_resource(
    *,
    url: str,
    kind: ResourceAssetKind,
    source: ResourceValidationSource,
    context: ResourceRequestContext,
) -> ResourceCheck:
    response, check = _request_resource(method='HEAD', url=url, kind=kind, source=source, context=context)
    if check.ok or response is None or response.status_code not in {405, 501}:
        return check

    _response, fallback_check = _request_resource(method='GET', url=url, kind=kind, source=source, context=context)
    return fallback_check


def _parse_live2d_model3(source: str, *, model3_url: str) -> Live2DResourceManifest:
    try:
        payload = json.loads(source)
    except json.JSONDecodeError as exc:
        msg = f'Live2D model3 JSON is invalid: {exc.msg}'
        raise SourceParseError(msg) from exc

    if not isinstance(payload, dict):
        msg = 'Live2D model3 root must be an object'
        raise SourceSchemaError(msg)
    file_references = payload.get('FileReferences')
    if not isinstance(file_references, dict):
        msg = 'Live2D model3 FileReferences must be an object'
        raise SourceSchemaError(msg)

    moc3_reference = _required_live2d_reference(
        file_references,
        'Moc',
        spec=_Live2DReferenceSpec(kind='live2d.moc3', base_url=model3_url, metadata={'live2d_field': 'moc3'}),
        context='Live2D model3 FileReferences',
    )
    texture_references = _required_live2d_reference_list(
        file_references,
        'Textures',
        spec=_Live2DReferenceSpec(
            kind='live2d.texture',
            base_url=model3_url,
            metadata={'live2d_field': 'texture'},
            metadata_key='texture_index',
        ),
        context='Live2D model3 FileReferences',
    )
    physics_reference = _optional_live2d_reference(
        file_references,
        'Physics',
        kind='live2d.physics',
        base_url=model3_url,
        metadata={'live2d_field': 'physics'},
    )
    pose_reference = _optional_live2d_reference(
        file_references,
        'Pose',
        kind='live2d.pose',
        base_url=model3_url,
        metadata={'live2d_field': 'pose'},
    )
    display_info_reference = _optional_live2d_reference(
        file_references,
        'DisplayInfo',
        kind='live2d.display-info',
        base_url=model3_url,
        metadata={'live2d_field': 'display_info'},
    )
    expression_references = _parse_live2d_expression_references(file_references.get('Expressions'), base_url=model3_url)
    motion_references, audio_references, text_references, motion_names, has_audio, has_text = _parse_live2d_motion_references(
        file_references.get('Motions'),
        base_url=model3_url,
    )

    return Live2DResourceManifest(
        moc3_url=moc3_reference.url,
        texture_urls=tuple(reference.url for reference in texture_references),
        physics_url=physics_reference.url if physics_reference is not None else '',
        pose_url=pose_reference.url if pose_reference is not None else '',
        display_info_url=display_info_reference.url if display_info_reference is not None else '',
        expression_urls=tuple(reference.url for reference in expression_references),
        motion_urls=tuple(reference.url for reference in motion_references),
        audio_urls=tuple(reference.url for reference in audio_references),
        text_urls=tuple(reference.url for reference in text_references),
        motion_names=motion_names,
        expression_names=tuple(_context_text(reference.context, 'expression_name') for reference in expression_references),
        has_audio=has_audio,
        has_text=has_text,
        references=tuple(
            reference
            for reference in (
                moc3_reference,
                *texture_references,
                physics_reference,
                pose_reference,
                display_info_reference,
                *expression_references,
                *motion_references,
                *audio_references,
                *text_references,
            )
            if reference is not None
        ),
    )


def _required_live2d_reference(
    item: dict[str, Any],
    field_name: str,
    *,
    spec: _Live2DReferenceSpec,
    context: str,
) -> ResourceReference:
    raw_path = _optional_resource_reference_path(item, field_name)
    if raw_path:
        return _resource_reference(kind=spec.kind, raw_path=raw_path, base_url=spec.base_url, metadata=spec.metadata)
    msg = f'{context}.{field_name} must be a non-empty string'
    raise SourceSchemaError(msg)


def _required_live2d_reference_list(
    item: dict[str, Any],
    field_name: str,
    *,
    spec: _Live2DReferenceSpec,
    context: str,
) -> tuple[ResourceReference, ...]:
    values = item.get(field_name)
    if not isinstance(values, list):
        msg = f'{context}.{field_name} must be a list'
        raise SourceSchemaError(msg)

    references = tuple(
        _resource_reference(
            kind=spec.kind,
            raw_path=value,
            base_url=spec.base_url,
            metadata={**spec.metadata, spec.metadata_key: index},
        )
        for index, value in enumerate(values)
        if isinstance(value, str) and value.strip()
    )
    if references:
        return references
    msg = f'{context}.{field_name} must contain at least one non-empty string'
    raise SourceSchemaError(msg)


def _optional_live2d_reference(
    item: dict[str, Any],
    field_name: str,
    *,
    kind: ResourceAssetKind,
    base_url: str,
    metadata: dict[str, Any],
) -> ResourceReference | None:
    raw_path = _optional_resource_reference_path(item, field_name)
    if not raw_path:
        return None
    return _resource_reference(kind=kind, raw_path=raw_path, base_url=base_url, metadata=metadata)


def _optional_resource_reference_path(item: dict[str, Any], field_name: str) -> str:
    value = item.get(field_name)
    if not isinstance(value, str) or not value.strip():
        return ''
    return value.strip()


def _resource_reference(*, kind: ResourceAssetKind, raw_path: str, base_url: str, metadata: dict[str, Any]) -> ResourceReference:
    return ResourceReference(kind=kind, raw_path=raw_path.strip(), url=_resolve_resource_url(base_url, raw_path), context=metadata)


def _parse_live2d_expression_references(value: Any, *, base_url: str) -> tuple[ResourceReference, ...]:
    if not isinstance(value, list):
        return ()

    references: list[ResourceReference] = []
    for index, expression in enumerate(value):
        if not isinstance(expression, dict):
            continue
        raw_path = _optional_resource_reference_path(expression, 'File')
        if not raw_path:
            continue
        name = expression.get('Name')
        expression_name = name.strip() if isinstance(name, str) and name.strip() else _resource_basename(raw_path)
        references.append(
            _resource_reference(
                kind='live2d.expression',
                raw_path=raw_path,
                base_url=base_url,
                metadata={
                    'live2d_field': 'expression',
                    'expression_index': index,
                    'expression_name': expression_name,
                },
            ),
        )
    return tuple(references)


def _parse_live2d_motion_references(
    value: Any,
    *,
    base_url: str,
) -> tuple[tuple[ResourceReference, ...], tuple[ResourceReference, ...], tuple[ResourceReference, ...], tuple[str, ...], bool, bool]:
    if not isinstance(value, dict):
        return (), (), (), (), False, False

    names: list[str] = []
    has_audio = False
    has_text = False
    motion_references: list[ResourceReference] = []
    audio_references: list[ResourceReference] = []
    text_references: list[ResourceReference] = []
    for group_name, motions in value.items():
        normalized_group = group_name.strip() if isinstance(group_name, str) else ''
        if normalized_group:
            names.append(normalized_group)
        if not isinstance(motions, list):
            continue
        for motion_index, motion in enumerate(motions):
            parsed = _parse_live2d_motion_item(motion, base_url=base_url, group=normalized_group, index=motion_index)
            if parsed.name and not normalized_group:
                names.append(parsed.name)
            if parsed.motion is not None:
                motion_references.append(parsed.motion)
            if parsed.audio is not None:
                has_audio = True
                audio_references.append(parsed.audio)
            if parsed.text is not None:
                has_text = True
                text_references.append(parsed.text)

    return tuple(motion_references), tuple(audio_references), tuple(text_references), _unique_non_empty(names), has_audio, has_text


def _parse_live2d_motion_item(motion: Any, *, base_url: str, group: str, index: int) -> _Live2DMotionReferences:
    if not isinstance(motion, dict):
        return _Live2DMotionReferences()

    context = {
        'live2d_field': 'motion',
        'motion_group': group,
        'motion_index': index,
    }
    motion_file = _optional_resource_reference_path(motion, 'File')
    sound_file = _optional_resource_reference_path(motion, 'Sound')
    text_file = _optional_resource_reference_path(motion, 'Text')
    return _Live2DMotionReferences(
        motion=_optional_motion_reference(kind='live2d.motion', raw_path=motion_file, base_url=base_url, metadata=context),
        audio=_optional_motion_reference(
            kind='live2d.audio',
            raw_path=sound_file,
            base_url=base_url,
            metadata={**context, 'live2d_field': 'audio'},
        ),
        text=_optional_motion_reference(
            kind='live2d.text',
            raw_path=text_file,
            base_url=base_url,
            metadata={**context, 'live2d_field': 'text'},
        ),
        name=_resource_basename(motion_file),
    )


def _optional_motion_reference(
    *,
    kind: ResourceAssetKind,
    raw_path: str,
    base_url: str,
    metadata: dict[str, Any],
) -> ResourceReference | None:
    if not raw_path:
        return None
    return _resource_reference(kind=kind, raw_path=raw_path, base_url=base_url, metadata=metadata)


def _context_text(context: dict[str, Any], key: str) -> str:
    value = context.get(key)
    return value if isinstance(value, str) else ''


def _spine_resource_manifest(url: str) -> SpineResourceManifest:
    base_url = url.rstrip('/')
    path_name = PurePosixPath(urlsplit(base_url).path).name
    if base_url.endswith('.skel'):
        stem_url = base_url.removesuffix('.skel')
    elif base_url.endswith('.atlas'):
        stem_url = base_url.removesuffix('.atlas')
    else:
        file_stem = path_name.removesuffix('-spine') if path_name.endswith('-spine') else path_name
        stem_url = f'{base_url}/{file_stem}'
    return SpineResourceManifest(skel_url=f'{stem_url}.skel', atlas_url=f'{stem_url}.atlas', texture_urls=())


def _parse_spine_atlas_texture_urls(source: str, *, atlas_url: str) -> tuple[str, ...]:
    texture_paths = _parse_spine_atlas_texture_paths(source)
    return tuple(_resolve_resource_url(atlas_url, texture_path) for texture_path in texture_paths)


def _parse_spine_atlas_texture_paths(source: str) -> tuple[str, ...]:
    if not source.strip():
        msg = 'Spine atlas is empty'
        raise SourceParseError(msg)

    texture_paths: list[str] = []
    next_non_empty_line_is_page = True
    for raw_line in source.splitlines():
        line = raw_line.strip()
        if not line:
            next_non_empty_line_is_page = True
            continue
        if raw_line[:1].isspace() or ':' in line:
            continue
        if next_non_empty_line_is_page:
            texture_paths.append(line)
            next_non_empty_line_is_page = False

    texture_paths = list(_unique_non_empty(texture_paths))
    if texture_paths:
        return tuple(texture_paths)

    msg = 'Spine atlas does not contain a texture page'
    raise SourceSchemaError(msg)


def _resolve_resource_url(base_url: str, value: str) -> str:
    return urljoin(base_url, value.strip())


def _same_name_display_info_url(model3_url: str) -> str:
    if not model3_url.endswith('.model3.json'):
        return ''
    return f'{model3_url.removesuffix(".model3.json")}.cdi3.json'


def _resource_basename(value: Any) -> str:
    if not isinstance(value, str) or not value.strip():
        return ''
    return PurePosixPath(urlsplit(value).path or value).name


def _has_non_empty_str(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def _unique_non_empty(values: Iterable[str]) -> tuple[str, ...]:
    return tuple(dict.fromkeys(value for value in values if value))


def _resource_failure_message(checks: Iterable[ResourceCheck]) -> str:
    failures = [f'{check.kind} {check.status}: {check.url}' for check in checks if not check.ok]
    return '; '.join(failures)


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

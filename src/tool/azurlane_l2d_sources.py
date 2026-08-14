from __future__ import annotations

import json
import re
import unicodedata
from dataclasses import asdict, dataclass, field, replace
from datetime import UTC, datetime
from hashlib import blake2b
from pathlib import PurePosixPath
from typing import TYPE_CHECKING, Any, Literal
from urllib.parse import urljoin, urlsplit, urlunsplit

import httpx

from src.tool.filename import sanitize

if TYPE_CHECKING:
    from collections.abc import Callable, Iterable, Mapping

L2D_SU_SHIP_INDEX_URL_TEMPLATE = 'https://l2d.su/data/ships-{region}.json'
L2D_SU_SHIP_DETAIL_URL_TEMPLATE = 'https://l2d.su/data/ships/{region}/{ship_id}.json'
L2D_SU_STATIC_BASE_URL = 'https://static.l2d.su/azurlane'
L2D_SU_REFERER = 'https://l2d.su/'
L2D_SU_PRIMARY_REGION = 'CN'
L2D_SU_ENGLISH_REGION = 'EN'
NAGAMI_MAPPING_URL = 'https://data.nagami.moe/live2d/l2d_mapping.json'
NAGAMI_LIVE2D_BASE_URL = 'https://cdn.nagami.moe/live2d'
DEFAULT_USER_AGENT = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36'
HTTP_ERROR_MIN = 400
CATALOG_ENTRY_ID_PREFIX = 'azurlane'
CATALOG_VARIANT_TOKEN_SIZE = 6
SPINE_PARTS_MANIFEST_EXTENSION = '.json'

SourceErrorKind = Literal['network', 'parse', 'schema']
L2DSuModelKind = Literal['live2d', 'spine', 'painting']
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
    'spine.parts',
    'spine.skel',
    'spine.atlas',
    'spine.texture',
    'painting.image',
    'painting.face',
    'icon.square',
    'icon.shipyard',
    'voice.audio',
]

_MODEL_TYPES: tuple[L2DSuModelKind, ...] = ('live2d', 'spine', 'painting')
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
    'spine.parts': '.json',
    'spine.skel': '.skel',
    'spine.atlas': '.atlas',
    'spine.texture': '.webp',
    'painting.image': '.webp',
    'painting.face': '.webp',
    'icon.square': '.webp',
    'icon.shipyard': '.webp',
    'voice.audio': '.ogg',
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
    model_key: str = ''
    skin_type: str = ''
    feature_tags: tuple[str, ...] = ()
    is_live2d_plus: bool = False
    face_ids: tuple[str, ...] = ()
    square_icon: str = ''
    shipyard_icon: str = ''
    q_icon: str = ''
    shop_type: str = ''


@dataclass(frozen=True, slots=True)
class L2DSuCharacterSnapshot:
    char_id: int
    char_key: str
    char_name: str
    char_name_en: str
    nation: str = ''
    ship_type: str = ''
    rarity: str = ''
    default_skin_id: int | None = None
    skin_series: tuple[str, ...] = ()
    live2d: tuple[L2DSuModelSnapshot, ...] = ()
    spine: tuple[L2DSuModelSnapshot, ...] = ()
    paintings: tuple[L2DSuModelSnapshot, ...] = ()


@dataclass(frozen=True, slots=True)
class L2DSuVoiceLine:
    key: str
    voice_name: str
    resource_key: str
    voice_path: str
    text: str
    face_id: str = ''
    l2d_action: str = ''
    spine_action: str = ''
    is_extra: bool = False


@dataclass(frozen=True, slots=True)
class L2DSuNewSkin:
    ship_group_id: int
    ship_name: str
    skin_name: str
    skin_type: str
    skin_ids: tuple[int, ...] = ()


@dataclass(frozen=True, slots=True)
class L2DSuIndexData:
    region: str
    game_version: str
    generated_at: str
    ship_count: int
    characters: tuple[L2DSuCharacterSnapshot, ...]
    new_skins: tuple[L2DSuNewSkin, ...] = ()
    filters: dict[str, Any] = field(default_factory=dict)
    skin_update_history: tuple[dict[str, Any], ...] = ()

    def summary(self) -> dict[str, int]:
        return {
            'character_count': len(self.characters),
            'live2d_count': sum(len(character.live2d) for character in self.characters),
            'spine_count': sum(len(character.spine) for character in self.characters),
            'painting_count': sum(len(character.paintings) for character in self.characters),
        }


@dataclass(frozen=True, slots=True)
class L2DSuSourceSnapshot:
    metadata: SourceFetchMetadata
    region: str = ''
    game_version: str = ''
    generated_at: str = ''
    ship_count: int = 0
    characters: tuple[L2DSuCharacterSnapshot, ...] = ()
    new_skins: tuple[L2DSuNewSkin, ...] = ()
    filters: dict[str, Any] = field(default_factory=dict)
    skin_update_history: tuple[dict[str, Any], ...] = ()
    errors: tuple[SourceSnapshotError, ...] = ()
    source: Literal['l2d.su'] = 'l2d.su'

    def summary(self) -> dict[str, int]:
        return {
            'character_count': len(self.characters),
            'live2d_count': sum(len(character.live2d) for character in self.characters),
            'spine_count': sum(len(character.spine) for character in self.characters),
            'painting_count': sum(len(character.paintings) for character in self.characters),
            'new_skin_count': len(self.new_skins),
            'error_count': len(self.errors),
        }

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class NagamiMappingEntry:
    key: str
    name: str
    background: str = ''


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
    nation: str = ''
    ship_type: str = ''
    rarity: str = ''
    class_name: str = ''
    default_skin_id: int | None = None
    skin_series: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class ModelCostume:
    key: str
    id: int | None = None
    name_zh: str = ''
    name_en: str = ''
    skin_type: str = ''
    feature_tags: tuple[str, ...] = ()
    is_live2d_plus: bool = False
    face_ids: tuple[str, ...] = ()
    square_icon: str = ''
    shipyard_icon: str = ''
    q_icon: str = ''
    shop_type: str = ''


@dataclass(frozen=True, slots=True)
class ModelResources:
    primary_url: str
    fallback_url: str = ''
    display_info_url: str = ''


@dataclass(frozen=True, slots=True)
class ModelResourceSummary:
    moc3: str = ''
    skeleton: str = ''
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
    motion_names: tuple[str, ...] = ()
    expression_names: tuple[str, ...] = ()
    has_audio: bool = False
    has_text: bool = False
    references: tuple[ResourceReference, ...] = ()


@dataclass(frozen=True, slots=True)
class SpineModelPart:
    name: str
    skeleton_url: str
    atlas_urls: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class SpineResourceManifest:
    parts: tuple[SpineModelPart, ...]
    parts_manifest_url: str = ''


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
    text: str = ''
    name: str = ''


def l2d_su_ship_index_url(region: str = L2D_SU_PRIMARY_REGION) -> str:
    return L2D_SU_SHIP_INDEX_URL_TEMPLATE.format(region=region)


def l2d_su_ship_detail_url(ship_id: int, region: str = L2D_SU_PRIMARY_REGION) -> str:
    return L2D_SU_SHIP_DETAIL_URL_TEMPLATE.format(region=region, ship_id=ship_id)


def parse_l2d_su_ship_models(source: str) -> dict[int, str]:
    """Map costume_id to the authoritative model URL from a per-ship detail payload."""
    payload = _decode_json_object(source, context='l2d.su ship detail')
    ship = payload.get('ship')
    if not isinstance(ship, dict):
        msg = 'l2d.su ship detail must contain a ship object'
        raise SourceSchemaError(msg)

    skins = ship.get('skins')
    if not isinstance(skins, list):
        msg = 'l2d.su ship detail must contain a skins list'
        raise SourceSchemaError(msg)

    models: dict[int, str] = {}
    for skin in skins:
        if not isinstance(skin, dict):
            continue
        model = skin.get('model')
        costume_id = skin.get('id')
        if not isinstance(model, dict) or not isinstance(costume_id, int) or isinstance(costume_id, bool):
            continue
        path = _optional_str(model, 'path')
        if path:
            models[costume_id] = _l2d_su_static_url(path)
    return models


def parse_l2d_su_ship_voices(source: str) -> dict[int, tuple[L2DSuVoiceLine, ...]]:
    """Map costume_id to its voice lines from a per-ship detail payload."""
    payload = _decode_json_object(source, context='l2d.su ship detail')
    ship = payload.get('ship')
    if not isinstance(ship, dict):
        msg = 'l2d.su ship detail must contain a ship object'
        raise SourceSchemaError(msg)
    skins = ship.get('skins')
    if not isinstance(skins, list):
        msg = 'l2d.su ship detail must contain a skins list'
        raise SourceSchemaError(msg)

    voices: dict[int, tuple[L2DSuVoiceLine, ...]] = {}
    for skin in skins:
        if not isinstance(skin, dict):
            continue
        costume_id = skin.get('id')
        if not isinstance(costume_id, int) or isinstance(costume_id, bool):
            continue
        lines = (
            *_parse_l2d_su_voice_lines(skin.get('words'), is_extra=False),
            *_parse_l2d_su_voice_lines(skin.get('extraWords'), is_extra=True),
        )
        if lines:
            voices[costume_id] = lines
    return voices


def _parse_l2d_su_voice_lines(value: Any, *, is_extra: bool) -> tuple[L2DSuVoiceLine, ...]:
    if not isinstance(value, list):
        return ()

    lines: list[L2DSuVoiceLine] = []
    for item in value:
        if not isinstance(item, dict):
            continue
        voice_path = _optional_str(item, 'voicePath')
        if not voice_path:
            continue
        lines.append(
            L2DSuVoiceLine(
                key=_optional_str(item, 'key'),
                voice_name=_optional_str(item, 'voiceName'),
                resource_key=_optional_str(item, 'resourceKey'),
                voice_path=voice_path,
                text=_optional_str(item, 'text'),
                face_id=_optional_str(item, 'faceId'),
                l2d_action=_optional_str(item, 'l2dAction'),
                spine_action=_optional_str(item, 'spineAction'),
                is_extra=is_extra,
            ),
        )
    return tuple(lines)


def l2d_su_character_fingerprint(character: L2DSuCharacterSnapshot) -> str:
    """Stable digest of a ship's index entry, used to detect when its detail payload is stale."""
    models = sorted(
        (*character.live2d, *character.spine, *character.paintings),
        key=lambda model: (model.kind, model.costume_id, model.model_key),
    )
    parts = [f'{character.char_id}:{character.char_key}:{character.char_name}']
    parts.extend(f'{model.kind}:{model.costume_id}:{model.model_key}:{model.costume_name}:{",".join(model.face_ids)}' for model in models)
    return blake2b('|'.join(parts).encode(), digest_size=16).hexdigest()


def model_probe_urls(entry: ModelEntry) -> tuple[str, ...]:
    """URLs that prove a model's assets exist, cheapest first."""
    if entry.type in {'live2d', 'painting'}:
        return (entry.resources.primary_url,)
    manifest = spine_resource_manifest(entry.resources.primary_url)
    return (manifest.parts[0].skeleton_url, spine_parts_manifest_url(entry.resources.primary_url))


@dataclass(frozen=True, slots=True)
class L2DSuOriginProbe:
    """Outcome of a connectivity check against the l2d.su origin."""

    ok: bool
    code: str
    message: str
    exit_ip: str = ''


def probe_l2d_su_origin(
    proxy: str,
    *,
    timeout: float = 45.0,
    region: str = L2D_SU_PRIMARY_REGION,
    client: httpx.Client | None = None,
) -> L2DSuOriginProbe:
    """Check that the origin is reachable through ``proxy``, reporting why it is not if it is not.

    Uses a HEAD against the ship index: it exercises the same host, headers and TLS path the
    crawler uses, without pulling the payload. Connection reuse is disabled to match the crawler,
    so this also exercises a freshly rotated exit.
    """
    if client is None and not proxy.strip():
        return L2DSuOriginProbe(ok=False, code='incomplete', message='No proxy configured.')

    url = l2d_su_ship_index_url(region)
    if client is not None:
        return _run_origin_probe(client, url=url)

    with httpx.Client(
        proxy=proxy.strip(),
        follow_redirects=True,
        timeout=timeout,
        headers=_request_headers(),
        limits=httpx.Limits(max_keepalive_connections=0),
    ) as owned_client:
        return _run_origin_probe(owned_client, url=url)


def _run_origin_probe(client: httpx.Client, *, url: str) -> L2DSuOriginProbe:
    try:
        exit_ip = _probe_exit_ip(client)
        response = client.head(url)
    except httpx.ProxyError as exc:
        return L2DSuOriginProbe(ok=False, code='proxy_error', message=f'Proxy refused the connection: {exc}')
    except httpx.HTTPError as exc:
        # The origin null-routes blocked addresses, so this is what a banned exit looks like.
        return L2DSuOriginProbe(ok=False, code='unreachable', message=f'Could not reach {url}: {exc}')

    if response.status_code >= HTTP_ERROR_MIN:
        return L2DSuOriginProbe(
            ok=False,
            code='http_error',
            message=f'{url} returned HTTP {response.status_code}.',
            exit_ip=exit_ip,
        )
    return L2DSuOriginProbe(ok=True, code='ok', message=f'Reached {url} through the proxy.', exit_ip=exit_ip)


def _probe_exit_ip(client: httpx.Client) -> str:
    """Best-effort exit address, shown so an operator can tell residential from datacenter."""
    try:
        response = client.get('https://checkip.amazonaws.com', timeout=20.0)
    except httpx.HTTPError:
        return ''
    return response.text.strip() if response.status_code < HTTP_ERROR_MIN else ''


def parse_l2d_su_ship_class(source: str) -> str:
    """Ship class name from a per-ship detail payload; the index does not carry it."""
    payload = _decode_json_object(source, context='l2d.su ship detail')
    ship = payload.get('ship')
    if not isinstance(ship, dict):
        msg = 'l2d.su ship detail must contain a ship object'
        raise SourceSchemaError(msg)
    return _optional_str(ship, 'className')


def apply_l2d_su_ship_classes(catalog: AzurLaneModelCatalog, classes: Mapping[int, str]) -> AzurLaneModelCatalog:
    """Attach ship class names, which are only available from the per-ship detail endpoint."""
    if not classes:
        return catalog

    entries = tuple(_entry_with_ship_class(entry, classes) for entry in catalog.entries)
    return replace(catalog, entries=entries)


def _entry_with_ship_class(entry: ModelEntry, classes: Mapping[int, str]) -> ModelEntry:
    if entry.character.id is None:
        return entry
    class_name = classes.get(entry.character.id)
    if not class_name or class_name == entry.character.class_name:
        return entry
    return replace(entry, character=replace(entry.character, class_name=class_name))


def apply_l2d_su_model_paths(catalog: AzurLaneModelCatalog, paths: Mapping[int, str]) -> AzurLaneModelCatalog:
    if not paths:
        return catalog

    entries = tuple(_entry_with_resolved_path(entry, paths) for entry in catalog.entries)
    return replace(catalog, entries=entries)


def _entry_with_resolved_path(entry: ModelEntry, paths: Mapping[int, str]) -> ModelEntry:
    if entry.type == 'painting' or entry.source == 'nagami' or entry.costume.id is None:
        return entry
    resolved = paths.get(entry.costume.id)
    if not resolved or resolved == entry.resources.primary_url:
        return entry
    return replace(entry, resources=replace(entry.resources, primary_url=resolved))


def _l2d_su_static_url(path: str) -> str:
    if path.startswith(('http://', 'https://')):
        return path
    return f'{L2D_SU_STATIC_BASE_URL}/{path.lstrip("/")}'


def parse_l2d_su_ship_index(source: str, *, region: str = '') -> L2DSuIndexData:
    payload = _decode_json_object(source, context='l2d.su ship index')
    ships = payload.get('ships')
    if not isinstance(ships, list):
        msg = 'l2d.su ship index must contain a ships list'
        raise SourceSchemaError(msg)

    filters = payload.get('filters')
    history = payload.get('skinUpdateHistory')
    return L2DSuIndexData(
        region=_optional_str(payload, 'locale') or region,
        game_version=_optional_str(payload, 'version'),
        generated_at=_optional_str(payload, 'generatedAt'),
        ship_count=len(ships),
        characters=tuple(_parse_l2d_su_ship(item, index=index) for index, item in enumerate(ships)),
        new_skins=_parse_l2d_su_new_skins(payload.get('newSkins')),
        filters=filters if isinstance(filters, dict) else {},
        skin_update_history=tuple(item for item in history if isinstance(item, dict)) if isinstance(history, list) else (),
    )


def merge_l2d_su_english_names(primary: L2DSuIndexData, english: L2DSuIndexData) -> L2DSuIndexData:
    english_by_char_id = {character.char_id: character for character in english.characters}
    characters = tuple(
        _character_with_english_names(character, english_by_char_id.get(character.char_id)) for character in primary.characters
    )
    return replace(primary, characters=characters)


def parse_nagami_mapping(source: str) -> NagamiMappingData:
    payload = _decode_json_object(source, context='Nagami mapping')
    entries: list[NagamiMappingEntry] = []
    for key, value in payload.items():
        if not isinstance(key, str) or not key.strip():
            msg = 'Nagami mapping keys must be non-empty strings'
            raise SourceSchemaError(msg)
        entries.append(_parse_nagami_mapping_entry(key, value))
    return NagamiMappingData(entries=tuple(sorted(entries, key=lambda entry: entry.key.casefold())))


def fetch_l2d_su_snapshot(
    *,
    region: str = L2D_SU_PRIMARY_REGION,
    english_region: str = L2D_SU_ENGLISH_REGION,
    timeout: float = 30.0,
    client: httpx.Client | None = None,
    origin_throttle: Callable[[], None] | None = None,
) -> L2DSuSourceSnapshot:
    parsed, metadata, errors = _fetch_l2d_su_index(region=region, timeout=timeout, client=client, origin_throttle=origin_throttle)
    if parsed is None:
        return L2DSuSourceSnapshot(metadata=metadata, region=region, errors=errors)

    if english_region and english_region != region:
        english, _english_metadata, english_errors = _fetch_l2d_su_index(
            region=english_region,
            timeout=timeout,
            client=client,
            origin_throttle=origin_throttle,
        )
        errors = (*errors, *english_errors)
        if english is not None:
            parsed = merge_l2d_su_english_names(parsed, english)

    return L2DSuSourceSnapshot(
        metadata=metadata,
        region=parsed.region,
        game_version=parsed.game_version,
        generated_at=parsed.generated_at,
        ship_count=parsed.ship_count,
        characters=parsed.characters,
        new_skins=parsed.new_skins,
        filters=parsed.filters,
        skin_update_history=parsed.skin_update_history,
        errors=errors,
    )


def fetch_nagami_snapshot(
    *,
    url: str = NAGAMI_MAPPING_URL,
    timeout: float = 30.0,
    client: httpx.Client | None = None,
) -> NagamiSourceSnapshot:
    response, metadata, error = _fetch_source(url=url, timeout=timeout, client=client)
    if error is not None:
        return NagamiSourceSnapshot(metadata=metadata, errors=(error,))
    if response is None:
        return NagamiSourceSnapshot(metadata=metadata, errors=(_source_error('network', 'Fetch did not return a response', metadata),))

    try:
        parsed = parse_nagami_mapping(response.text)
    except SourceParseError as exc:
        return NagamiSourceSnapshot(metadata=metadata, errors=(_source_error('parse', str(exc), metadata),))
    except SourceSchemaError as exc:
        return NagamiSourceSnapshot(metadata=metadata, errors=(_source_error('schema', str(exc), metadata),))

    return NagamiSourceSnapshot(metadata=metadata, entries=parsed.entries)


def fetch_source_snapshots(
    *,
    timeout: float = 30.0,
    client: httpx.Client | None = None,
    origin_client: httpx.Client | None = None,
    origin_throttle: Callable[[], None] | None = None,
) -> AzurLaneSourceSnapshots:
    """Fetch both sources. ``origin_client`` serves l2d.su only, so a proxy configured there
    never carries the CDN-hosted nagami mapping."""
    if client is not None:
        return AzurLaneSourceSnapshots(
            l2d_su=fetch_l2d_su_snapshot(timeout=timeout, client=origin_client or client, origin_throttle=origin_throttle),
            nagami=fetch_nagami_snapshot(timeout=timeout, client=client),
        )

    with httpx.Client(follow_redirects=True, timeout=timeout, headers=_request_headers()) as owned_client:
        return AzurLaneSourceSnapshots(
            l2d_su=fetch_l2d_su_snapshot(timeout=timeout, client=origin_client or owned_client, origin_throttle=origin_throttle),
            nagami=fetch_nagami_snapshot(timeout=timeout, client=owned_client),
        )


def build_azurlane_model_catalog(snapshots: AzurLaneSourceSnapshots) -> AzurLaneModelCatalog:
    nagami_candidates = build_nagami_fallback_candidates(snapshots.nagami)
    nagami_by_key = {candidate.key: candidate for candidate in nagami_candidates}
    matched_nagami_keys: set[str] = set()
    entries_by_id: dict[str, ModelEntry] = {}

    for character in sorted(snapshots.l2d_su.characters, key=lambda item: (_catalog_key(item.char_key), item.char_id)):
        l2d_su_models = (*character.live2d, *character.spine, *character.paintings)
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
    spine_parts_source: str | None = None,
    atlas_sources: Mapping[str, str] | None = None,
    voice_lines: tuple[L2DSuVoiceLine, ...] = (),
) -> AzurLaneModelResourceEnumeration:
    if entry.type == 'live2d':
        if model3_source is None:
            msg = f'Live2D entry {entry.id} requires model3_source for resource enumeration'
            raise SourceSchemaError(msg)
        specs = _live2d_resource_specs(entry, model3_source=model3_source)
    elif entry.type == 'painting':
        specs = _painting_resource_specs(entry, voice_lines=voice_lines)
    else:
        specs = _spine_resource_specs(entry, parts_source=spine_parts_source, atlas_sources=atlas_sources)

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
        entry_count=sum(len(character.live2d) + len(character.spine) + len(character.paintings) for character in snapshot.characters),
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
        for model in (*character.live2d, *character.spine, *character.paintings)
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
    if entry.type == 'painting':
        return _validate_painting_resource_candidate(url=url, source=source, timeout=timeout, client=client)
    return _validate_spine_resource_candidate(url=url, source=source, timeout=timeout, client=client)


def _validate_painting_resource_candidate(
    *,
    url: str,
    source: ResourceValidationSource,
    timeout: float,
    client: httpx.Client,
) -> ResourceCandidateValidation:
    context = ResourceRequestContext(timeout=timeout, client=client)
    check = _probe_resource(url=url, kind='painting.image', source=source, context=context)
    if check.ok:
        return ResourceCandidateValidation(
            ok=True,
            url=url,
            resource_summary=ModelResourceSummary(textures=(url,)),
            display_info_url='',
            checks=(check,),
        )
    return _failed_candidate(url=url, check=check)


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
    manifest, checks, checked_skeleton_urls = _resolve_spine_manifest_for_validation(url=url, source=source, context=context)

    texture_urls: list[str] = []
    for part in manifest.parts:
        if part.skeleton_url not in checked_skeleton_urls:
            checks.append(_probe_resource(url=part.skeleton_url, kind='spine.skel', source=source, context=context))
        for atlas_url in part.atlas_urls:
            atlas_check, part_texture_urls = _validate_spine_atlas(atlas_url=atlas_url, source=source, context=context)
            checks.append(atlas_check)
            texture_urls.extend(part_texture_urls)

    unique_texture_urls = _unique_non_empty(texture_urls)
    checks.extend(
        _probe_resource(url=texture_url, kind='spine.texture', source=source, context=context) for texture_url in unique_texture_urls
    )
    resource_summary = ModelResourceSummary(
        skeleton=manifest.parts[0].skeleton_url if manifest.parts else '',
        textures=unique_texture_urls,
    )
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


def _resolve_spine_manifest_for_validation(
    *,
    url: str,
    source: ResourceValidationSource,
    context: ResourceRequestContext,
) -> tuple[SpineResourceManifest, list[ResourceCheck], set[str]]:
    manifest = spine_resource_manifest(url)
    single_part_url = manifest.parts[0].skeleton_url
    single_part_check = _probe_resource(url=single_part_url, kind='spine.skel', source=source, context=context)
    if single_part_check.ok:
        return manifest, [single_part_check], {single_part_url}

    parts_url = spine_parts_manifest_url(url)
    response, parts_check = _request_resource(method='GET', url=parts_url, kind='spine.parts', source=source, context=context)
    if not parts_check.ok or response is None:
        return manifest, [single_part_check], {single_part_url}

    empty_manifest = replace(manifest, parts=())
    try:
        parts_manifest = spine_resource_manifest(url, parts_source=response.text)
    except SourceParseError as exc:
        return empty_manifest, [single_part_check, replace(parts_check, ok=False, status='parse', message=str(exc))], set()
    except SourceSchemaError as exc:
        return empty_manifest, [single_part_check, replace(parts_check, ok=False, status='schema', message=str(exc))], set()
    return parts_manifest, [parts_check], set()


def _validate_spine_atlas(
    *,
    atlas_url: str,
    source: ResourceValidationSource,
    context: ResourceRequestContext,
) -> tuple[ResourceCheck, tuple[str, ...]]:
    response, atlas_check = _request_resource(method='GET', url=atlas_url, kind='spine.atlas', source=source, context=context)
    if not atlas_check.ok or response is None:
        return atlas_check, ()

    try:
        return atlas_check, _parse_spine_atlas_texture_urls(response.text, atlas_url=atlas_url)
    except SourceParseError as exc:
        return replace(atlas_check, ok=False, status='parse', message=str(exc)), ()
    except SourceSchemaError as exc:
        return replace(atlas_check, ok=False, status='schema', message=str(exc)), ()


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


def _spine_resource_specs(
    entry: ModelEntry,
    *,
    parts_source: str | None,
    atlas_sources: Mapping[str, str] | None,
) -> tuple[_ResourceSpec, ...]:
    manifest = spine_resource_manifest(entry.resources.primary_url, parts_source=parts_source)
    specs: list[_ResourceSpec] = []
    if manifest.parts_manifest_url:
        specs.append(
            _ResourceSpec(
                kind='spine.parts',
                source_url=manifest.parts_manifest_url,
                fallback_url='',
                base_url=manifest.parts_manifest_url,
                context=_asset_context(entry, {'spine_field': 'parts'}),
            ),
        )

    for part in manifest.parts:
        specs.append(
            _ResourceSpec(
                kind='spine.skel',
                source_url=part.skeleton_url,
                fallback_url='',
                base_url=part.skeleton_url,
                context=_asset_context(entry, {'spine_field': 'skel', 'spine_part': part.name}),
            ),
        )
        for atlas_url in part.atlas_urls:
            specs.append(  # noqa: PERF401
                _ResourceSpec(
                    kind='spine.atlas',
                    source_url=atlas_url,
                    fallback_url='',
                    base_url=atlas_url,
                    context=_asset_context(entry, {'spine_field': 'atlas', 'spine_part': part.name}),
                ),
            )
        specs.extend(_spine_texture_specs(entry, part=part, atlas_sources=atlas_sources))
    return tuple(specs)


def _spine_texture_specs(
    entry: ModelEntry,
    *,
    part: SpineModelPart,
    atlas_sources: Mapping[str, str] | None,
) -> tuple[_ResourceSpec, ...]:
    if atlas_sources is None:
        return ()

    specs: list[_ResourceSpec] = []
    for atlas_url in part.atlas_urls:
        atlas_source = atlas_sources.get(atlas_url)
        if atlas_source is None:
            continue
        specs.extend(
            _ResourceSpec(
                kind='spine.texture',
                source_url=_resolve_resource_url(atlas_url, texture_path),
                fallback_url='',
                base_url=atlas_url,
                context=_asset_context(
                    entry,
                    {
                        'spine_field': 'texture',
                        'spine_part': part.name,
                        'atlas_page': texture_path,
                        'page_index': page_index,
                    },
                ),
            )
            for page_index, texture_path in enumerate(_parse_spine_atlas_texture_paths(atlas_source))
        )
    return tuple(specs)


def _painting_resource_specs(entry: ModelEntry, *, voice_lines: tuple[L2DSuVoiceLine, ...]) -> tuple[_ResourceSpec, ...]:
    painting_key = _painting_key_from_url(entry.resources.primary_url)
    specs = [
        _ResourceSpec(
            kind='painting.image',
            source_url=entry.resources.primary_url,
            fallback_url='',
            base_url=entry.resources.primary_url,
            context=_asset_context(entry, {'painting_field': 'image'}),
        ),
    ]
    # No case fallback is spelled out here: the downloader derives case variants for any
    # candidate that fails, which covers the filename and the key-bearing directory alike.
    specs.extend(
        _ResourceSpec(
            kind='painting.face',
            source_url=l2d_su_painting_face_url(painting_key, face_id),
            fallback_url='',
            base_url=_l2d_su_relative_base_url(),
            context=_asset_context(entry, {'painting_field': 'face', 'face_id': face_id}),
        )
        for face_id in entry.costume.face_ids
    )
    # q_icon is deliberately absent: the index advertises qicon/<key> for every skin, but the
    # CDN hosts none of them -- l2d.su's own page renders that slot broken too.
    icon_fields: tuple[tuple[ResourceAssetKind, str], ...] = (
        ('icon.square', entry.costume.square_icon),
        ('icon.shipyard', entry.costume.shipyard_icon),
    )
    specs.extend(
        _ResourceSpec(
            kind=kind,
            source_url=l2d_su_icon_url(icon_path),
            fallback_url='',
            base_url=_l2d_su_relative_base_url(),
            context=_asset_context(entry, {'painting_field': 'icon', 'icon_path': icon_path}),
        )
        for kind, icon_path in icon_fields
        if icon_path
    )
    specs.extend(
        _ResourceSpec(
            kind='voice.audio',
            source_url=l2d_su_voice_url(line.voice_path),
            fallback_url='',
            base_url=_l2d_su_relative_base_url(),
            context=_asset_context(entry, {'painting_field': 'voice', **asdict(line)}),
        )
        for line in voice_lines
    )
    return tuple(specs)


def _painting_key_from_url(url: str) -> str:
    return PurePosixPath(urlsplit(url).path).name.removesuffix('.webp')


def _segment_case_variants(segment: str) -> tuple[str, ...]:
    lowered = segment.lower()
    capitalised = segment[:1].upper() + segment[1:] if segment else segment
    return tuple(dict.fromkeys(v for v in (lowered, capitalised) if v and v != segment))


def case_variant_urls(url: str) -> tuple[str, ...]:
    """URLs differing from `url` only in the case of the segment carrying the model key.

    l2d.su's object storage is case-sensitive and its index disagrees with it in both
    directions: the index offers painting/Z28_3.webp where the object is z28_3.webp, and
    painting/u47_5.webp where the object is U47_5.webp. Neither lowercasing nor capitalising
    everything is right, so both are offered as candidates and tried only after the
    index-given URL has already failed.

    Only the segment that can carry the key is varied. Face variations are numbered files
    under a directory named for the key, so a numeric filename means the key sits one level
    up; everything else carries it in the filename. The segment above that is a fixed CDN
    category (painting, squareicon, paintingface) and is never varied.
    """
    split = urlsplit(url)
    head, separator, filename = split.path.rpartition('/')
    if not separator:
        return ()

    if PurePosixPath(filename).stem.isdigit():
        parent_head, parent_separator, parent = head.rpartition('/')
        if not parent_separator:
            return ()
        paths = [f'{parent_head}/{variant}/{filename}' for variant in _segment_case_variants(parent)]
    else:
        paths = [f'{head}/{variant}' for variant in _segment_case_variants(filename)]

    return tuple(urlunsplit(split._replace(path=path)) for path in dict.fromkeys(paths))


def _l2d_su_relative_base_url() -> str:
    """Base marker that makes local paths mirror the CDN layout below the azurlane root."""
    return f'{L2D_SU_STATIC_BASE_URL}/marker'


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
    motion_references, audio_references, motion_names, has_audio, has_text = _parse_live2d_motion_references(
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
) -> tuple[tuple[ResourceReference, ...], tuple[ResourceReference, ...], tuple[str, ...], bool, bool]:
    if not isinstance(value, dict):
        return (), (), (), False, False

    names: list[str] = []
    has_audio = False
    has_text = False
    motion_references: list[ResourceReference] = []
    audio_references: list[ResourceReference] = []
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
            if parsed.text:
                has_text = True

    return tuple(motion_references), tuple(audio_references), _unique_non_empty(names), has_audio, has_text


def _parse_live2d_motion_item(motion: Any, *, base_url: str, group: str, index: int) -> _Live2DMotionReferences:
    if not isinstance(motion, dict):
        return _Live2DMotionReferences()

    context = {
        'live2d_field': 'motion',
        'motion_group': group,
        'motion_index': index,
    }
    motion_file = _optional_resource_reference_path(motion, 'File')
    # Cubism names the field Sound; l2d.su's older models carry an absolute URL under Audio.
    sound_file = _optional_resource_reference_path(motion, 'Sound') or _optional_resource_reference_path(motion, 'Audio')
    # Text is the subtitle line itself (spoken dialogue), not a file path. Treating it as a
    # path once produced hundreds of 404s ending in full sentences of dialogue.
    text = _optional_resource_reference_path(motion, 'Text')
    if text:
        context['motion_text'] = text
    return _Live2DMotionReferences(
        motion=_optional_motion_reference(kind='live2d.motion', raw_path=motion_file, base_url=base_url, metadata=context),
        audio=_optional_motion_reference(
            kind='live2d.audio',
            raw_path=sound_file,
            base_url=base_url,
            metadata={**context, 'live2d_field': 'audio'},
        ),
        text=text,
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


def spine_parts_manifest_url(url: str) -> str:
    return f'{_spine_stem_url(url)}{SPINE_PARTS_MANIFEST_EXTENSION}'


def spine_resource_manifest(url: str, *, parts_source: str | None = None) -> SpineResourceManifest:
    stem_url = _spine_stem_url(url)
    if parts_source is None:
        name = PurePosixPath(urlsplit(stem_url).path).name
        part = SpineModelPart(name=name, skeleton_url=f'{stem_url}.skel', atlas_urls=(f'{stem_url}.atlas',))
        return SpineResourceManifest(parts=(part,))

    parts_url = spine_parts_manifest_url(url)
    return SpineResourceManifest(
        parts=parse_spine_parts_manifest(parts_source, parts_manifest_url=parts_url),
        parts_manifest_url=parts_url,
    )


def parse_spine_parts_manifest(source: str, *, parts_manifest_url: str) -> tuple[SpineModelPart, ...]:
    payload = _decode_json_object(source, context='Spine parts manifest')
    models = payload.get('models')
    if not isinstance(models, list):
        msg = 'Spine parts manifest must contain a models list'
        raise SourceSchemaError(msg)

    parts = tuple(
        _spine_model_part(model, parts_manifest_url=parts_manifest_url, index=index)
        for index, model in enumerate(models)
        if isinstance(model, dict)
    )
    if parts:
        return parts
    msg = 'Spine parts manifest does not contain a model with a skeleton'
    raise SourceSchemaError(msg)


def _spine_model_part(model: dict[str, Any], *, parts_manifest_url: str, index: int) -> SpineModelPart:
    context = f'Spine parts manifest models[{index}]'
    skeleton = _required_str(model, 'skeleton', context=context)
    atlases = model.get('atlases')
    atlas_paths = _string_tuple(atlases) if isinstance(atlases, list) else ()
    return SpineModelPart(
        name=_optional_str(model, 'name') or _resource_basename(skeleton),
        skeleton_url=_resolve_resource_url(parts_manifest_url, skeleton),
        atlas_urls=tuple(_resolve_resource_url(parts_manifest_url, atlas_path) for atlas_path in atlas_paths),
    )


def _spine_stem_url(url: str) -> str:
    base_url = url.rstrip('/')
    for suffix in ('.skel', '.atlas', SPINE_PARTS_MANIFEST_EXTENSION):
        if base_url.endswith(suffix):
            return base_url.removesuffix(suffix)

    path_name = PurePosixPath(urlsplit(base_url).path).name
    file_stem = path_name.removesuffix('-spine') if path_name.endswith('-spine') else path_name
    return f'{base_url}/{file_stem}'


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
                nation=character.nation,
                ship_type=character.ship_type,
                rarity=character.rarity,
                default_skin_id=character.default_skin_id,
                skin_series=character.skin_series,
            ),
            costume=ModelCostume(
                id=model.costume_id,
                key=_catalog_key(model_key),
                name_zh=model.costume_name,
                name_en=model.costume_name_en,
                skin_type=model.skin_type,
                feature_tags=model.feature_tags,
                is_live2d_plus=model.is_live2d_plus,
                face_ids=model.face_ids,
                square_icon=model.square_icon,
                shipyard_icon=model.shipyard_icon,
                q_icon=model.q_icon,
                shop_type=model.shop_type,
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
    if model.kind == 'painting':
        filename = filename.removesuffix('.webp')
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


def _decode_json_object(source: str, *, context: str) -> dict[str, Any]:
    try:
        payload = json.loads(source)
    except json.JSONDecodeError as exc:
        msg = f'{context} is not valid JSON: {exc.msg}'
        raise SourceParseError(msg) from exc

    if not isinstance(payload, dict):
        msg = f'{context} root must be an object'
        raise SourceSchemaError(msg)
    return payload


def _parse_l2d_su_ship(item: Any, *, index: int) -> L2DSuCharacterSnapshot:
    context = f'l2d.su ships[{index}]'
    if not isinstance(item, dict):
        msg = f'{context} must be an object'
        raise SourceSchemaError(msg)

    models = _parse_l2d_su_skins(item, context=context)
    default_skin_id = item.get('defaultSkinId')
    return L2DSuCharacterSnapshot(
        char_id=_required_int(item, 'shipGroupId', context=context),
        char_key=_required_str(item, 'resourceKey', context=context),
        char_name=_required_str(item, 'name', context=context),
        char_name_en=_optional_str(item, 'englishName'),
        nation=_optional_str(item, 'nationName'),
        ship_type=_optional_str(item, 'typeName'),
        rarity=_optional_str(item, 'rarityName'),
        default_skin_id=default_skin_id if isinstance(default_skin_id, int) and not isinstance(default_skin_id, bool) else None,
        skin_series=_l2d_su_skin_series(item),
        live2d=tuple(model for model in models if model.kind == 'live2d'),
        spine=tuple(model for model in models if model.kind == 'spine'),
        paintings=tuple(model for model in models if model.kind == 'painting'),
    )


def _l2d_su_skin_series(item: dict[str, Any]) -> tuple[str, ...]:
    """Skin series labels for a ship, matching the values in the index's filters.skinSeries."""
    skins = item.get('skins')
    if not isinstance(skins, list):
        return ()

    series: list[str] = []
    for skin in skins:
        if not isinstance(skin, dict):
            continue
        # l2d.su labels a skin by its shop series, falling back to the skin type for shopless skins.
        label = _optional_str(skin, 'shopTypeName') or _optional_str(skin, 'skinTypeName')
        if label and label not in series:
            series.append(label)
    return tuple(series)


def _parse_l2d_su_skins(item: dict[str, Any], *, context: str) -> tuple[L2DSuModelSnapshot, ...]:
    skins = item.get('skins', [])
    if not isinstance(skins, list):
        msg = f'{context}.skins must be a list'
        raise SourceSchemaError(msg)

    models: list[L2DSuModelSnapshot] = []
    for index, skin in enumerate(skins):
        if not _assert_object(skin, context=f'{context}.skins[{index}]'):
            continue
        models.extend(_parse_l2d_su_skin(skin, context=f'{context}.skins[{index}]'))
    return tuple(models)


def _parse_l2d_su_skin(skin: dict[str, Any], *, context: str) -> tuple[L2DSuModelSnapshot, ...]:
    snapshots: list[L2DSuModelSnapshot] = []
    kind = _l2d_su_skin_kind(skin)
    model_key = _optional_str(skin, 'painting') or _optional_str(skin, 'prefab')
    if kind is not None and model_key:
        snapshots.append(_l2d_su_skin_snapshot(skin, kind=kind, model_key=model_key, context=context))

    painting_key = _optional_str(skin, 'painting')
    if painting_key:
        snapshots.append(_l2d_su_skin_snapshot(skin, kind='painting', model_key=painting_key, context=context))
    return tuple(snapshots)


def _l2d_su_skin_snapshot(skin: dict[str, Any], *, kind: L2DSuModelKind, model_key: str, context: str) -> L2DSuModelSnapshot:
    asset_paths = skin.get('assetPaths')
    icons = asset_paths if isinstance(asset_paths, dict) else {}
    return L2DSuModelSnapshot(
        kind=kind,
        costume_id=_required_int(skin, 'id', context=context),
        costume_name=_optional_str(skin, 'name') or model_key,
        costume_name_en='',
        path=_l2d_su_model_url(kind=kind, model_key=model_key),
        model_key=model_key,
        skin_type=_optional_str(skin, 'skinTypeName'),
        feature_tags=_string_tuple(skin.get('featureTags')),
        is_live2d_plus=skin.get('isLive2dPlus') is True,
        face_ids=_string_tuple(skin.get('paintingFaceIds')),
        square_icon=_optional_str(icons, 'squareIcon'),
        shipyard_icon=_optional_str(icons, 'shipyardIcon'),
        q_icon=_optional_str(icons, 'qIcon'),
        shop_type=_optional_str(skin, 'shopTypeName'),
    )


def _l2d_su_skin_kind(skin: dict[str, Any]) -> L2DSuModelKind | None:
    dynamic_type = _optional_str(skin, 'dynamicType')
    if skin.get('isLive2d') is True or dynamic_type == 'live2d':
        return 'live2d'
    if skin.get('isSpine') is True or dynamic_type == 'spine':
        return 'spine'
    return None


def _l2d_su_model_url(*, kind: L2DSuModelKind, model_key: str) -> str:
    if kind == 'live2d':
        relative_path = f'live2d/{model_key}/{model_key}.model3.json'
    elif kind == 'spine':
        relative_path = f'spinepainting/{model_key}'
    else:
        relative_path = f'painting/{model_key}.webp'
    return f'{L2D_SU_STATIC_BASE_URL}/{relative_path}'


def l2d_su_painting_url(painting_key: str) -> str:
    return f'{L2D_SU_STATIC_BASE_URL}/painting/{painting_key}.webp'


def l2d_su_painting_face_url(painting_key: str, face_id: str) -> str:
    return f'{L2D_SU_STATIC_BASE_URL}/paintingface/{painting_key}/{face_id}.webp'


def l2d_su_icon_url(icon_path: str) -> str:
    return f'{L2D_SU_STATIC_BASE_URL}/{icon_path}.webp'


def l2d_su_voice_url(voice_path: str) -> str:
    return f'{L2D_SU_STATIC_BASE_URL}/{voice_path}.ogg'


def _parse_l2d_su_new_skins(value: Any) -> tuple[L2DSuNewSkin, ...]:
    if not isinstance(value, list):
        return ()

    return tuple(
        L2DSuNewSkin(
            ship_group_id=item.get('shipGroupId') if isinstance(item.get('shipGroupId'), int) else 0,
            ship_name=_optional_str(item, 'shipName'),
            skin_name=_optional_str(item, 'skinName'),
            skin_type=_optional_str(item, 'skinType'),
            skin_ids=tuple(skin_id for skin_id in item.get('skinIds', []) if isinstance(skin_id, int) and not isinstance(skin_id, bool)),
        )
        for item in value
        if isinstance(item, dict)
    )


def _character_with_english_names(
    character: L2DSuCharacterSnapshot,
    english: L2DSuCharacterSnapshot | None,
) -> L2DSuCharacterSnapshot:
    if english is None:
        return character

    english_names = {model.costume_id: model.costume_name for model in (*english.live2d, *english.spine, *english.paintings)}
    return replace(
        character,
        char_name_en=english.char_name or character.char_name_en,
        live2d=tuple(replace(model, costume_name_en=english_names.get(model.costume_id, '')) for model in character.live2d),
        spine=tuple(replace(model, costume_name_en=english_names.get(model.costume_id, '')) for model in character.spine),
        paintings=tuple(replace(model, costume_name_en=english_names.get(model.costume_id, '')) for model in character.paintings),
    )


def _fetch_l2d_su_index(
    *,
    region: str,
    timeout: float,
    client: httpx.Client | None,
    origin_throttle: Callable[[], None] | None = None,
) -> tuple[L2DSuIndexData | None, SourceFetchMetadata, tuple[SourceSnapshotError, ...]]:
    url = l2d_su_ship_index_url(region)
    if origin_throttle is not None:
        origin_throttle()
    response, metadata, error = _fetch_source(url=url, timeout=timeout, client=client)
    if error is not None:
        return None, metadata, (error,)
    if response is None:
        return None, metadata, (_source_error('network', 'Fetch did not return a response', metadata),)

    try:
        parsed = parse_l2d_su_ship_index(response.text, region=region)
    except SourceParseError as exc:
        return None, metadata, (_source_error('parse', str(exc), metadata),)
    except SourceSchemaError as exc:
        return None, metadata, (_source_error('schema', str(exc), metadata),)
    return parsed, metadata, ()


def _parse_nagami_mapping_entry(key: str, value: Any) -> NagamiMappingEntry:
    if isinstance(value, str):
        name, background = value, ''
    elif isinstance(value, dict):
        name, background = _optional_str(value, 'name'), _optional_str(value, 'bg')
    else:
        msg = f'Nagami mapping value for {key!r} must be a string or an object'
        raise SourceSchemaError(msg)

    if not name.strip():
        msg = f'Nagami mapping name for {key!r} must be a non-empty string'
        raise SourceSchemaError(msg)
    return NagamiMappingEntry(key=key, name=name, background=background)


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


def _optional_str(item: dict[str, Any], field_name: str) -> str:
    value = item.get(field_name)
    if not isinstance(value, str):
        return ''
    return value.strip()


def _string_tuple(value: Any) -> tuple[str, ...]:
    if not isinstance(value, list):
        return ()
    return tuple(item.strip() for item in value if isinstance(item, str) and item.strip())


def _request_headers() -> dict[str, str]:
    return {
        'Accept': 'application/json, text/javascript, */*',
        'Referer': L2D_SU_REFERER,
        'User-Agent': DEFAULT_USER_AGENT,
    }


def _utc_now_iso() -> str:
    return datetime.now(UTC).isoformat()

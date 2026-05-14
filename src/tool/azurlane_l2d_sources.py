from __future__ import annotations

import json
from dataclasses import asdict, dataclass
from datetime import UTC, datetime
from typing import Any, Literal

import httpx

L2D_SU_CATALOG_URL = 'https://l2d.su/json/live2dMaster.json'
NAGAMI_MAPPING_BUNDLE_URL = 'https://azurlane.nagami.moe/_app/immutable/chunks/l2d_mapping.oLieetCb.js'
AZUR_LANE_GAME_ID = 1
DEFAULT_USER_AGENT = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36'
HTTP_ERROR_MIN = 400

SourceErrorKind = Literal['network', 'parse', 'schema']
L2DSuModelKind = Literal['live2d', 'spine']


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

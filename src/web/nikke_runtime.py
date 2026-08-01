from __future__ import annotations

import math
import re
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any, Self
from urllib.parse import urljoin

from src.web.nikke_layer_metadata import live2d_layer_fingerprint

GAMEKEE_BASE_URL = 'https://www.gamekee.com'
DEFAULT_RUNTIME_TIMEOUT_MS = 60_000
DEFAULT_RUNTIME_VIEWPORT = {'width': 1440, 'height': 1000}
LAYER_CAPTURE_SCHEMA = 2
LAYER_CAPTURE_MATCH_METHOD = 'gamekee-runtime-container'
RUNTIME_ANIMATION_MATCH_METHOD = 'gamekee-runtime-player'
RUNTIME_CLICK_MATCH_METHOD = 'gamekee-runtime-player-event'
LIVE2D_KEY_RE = re.compile(r'/live2d/[^/]+/([^/?#]+)/')
GAMEKEE_RUNTIME_CAPTURE_SCRIPT = r"""
(() => {
  const version = 1;
  const existing = window.__gvRuntimeCapture;
  if (existing && existing.version === version) return;

  const state = {
    version,
    patched: false,
    players: [],
    events: [],
    sequence: 0,
  };

  function nowMs() {
    return Math.round(performance.now());
  }

  function animationName(animation) {
    if (typeof animation === "string") return animation;
    if (animation && animation.name) return String(animation.name);
    return "";
  }

  function toInt(value) {
    const parsed = Number.parseInt(value, 10);
    return Number.isFinite(parsed) ? parsed : null;
  }

  function containerForPlayer(player) {
    const parent = player && player.parent;
    return parent && parent.closest ? parent.closest(".spine-player-container") : null;
  }

  function containerIndex(container) {
    if (!container) return -1;
    return [...document.querySelectorAll(".spine-player-container")].indexOf(container);
  }

  function animationDurationsMs(player) {
    const animations = player?.skeleton?.data?.animations;
    if (!Array.isArray(animations)) return {};
    const out = {};
    for (const animation of animations) {
      const name = animationName(animation);
      if (!name) continue;
      const duration = Number(animation.duration);
      if (Number.isFinite(duration) && duration >= 0) {
        out[name] = Math.round(duration * 1000);
      }
    }
    return out;
  }

  function playerSummary(player, playerIndex) {
    const container = containerForPlayer(player);
    const style = container ? getComputedStyle(container) : null;
    return {
      playerIndex,
      containerIndex: containerIndex(container),
      zIndex: style ? toInt(style.zIndex) : null,
      rawZIndex: style ? style.zIndex : "",
      atlasUrl: player?.config?.atlasUrl || "",
      skelUrl: player?.config?.skelUrl || player?.config?.binaryUrl || "",
      configuredAnimation: player?.config?.animation || "",
      currentAnimation: player?.animationState?.tracks?.[0]?.animation?.name || "",
      animationDurationsMs: animationDurationsMs(player),
    };
  }

  function record(type, payload) {
    state.events.push({
      sequence: ++state.sequence,
      timeMs: nowMs(),
      type,
      ...payload,
    });
  }

  function ensurePlayer(player) {
    let playerIndex = state.players.indexOf(player);
    if (playerIndex === -1) {
      playerIndex = state.players.length;
      state.players.push(player);
      record("player", playerSummary(player, playerIndex));
    }
    return playerIndex;
  }

  function patchSpine(spine) {
    if (!spine || state.patched) return;
    state.patched = true;

    const playerPrototype = spine.SpinePlayer && spine.SpinePlayer.prototype;
    if (playerPrototype) {
      for (const method of ["initialize", "loadSkeleton", "play", "setAnimation", "addAnimation"]) {
        const original = playerPrototype[method];
        if (typeof original !== "function" || original.__gvRuntimePatched) continue;
        playerPrototype[method] = function patchedPlayerMethod(...args) {
          const playerIndex = ensurePlayer(this);
          record("playerMethod", {
            playerIndex,
            method,
            args: args.map((arg) => animationName(arg) || String(arg ?? "")).slice(0, 8),
            ...playerSummary(this, playerIndex),
          });
          return original.apply(this, args);
        };
        playerPrototype[method].__gvRuntimePatched = true;
      }
    }

    const statePrototype = spine.AnimationState && spine.AnimationState.prototype;
    if (statePrototype) {
      for (const method of ["setAnimation", "addAnimation", "setAnimationWith", "addAnimationWith"]) {
        const original = statePrototype[method];
        if (typeof original !== "function" || original.__gvRuntimePatched) continue;
        statePrototype[method] = function patchedAnimationStateMethod(...args) {
          const playerIndex = state.players.findIndex((player) => player && player.animationState === this);
          record("animationState", {
            playerIndex,
            method,
            trackIndex: Number.isFinite(args[0]) ? args[0] : null,
            animation: animationName(args[1]),
            loop: typeof args[2] === "boolean" ? args[2] : null,
            delayMs: Number.isFinite(args[3]) ? Math.round(args[3] * 1000) : null,
          });
          return original.apply(this, args);
        };
        statePrototype[method].__gvRuntimePatched = true;
      }
    }
  }

  state.snapshot = () => {
    const events = state.events.slice();
    return state.players
      .map((player, playerIndex) => ({
        ...playerSummary(player, playerIndex),
        events: events.filter((event) => event.playerIndex === playerIndex),
      }))
      .filter((player) => player.containerIndex >= 0);
  };

  state.eventCount = () => state.sequence;

  Object.defineProperty(window, "__gvRuntimeCapture", {
    value: state,
    configurable: true,
  });

  let spineValue = window.spine_4_1_54;
  Object.defineProperty(window, "spine_4_1_54", {
    configurable: true,
    get() {
      return spineValue;
    },
    set(value) {
      spineValue = value;
      patchSpine(value);
    },
  });
  if (spineValue) patchSpine(spineValue);
})();
"""


class RuntimeCaptureDependencyError(RuntimeError):
    pass


def _utc_now_iso() -> str:
    return datetime.now(UTC).isoformat()


def _to_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float) and math.isfinite(value) and value.is_integer():
        return int(value)
    if isinstance(value, str):
        match = re.match(r'^[+-]?\d+', value.strip())
        if match:
            return int(match.group(0))
    return None


def _to_float(value: Any) -> float | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int | float) and math.isfinite(value):
        return float(value)
    if isinstance(value, str):
        try:
            parsed = float(value.strip())
        except ValueError:
            return None
        return parsed if math.isfinite(parsed) else None
    return None


def _first_text(data: Mapping[str, Any], *keys: str) -> str:
    for key in keys:
        value = data.get(key)
        if value is None:
            continue
        text = str(value).strip()
        if text:
            return text
    return ''


def _normalize_url(url: str) -> str:
    value = url.strip()
    if not value:
        return ''
    if value.startswith('//'):
        return f'https:{value}'
    if value.startswith('/'):
        return urljoin(GAMEKEE_BASE_URL, value)
    return value


def _unique_in_order(values: Iterable[str]) -> list[str]:
    seen: set[str] = set()
    out: list[str] = []
    for raw_value in values:
        value = str(raw_value or '').strip()
        if not value or value in seen:
            continue
        seen.add(value)
        out.append(value)
    return out


def live2d_key_from_url(url: str) -> str:
    match = LIVE2D_KEY_RE.search(url)
    return match.group(1) if match else ''


def unique_live2d_keys(urls: Iterable[str]) -> list[str]:
    return _unique_in_order(live2d_key_from_url(url) for url in urls)


@dataclass(frozen=True, slots=True)
class RuntimeContainerSnapshot:
    index: int
    z_index: int | None
    raw_z_index: str = ''
    class_name: str = ''
    parent_class_name: str = ''
    canvas_count: int = 0
    rect: dict[str, float] = field(default_factory=dict)

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any], *, fallback_index: int) -> Self:
        raw_z_index = _first_text(raw, 'raw_z_index', 'rawZIndex')
        z_index = _to_int(raw.get('z_index', raw.get('zIndex')))
        if z_index is None:
            z_index = _to_int(raw_z_index)

        rect: dict[str, float] = {}
        raw_rect = raw.get('rect')
        if isinstance(raw_rect, Mapping):
            for key in ('x', 'y', 'width', 'height'):
                value = _to_float(raw_rect.get(key))
                if value is not None:
                    rect[key] = value

        return cls(
            index=_to_int(raw.get('index')) if _to_int(raw.get('index')) is not None else fallback_index,
            z_index=z_index,
            raw_z_index=raw_z_index,
            class_name=_first_text(raw, 'class_name', 'className'),
            parent_class_name=_first_text(raw, 'parent_class_name', 'parentClassName'),
            canvas_count=_to_int(raw.get('canvas_count', raw.get('canvasCount'))) or 0,
            rect=rect,
        )

    def to_raw_container(self) -> dict[str, Any]:
        return {
            'index': self.index,
            'zIndex': self.z_index,
            'rawZIndex': self.raw_z_index,
            'className': self.class_name,
            'parentClassName': self.parent_class_name,
            'canvasCount': self.canvas_count,
            'rect': dict(self.rect),
        }


@dataclass(frozen=True, slots=True)
class RuntimeAnimationEvent:
    sequence: int
    time_ms: int | None
    event_type: str
    method: str
    player_index: int | None
    track_index: int | None
    animation: str
    loop: bool | None
    delay_ms: int | None

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any]) -> Self:
        loop = raw.get('loop')
        return cls(
            sequence=_to_int(raw.get('sequence', raw.get('seq'))) or 0,
            time_ms=_to_int(raw.get('time_ms', raw.get('timeMs'))),
            event_type=_first_text(raw, 'type'),
            method=_first_text(raw, 'method'),
            player_index=_to_int(raw.get('player_index', raw.get('playerIndex'))),
            track_index=_to_int(raw.get('track_index', raw.get('trackIndex'))),
            animation=_first_text(raw, 'animation'),
            loop=loop if isinstance(loop, bool) else None,
            delay_ms=_to_int(raw.get('delay_ms', raw.get('delayMs'))),
        )

    def to_payload(self) -> dict[str, Any]:
        return {
            'sequence': self.sequence,
            'time_ms': self.time_ms,
            'type': self.event_type,
            'method': self.method,
            'player_index': self.player_index,
            'track_index': self.track_index,
            'animation': self.animation,
            'loop': self.loop,
            'delay_ms': self.delay_ms,
        }


@dataclass(frozen=True, slots=True)
class RuntimePlayerSnapshot:
    player_index: int
    container_index: int
    z_index: int | None
    raw_z_index: str
    atlas_url: str
    skel_url: str
    configured_animation: str
    current_animation: str
    animation_durations_ms: dict[str, int]
    events: tuple[RuntimeAnimationEvent, ...] = ()

    @classmethod
    def from_raw(cls, raw: Mapping[str, Any], *, fallback_index: int) -> Self:
        durations: dict[str, int] = {}
        raw_durations = raw.get('animation_durations_ms', raw.get('animationDurationsMs'))
        if isinstance(raw_durations, Mapping):
            for key, value in raw_durations.items():
                duration_ms = _to_int(value)
                if duration_ms is not None and duration_ms >= 0:
                    durations[str(key)] = duration_ms

        raw_events = raw.get('events')
        events = (
            tuple(RuntimeAnimationEvent.from_raw(event) for event in raw_events if isinstance(event, Mapping))
            if isinstance(raw_events, Sequence) and not isinstance(raw_events, str | bytes | bytearray)
            else ()
        )

        return cls(
            player_index=_to_int(raw.get('player_index', raw.get('playerIndex'))) or fallback_index,
            container_index=_to_int(raw.get('container_index', raw.get('containerIndex'))) or fallback_index,
            z_index=_to_int(raw.get('z_index', raw.get('zIndex'))),
            raw_z_index=_first_text(raw, 'raw_z_index', 'rawZIndex'),
            atlas_url=_normalize_url(_first_text(raw, 'atlas_url', 'atlasUrl')),
            skel_url=_normalize_url(_first_text(raw, 'skel_url', 'skelUrl')),
            configured_animation=_first_text(raw, 'configured_animation', 'configuredAnimation', 'animation'),
            current_animation=_first_text(raw, 'current_animation', 'currentAnimation', 'current'),
            animation_durations_ms=durations,
            events=events,
        )

    def runtime_key(self) -> str:
        return live2d_key_from_url(self.atlas_url) or live2d_key_from_url(self.skel_url)

    def to_payload(self) -> dict[str, Any]:
        return {
            'player_index': self.player_index,
            'container_index': self.container_index,
            'z_index': self.z_index,
            'raw_z_index': self.raw_z_index,
            'atlas_url': self.atlas_url,
            'skel_url': self.skel_url,
            'configured_animation': self.configured_animation,
            'current_animation': self.current_animation,
            'runtime_key': self.runtime_key(),
        }


@dataclass(frozen=True, slots=True)
class LayerCaptureBuildInput:
    content_id: int
    title: str = ''
    models: Sequence[Mapping[str, Any]] = ()
    containers: Sequence[Mapping[str, Any] | RuntimeContainerSnapshot] = ()
    runtime_players: Sequence[Mapping[str, Any] | RuntimePlayerSnapshot] = ()
    runtime_click_start_sequence: int | None = None
    requested_live2d_keys: Sequence[str] = ()
    captured_at: str = ''
    source_url: str = ''


@dataclass(frozen=True, slots=True)
class RuntimeCaptureRequest:
    content_id: int
    models: Sequence[Mapping[str, Any]]
    title: str = ''
    timeout_ms: int = DEFAULT_RUNTIME_TIMEOUT_MS
    headless: bool = True
    source_url: str = ''
    expected_layer_count: int | None = None


@dataclass(frozen=True, slots=True)
class RuntimeGroupCapture:
    models: Sequence[Mapping[str, Any]]
    containers: Sequence[RuntimeContainerSnapshot]
    requested_live2d_keys: Sequence[str]
    runtime_players: Sequence[RuntimePlayerSnapshot] = ()
    runtime_click_start_sequence: int | None = None


def _model_live2d_key(model: Mapping[str, Any]) -> str:
    return _first_text(model, 'live2d_key', 'live2dKey')


def _model_stable_id(model: Mapping[str, Any]) -> str:
    return _first_text(model, 'stable_id', 'stableId')


def _model_skin_index(model: Mapping[str, Any]) -> int | None:
    return _to_int(model.get('skin_index', model.get('skinIndex')))


def _model_skin_title(model: Mapping[str, Any]) -> str:
    return _first_text(model, 'skin_title', 'skinTitle', 'skin_name', 'skinName')


def _model_resource_urls(model: Mapping[str, Any]) -> dict[str, Any]:
    raw_urls = model.get('resource_urls', model.get('resourceUrls'))
    if raw_urls is None:
        raw_urls = model.get('urls')
    if not isinstance(raw_urls, Mapping):
        return {}

    urls: dict[str, Any] = {}
    for key, value in raw_urls.items():
        field_name = str(key)
        if isinstance(value, str):
            normalized = _normalize_url(value)
            if normalized:
                urls[field_name] = normalized
        elif isinstance(value, Sequence) and not isinstance(value, str | bytes | bytearray):
            normalized_values = [_normalize_url(item) for item in value if isinstance(item, str)]
            urls[field_name] = [item for item in normalized_values if item]
    return urls


def _model_runtime_animation_state(value: Any) -> dict[str, Any]:
    if not isinstance(value, Mapping):
        return {}

    state_payload: dict[str, Any] = {}
    animation = _first_text(value, 'animation')
    if animation:
        state_payload['animation'] = animation
    enabled = value.get('enabled')
    if isinstance(enabled, bool):
        state_payload['enabled'] = enabled
    loop = value.get('loop')
    if isinstance(loop, bool):
        state_payload['loop'] = loop
    match_method = _first_text(value, 'match_method', 'matchMethod')
    if match_method:
        state_payload['match_method'] = match_method
    match_confidence = _first_text(value, 'match_confidence', 'matchConfidence')
    if match_confidence in {'high', 'medium', 'low'}:
        state_payload['match_confidence'] = match_confidence
    duration_ms = _to_int(value.get('duration_ms', value.get('durationMs')))
    if duration_ms is not None and duration_ms >= 0:
        state_payload['duration_ms'] = duration_ms
    return state_payload


def _model_runtime_animations(model: Mapping[str, Any]) -> dict[str, Any]:
    raw = model.get('runtime_animations', model.get('runtimeAnimations'))
    if not isinstance(raw, Mapping):
        return {}

    out: dict[str, Any] = {}
    for state in ('idle', 'click'):
        state_payload = _model_runtime_animation_state(raw.get(state))
        if state_payload:
            out[state] = state_payload
    return out


def layer_capture_fingerprint(content_id: int, models: Sequence[Mapping[str, Any]]) -> str:
    return live2d_layer_fingerprint(content_id, [dict(model) for model in models])


def _model_kind(model: Mapping[str, Any]) -> str:
    text_parts = [str(model.get('label') or ''), str(model.get('live2d_key') or ''), str(model.get('animation') or '')]
    urls = model.get('urls')
    if isinstance(urls, Mapping):
        for value in urls.values():
            if isinstance(value, str):
                text_parts.append(value)
            elif isinstance(value, Sequence) and not isinstance(value, str | bytes | bytearray):
                text_parts.extend(str(item) for item in value)
    normalized = ' '.join(text_parts).lower()
    has_aim = 'aim' in normalized
    has_cover = 'cover' in normalized
    if has_cover and not has_aim:
        return 'cover'
    if has_aim and not has_cover:
        return 'aim'
    return 'full'


def _group_models_by_skin_and_kind(models: Sequence[Mapping[str, Any]]) -> list[list[Mapping[str, Any]]]:
    groups: dict[tuple[int | None, str], list[Mapping[str, Any]]] = {}
    for model in models:
        groups.setdefault((_model_skin_index(model), _model_kind(model)), []).append(model)
    return list(groups.values())


def multi_full_model_groups(models: Sequence[Mapping[str, Any]]) -> list[list[Mapping[str, Any]]]:
    groups = [group for group in _group_models_by_skin_and_kind(models) if group and _model_kind(group[0]) == 'full' and len(group) > 1]
    return sorted(groups, key=lambda group: -1 if _model_skin_index(group[0]) is None else _model_skin_index(group[0]))


def first_skin_model_group(models: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    if not models:
        return []
    full_groups = [group for group in _group_models_by_skin_and_kind(models) if group and _model_kind(group[0]) == 'full']
    if not full_groups:
        return []
    return sorted(full_groups, key=lambda group: -1 if _model_skin_index(group[0]) is None else _model_skin_index(group[0]))[0]


def select_model_group(models: Sequence[Mapping[str, Any]], runtime_keys: Sequence[str]) -> list[Mapping[str, Any]]:
    if not models:
        return []

    runtime_key_set = set(runtime_keys)
    groups = [group for group in _group_models_by_skin_and_kind(models) if group and _model_kind(group[0]) == 'full']
    candidates = [
        group
        for group in groups
        if all(runtime_key and any(_model_live2d_key(model) == runtime_key for model in group) for runtime_key in runtime_keys)
    ]
    if len(candidates) == 1:
        return candidates[0]

    exact = []
    for group in groups:
        group_keys = {_model_live2d_key(model) for model in group if _model_live2d_key(model)}
        if group_keys and group_keys == runtime_key_set:
            exact.append(group)
    if len(exact) == 1:
        return exact[0]

    return list(models)


def _normalize_runtime_containers(
    containers: Sequence[Mapping[str, Any] | RuntimeContainerSnapshot],
) -> list[RuntimeContainerSnapshot]:
    out: list[RuntimeContainerSnapshot] = []
    for index, container in enumerate(containers):
        if isinstance(container, RuntimeContainerSnapshot):
            out.append(container)
        elif isinstance(container, Mapping):
            out.append(RuntimeContainerSnapshot.from_raw(container, fallback_index=index))
    return out


def _normalize_runtime_players(players: Sequence[Mapping[str, Any] | RuntimePlayerSnapshot]) -> list[RuntimePlayerSnapshot]:
    out: list[RuntimePlayerSnapshot] = []
    for index, player in enumerate(players):
        if isinstance(player, RuntimePlayerSnapshot):
            out.append(player)
        elif isinstance(player, Mapping):
            out.append(RuntimePlayerSnapshot.from_raw(player, fallback_index=index))
    return out


def _flatten_resource_urls(urls: Mapping[str, Any]) -> set[str]:
    out: set[str] = set()
    for value in urls.values():
        if isinstance(value, str):
            normalized = _normalize_url(value)
            if normalized:
                out.add(normalized)
        elif isinstance(value, Sequence) and not isinstance(value, str | bytes | bytearray):
            out.update(_normalize_url(item) for item in value if isinstance(item, str) and item.strip())
    return out


def _runtime_player_matches_model(player: RuntimePlayerSnapshot, model: Mapping[str, Any] | None) -> bool:
    if model is None:
        return False

    player_key = player.runtime_key()
    model_key = _model_live2d_key(model)
    if player_key and model_key:
        return player_key == model_key

    model_urls = _flatten_resource_urls(_model_resource_urls(model))
    player_urls = {url for url in (player.atlas_url, player.skel_url) if url}
    return bool(model_urls.intersection(player_urls))


def _track_zero_animation_events(player: RuntimePlayerSnapshot) -> list[RuntimeAnimationEvent]:
    return [event for event in player.events if event.animation and event.event_type == 'animationState' and event.track_index in {None, 0}]


def _runtime_event_state(
    *,
    event: RuntimeAnimationEvent,
    match_method: str,
    default_loop: bool,
    duration_ms: int | None = None,
) -> dict[str, Any]:
    state: dict[str, Any] = {
        'animation': event.animation,
        'enabled': True,
        'loop': event.loop if event.loop is not None else default_loop,
        'match_method': match_method,
        'match_confidence': 'high',
    }
    if duration_ms is not None:
        state['duration_ms'] = duration_ms
    return state


def _runtime_animations_from_player(
    player: RuntimePlayerSnapshot,
    *,
    click_start_sequence: int | None,
) -> dict[str, dict[str, Any]]:
    events = _track_zero_animation_events(player)
    idle_candidates = [event for event in events if click_start_sequence is None or event.sequence <= click_start_sequence]
    idle_event = max(idle_candidates, key=lambda event: event.sequence, default=None)
    click_event = (
        min((event for event in events if event.sequence > click_start_sequence), key=lambda event: event.sequence, default=None)
        if click_start_sequence is not None
        else None
    )

    runtime_animations: dict[str, dict[str, Any]] = {}
    if idle_event is not None:
        runtime_animations['idle'] = _runtime_event_state(
            event=idle_event,
            match_method=RUNTIME_ANIMATION_MATCH_METHOD,
            default_loop=True,
        )
    elif player.configured_animation:
        runtime_animations['idle'] = {
            'animation': player.configured_animation,
            'enabled': True,
            'loop': True,
            'match_method': RUNTIME_ANIMATION_MATCH_METHOD,
            'match_confidence': 'high',
        }

    if click_event is not None:
        runtime_animations['click'] = _runtime_event_state(
            event=click_event,
            match_method=RUNTIME_CLICK_MATCH_METHOD,
            default_loop=False,
            duration_ms=player.animation_durations_ms.get(click_event.animation),
        )
    elif click_start_sequence is not None:
        runtime_animations['click'] = {
            'enabled': False,
            'match_method': RUNTIME_CLICK_MATCH_METHOD,
            'match_confidence': 'high',
        }

    return runtime_animations


def _runtime_layer_metadata(
    *,
    model: Mapping[str, Any] | None,
    runtime_player: RuntimePlayerSnapshot | None,
    click_start_sequence: int | None,
) -> dict[str, Any]:
    if model is None:
        return {}

    if runtime_player is not None:
        runtime_animations = _runtime_animations_from_player(runtime_player, click_start_sequence=click_start_sequence)
        runtime_animation_match_method = RUNTIME_ANIMATION_MATCH_METHOD
        runtime_animation_match_confidence = (
            'high' if runtime_animations and _runtime_player_matches_model(runtime_player, model) else 'low'
        )
        metadata: dict[str, Any] = {'runtime_player': runtime_player.to_payload()}
    else:
        runtime_animations = _model_runtime_animations(model)
        runtime_animation_match_method = _first_text(model, 'runtime_animation_match_method', 'runtimeAnimationMatchMethod')
        runtime_animation_match_confidence = _first_text(
            model,
            'runtime_animation_match_confidence',
            'runtimeAnimationMatchConfidence',
        )
        metadata = {}

    if runtime_animations:
        metadata['runtime_animations'] = runtime_animations
    if runtime_animation_match_method:
        metadata['runtime_animation_match_method'] = runtime_animation_match_method
    if runtime_animation_match_confidence in {'high', 'medium', 'low'}:
        metadata['runtime_animation_match_confidence'] = runtime_animation_match_confidence
    return metadata


def _build_multi_group_layer_capture_payload(
    *,
    request: RuntimeCaptureRequest,
    group_captures: Sequence[RuntimeGroupCapture],
) -> dict[str, Any]:
    captured_at = _utc_now_iso()
    source_url = request.source_url or f'{GAMEKEE_BASE_URL}/nikke/tj/{request.content_id}.html'
    layers: list[dict[str, Any]] = []
    warnings: list[str] = []
    requested_live2d_keys: list[str] = []
    matched_live2d_keys: list[str] = []
    captured_models: list[Mapping[str, Any]] = []
    container_count = 0

    for group_capture in group_captures:
        payload = build_layer_capture_payload(
            LayerCaptureBuildInput(
                content_id=request.content_id,
                title=request.title,
                models=group_capture.models,
                containers=group_capture.containers,
                runtime_players=group_capture.runtime_players,
                runtime_click_start_sequence=group_capture.runtime_click_start_sequence,
                requested_live2d_keys=group_capture.requested_live2d_keys,
                captured_at=captured_at,
                source_url=source_url,
            ),
        )
        layers.extend(payload['layers'])
        warnings.extend(str(warning) for warning in payload.get('warnings', []))
        runtime = payload.get('runtime')
        if isinstance(runtime, Mapping):
            requested_live2d_keys.extend(str(key) for key in runtime.get('requested_live2d_keys', []) if key)
            matched_live2d_keys.extend(str(key) for key in runtime.get('matched_live2d_keys', []) if key)
            container_count += _to_int(runtime.get('container_count')) or 0
        captured_models.extend(group_capture.models)

    return {
        'content_id': request.content_id,
        'source_url': source_url,
        'title': request.title,
        'captured_at': captured_at,
        'status': 'success',
        'layer_capture_schema': LAYER_CAPTURE_SCHEMA,
        'runtime': {
            'requested_live2d_keys': _unique_in_order(requested_live2d_keys),
            'matched_live2d_keys': _unique_in_order(matched_live2d_keys),
            'container_count': container_count,
        },
        'fingerprint': layer_capture_fingerprint(request.content_id, captured_models),
        'warnings': _unique_in_order(warnings),
        'layers': layers,
    }


def build_layer_capture_payload(capture: LayerCaptureBuildInput) -> dict[str, Any]:
    captured_at = capture.captured_at or _utc_now_iso()
    source_url = capture.source_url or f'{GAMEKEE_BASE_URL}/nikke/tj/{capture.content_id}.html'
    containers = _normalize_runtime_containers(capture.containers)
    runtime_players = _normalize_runtime_players(capture.runtime_players)
    runtime_player_by_container_index = {player.container_index: player for player in runtime_players if player.container_index >= 0}
    requested_live2d_keys = _unique_in_order(capture.requested_live2d_keys)
    model_keys = {_model_live2d_key(model) for model in capture.models if _model_live2d_key(model)}
    runtime_model_keys = [key for key in requested_live2d_keys if key in model_keys]
    model_group = select_model_group(capture.models, runtime_model_keys)
    model_by_runtime_key = {_model_live2d_key(model): model for model in model_group if _model_live2d_key(model)}
    matched_runtime_keys = [key for key in requested_live2d_keys if key in model_by_runtime_key]
    warnings: list[str] = []
    if len(matched_runtime_keys) < len(containers):
        warnings.append('fewer runtime live2d keys than visible containers')
    if len(containers) != len(model_group):
        warnings.append(f'visible container count {len(containers)} differs from full model count {len(model_group)}')

    layers: list[dict[str, Any]] = []
    for source_layer_index, container in enumerate(containers):
        runtime_key = matched_runtime_keys[source_layer_index] if source_layer_index < len(matched_runtime_keys) else ''
        model = model_by_runtime_key.get(runtime_key)
        if model is None and source_layer_index < len(model_group):
            model = model_group[source_layer_index]
        live2d_key = _model_live2d_key(model) if model is not None else runtime_key
        z_index = container.z_index
        has_high_confidence_match = model is not None and bool(runtime_key) and live2d_key == runtime_key and z_index is not None
        layer: dict[str, Any] = {
            'content_id': capture.content_id,
            'skin_index': _model_skin_index(model) if model is not None else None,
            'stable_id': _model_stable_id(model) if model is not None else '',
            'live2d_key': live2d_key,
            'resource_urls': _model_resource_urls(model) if model is not None else {},
            'source_layer_index': source_layer_index,
            'source_z_index': z_index,
            'is_primary': source_layer_index == 0,
            'layer_match_method': LAYER_CAPTURE_MATCH_METHOD,
            'layer_match_confidence': 'high' if has_high_confidence_match else 'low',
            'captured_at': captured_at,
            'raw_container': container.to_raw_container(),
        }
        layer.update(
            _runtime_layer_metadata(
                model=model,
                runtime_player=runtime_player_by_container_index.get(container.index),
                click_start_sequence=capture.runtime_click_start_sequence,
            ),
        )
        layers.append(layer)

    ordered_layers = sorted(
        (layer for layer in layers if _to_int(layer.get('source_z_index')) is not None),
        key=lambda layer: (_to_int(layer.get('source_z_index')) or 0, _to_int(layer.get('source_layer_index')) or 0),
    )
    for layer_order, layer in enumerate(ordered_layers, start=1):
        layer['layer_order'] = layer_order

    return {
        'content_id': capture.content_id,
        'source_url': source_url,
        'title': capture.title,
        'captured_at': captured_at,
        'status': 'success',
        'layer_capture_schema': LAYER_CAPTURE_SCHEMA,
        'runtime': {
            'requested_live2d_keys': requested_live2d_keys,
            'matched_live2d_keys': matched_runtime_keys,
            'container_count': len(containers),
        },
        'fingerprint': layer_capture_fingerprint(capture.content_id, model_group),
        'warnings': warnings,
        'layers': layers,
    }


async def _select_runtime_skin(page: Any, skin_title: str) -> None:
    if not skin_title or skin_title == '基础时装':
        return

    await page.locator('button.action-item[data-report-key*="clothes"]').click(timeout=10_000)
    await page.get_by_text(skin_title, exact=True).last.click(timeout=10_000)
    await page.wait_for_timeout(500)


async def _read_runtime_containers(page: Any) -> list[RuntimeContainerSnapshot]:
    raw_containers = await page.evaluate(
        """() => [...document.querySelectorAll(".spine-player-container")].map((element, index) => {
            const rect = element.getBoundingClientRect();
            const style = getComputedStyle(element);
            return {
                index,
                zIndex: Number.parseInt(style.zIndex, 10),
                rawZIndex: style.zIndex,
                className: element.className,
                parentClassName: element.parentElement?.className ?? "",
                canvasCount: element.querySelectorAll("canvas").length,
                rect: {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                },
            };
        })""",
    )
    if not isinstance(raw_containers, list):
        raw_containers = []
    return [
        RuntimeContainerSnapshot.from_raw(item, fallback_index=index)
        for index, item in enumerate(raw_containers)
        if isinstance(item, Mapping)
    ]


async def _runtime_event_count(page: Any) -> int:
    count = await page.evaluate('() => window.__gvRuntimeCapture?.eventCount?.() ?? 0')
    return _to_int(count) or 0


async def _read_runtime_players(page: Any) -> list[RuntimePlayerSnapshot]:
    raw_players = await page.evaluate('() => window.__gvRuntimeCapture?.snapshot?.() ?? []')
    if not isinstance(raw_players, list):
        raw_players = []
    return [
        RuntimePlayerSnapshot.from_raw(item, fallback_index=index) for index, item in enumerate(raw_players) if isinstance(item, Mapping)
    ]


async def _click_runtime_stage(page: Any) -> None:
    point = await page.evaluate(
        """() => {
            const element = document.querySelector(".live2d-stage")
                || document.querySelector(".live2d-container")
                || document.querySelector(".spine-player-container");
            if (!element) return null;
            const rect = element.getBoundingClientRect();
            return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
        }""",
    )
    if not isinstance(point, Mapping):
        return
    x = _to_float(point.get('x'))
    y = _to_float(point.get('y'))
    if x is None or y is None:
        return
    await page.mouse.click(x, y)


async def _capture_runtime_containers(request: RuntimeCaptureRequest) -> tuple[list[RuntimeContainerSnapshot], list[str]]:
    captures = await _capture_runtime_groups(request)
    if not captures:
        return [], []
    first_capture = captures[0]
    return list(first_capture.containers), list(first_capture.requested_live2d_keys)


async def _capture_runtime_groups(request: RuntimeCaptureRequest) -> list[RuntimeGroupCapture]:
    try:
        from playwright.async_api import async_playwright  # noqa: PLC0415
    except ImportError as exc:
        msg = 'Install Playwright and a Chromium browser before running Nikke runtime capture.'
        raise RuntimeCaptureDependencyError(msg) from exc

    requested_urls: list[str] = []
    source_url = request.source_url or f'{GAMEKEE_BASE_URL}/nikke/tj/{request.content_id}.html'
    model_groups = multi_full_model_groups(request.models) or [first_skin_model_group(request.models)]
    model_groups = [group for group in model_groups if group]

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch(headless=request.headless)
        try:
            page = await browser.new_page(viewport=DEFAULT_RUNTIME_VIEWPORT)
            await page.add_init_script(GAMEKEE_RUNTIME_CAPTURE_SCRIPT)
            page.on('request', lambda browser_request: requested_urls.append(browser_request.url))
            await page.goto(source_url, wait_until='domcontentloaded', timeout=request.timeout_ms)
            captures: list[RuntimeGroupCapture] = []
            for group_index, group in enumerate(model_groups):
                skin_title = _model_skin_title(group[0])
                is_initial_group = group_index == 0 and (not skin_title or skin_title == '基础时装')
                start_request_index = 0 if is_initial_group else len(requested_urls)
                await _select_runtime_skin(page, skin_title)
                expected_layer_count = request.expected_layer_count if request.expected_layer_count is not None else len(group)
                await page.wait_for_function(
                    'count => document.querySelectorAll(".spine-player-container canvas").length >= count',
                    arg=expected_layer_count,
                    timeout=request.timeout_ms,
                )
                await page.wait_for_timeout(1500)
                click_start_sequence = await _runtime_event_count(page)
                await _click_runtime_stage(page)
                await page.wait_for_timeout(1500)
                captures.append(
                    RuntimeGroupCapture(
                        models=group,
                        containers=await _read_runtime_containers(page),
                        requested_live2d_keys=unique_live2d_keys(requested_urls[start_request_index:]),
                        runtime_players=await _read_runtime_players(page),
                        runtime_click_start_sequence=click_start_sequence,
                    ),
                )
        finally:
            await browser.close()

    return captures


async def capture_gamekee_runtime_layers(request: RuntimeCaptureRequest) -> dict[str, Any]:
    group_captures = await _capture_runtime_groups(request)
    if len(group_captures) > 1:
        return _build_multi_group_layer_capture_payload(request=request, group_captures=group_captures)
    containers = list(group_captures[0].containers) if group_captures else []
    requested_live2d_keys = list(group_captures[0].requested_live2d_keys) if group_captures else []
    runtime_players = list(group_captures[0].runtime_players) if group_captures else []
    runtime_click_start_sequence = group_captures[0].runtime_click_start_sequence if group_captures else None
    models = group_captures[0].models if group_captures else request.models
    return build_layer_capture_payload(
        LayerCaptureBuildInput(
            content_id=request.content_id,
            title=request.title,
            models=models,
            containers=containers,
            runtime_players=runtime_players,
            runtime_click_start_sequence=runtime_click_start_sequence,
            requested_live2d_keys=requested_live2d_keys,
            source_url=request.source_url,
        ),
    )

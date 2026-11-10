"""`src/web/nikke_runtime.py` (WP17): the runtime layer capture's pure parts.

- `helpers.json`: `_to_int`, `_to_float`, `live2d_key_from_url`, `unique_live2d_keys`,
  the snapshots' `from_raw` (written back through `to_raw_container` / `to_payload`), and
  the skin groups (`multi_full_model_groups`, `first_skin_model_group`,
  `select_model_group`).
- `payloads.json`: `build_layer_capture_payload` over many inputs, and
  `capture_gamekee_runtime_layers` with `_capture_runtime_groups` replaced by the
  scenario's group captures (none, one, several). `_utc_now_iso` is pinned, so the clock
  never reaches a golden.
- `launch_argv.json`: the command line `playwright.chromium.launch(headless=...)` gives
  Chromium, captured by pointing `executable_path` at a script that records its arguments
  and exits (as `browser_login.py` does for `launch_persistent_context`). The throwaway
  profile Playwright creates is written as `<profile>`.
"""

from __future__ import annotations

import asyncio
import contextlib
import re
import stat
import tempfile
from pathlib import Path
from typing import Any

from src.web import nikke_runtime as runtime

_NOW = '2026-11-10T08:00:00.123456+00:00'

_INT_VALUES: list[Any] = [
    None,
    True,
    False,
    0,
    7,
    -3,
    7.0,
    7.5,
    -0.0,
    1e18,
    '12',
    ' 12px ',
    '+5',
    '-5abc',
    'auto',
    '',
    '٣',
    '²',
    '--1',
    '1.9',
    [],
    {},
]
_FLOAT_VALUES: list[Any] = [None, True, 3, -2, 2.5, '2.5', ' 1e3 ', 'inf', 'nan', 'x', '', [], {}, '1_0', '１２']


def _model(skin_index: Any, stable_id: str, live2d_key: str, **extra: Any) -> dict[str, Any]:
    model = {
        'label': 'live2d(full)',
        'skin_index': skin_index,
        'stable_id': stable_id,
        'live2d_key': live2d_key,
        'urls': {
            'atlas': f'https://cdn.example.com/live2d/{live2d_key}/model.atlas',
            'skel': f'https://cdn.example.com/live2d/{live2d_key}/model.skel',
        },
    }
    model.update(extra)
    return model


def _runtime_animations(prefix: str) -> dict[str, Any]:
    return {
        'idle': {'animation': f'{prefix}-idle', 'enabled': True, 'loop': True, 'match_method': 'gamekee-runtime-player', 'match_confidence': 'high'},
        'click': {
            'animation': f'{prefix}-click',
            'enabled': True,
            'loop': False,
            'match_method': 'gamekee-runtime-player-event',
            'match_confidence': 'high',
            'duration_ms': 1200,
        },
    }


_CONTAINERS: list[dict[str, Any]] = [
    {},
    {'index': 3, 'zIndex': 9, 'rawZIndex': '9', 'className': 'spine-player-container', 'parentClassName': ' p ', 'canvasCount': 2},
    {'z_index': None, 'zIndex': 4, 'raw_z_index': '7', 'rawZIndex': '8'},
    {'zIndex': 'auto', 'rawZIndex': 'auto', 'canvas_count': '3', 'rect': {'x': 1, 'y': 2.5, 'width': 'wide', 'height': '40'}},
    {'zIndex': None, 'rawZIndex': '12', 'rect': [1, 2]},
    {'index': '2', 'zIndex': 1.0, 'class_name': 'c', 'className': 'ignored', 'canvasCount': 0, 'rect': {'height': 3, 'x': 0}},
    {'index': None, 'zIndex': True, 'rawZIndex': 5},
]

_PLAYERS: list[dict[str, Any]] = [
    {},
    {
        'playerIndex': 0,
        'containerIndex': 0,
        'zIndex': 9,
        'rawZIndex': '9',
        'atlasUrl': '//cdn.example.com/live2d/nikke/main/model.atlas',
        'skelUrl': '/live2d/nikke/main/model.skel',
        'configuredAnimation': 'idle',
        'currentAnimation': 'idle',
        'animationDurationsMs': {'action': 1200, 'bad': -1, 'idle': '300', 'x': None},
        'events': [
            {'sequence': 1, 'timeMs': 10, 'type': 'animationState', 'method': 'setAnimation', 'trackIndex': 0, 'animation': 'idle', 'loop': True},
            {'seq': '2', 'type': 'playerMethod', 'method': 'play', 'playerIndex': 0, 'delayMs': 250, 'loop': 1},
            'not an event',
        ],
    },
    {'player_index': 2, 'container_index': 5, 'atlas_url': 'https://x/live2d/g/k/a.atlas', 'animation': 'fallback', 'current': 'now', 'events': 'nope'},
    {'playerIndex': 4, 'containerIndex': -1, 'skelUrl': 'https://x/other/model.skel', 'events': [{}]},
]


def _model_groups() -> list[list[dict[str, Any]]]:
    return [
        [],
        [
            _model(0, 'stable-main', 'main'),
            _model(0, 'stable-front', 'front'),
            _model(0, 'stable-aim', 'aim', label='live2d(aim)'),
            _model(0, 'stable-cover', 'cover', label='live2d(cover)'),
        ],
        [
            _model(0, 'base-main', 'base-main'),
            _model(3, 'layer-main', 'layer-main', skin_title='Layered'),
            _model(3, 'layer-back', 'layer-back', skin_title='Layered'),
            _model(None, 'none-a', 'none-a'),
            _model(None, 'none-b', 'none-b'),
            _model(1, 'one-a', 'one-a', skinIndex=9),
            _model(1, 'one-b', 'one-b'),
        ],
        [_model(2, 'aim-only', 'aim-only', label='aim')],
        [_model('2', 's2', 'shared'), _model(2, 's2b', 'shared'), _model(5, 's5', 'shared')],
    ]


_RUNTIME_KEY_SETS: list[list[str]] = [[], ['main'], ['main', 'front'], ['layer-main', 'layer-back'], ['shared'], ['', 'main'], ['nope']]


def _helpers() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    cases.extend({'fn': 'to_int', 'value': value, 'result': runtime._to_int(value)} for value in _INT_VALUES)  # noqa: SLF001
    cases.extend({'fn': 'to_float', 'value': value, 'result': runtime._to_float(value)} for value in _FLOAT_VALUES)  # noqa: SLF001
    urls = [
        'https://cdn.gamekee.com/live2d/nikke/main/model.atlas?version=1',
        'https://cdn.gamekee.com/live2d/nikke/front/model.skel',
        'https://cdn.gamekee.com/live2d/nikke/main/model.png',
        'https://cdn.example.com/not-live2d/model.png',
        'https://x/live2d/a/b',
        'https://x/live2d/a/b?/c/',
        'https://x/live2d//b/',
        '/live2d/g/k/x/live2d/h/j/',
    ]
    cases.extend({'fn': 'live2d_key_from_url', 'url': url, 'result': runtime.live2d_key_from_url(url)} for url in urls)
    cases.append({'fn': 'unique_live2d_keys', 'urls': urls, 'result': runtime.unique_live2d_keys(urls)})
    for fallback in (0, 4):
        for raw in _CONTAINERS:
            snapshot = runtime.RuntimeContainerSnapshot.from_raw(raw, fallback_index=fallback)
            cases.append({'fn': 'container_from_raw', 'raw': raw, 'fallback': fallback, 'result': snapshot.to_raw_container()})
        for raw in _PLAYERS:
            player = runtime.RuntimePlayerSnapshot.from_raw(raw, fallback_index=fallback)
            cases.append(
                {
                    'fn': 'player_from_raw',
                    'raw': raw,
                    'fallback': fallback,
                    'result': player.to_payload(),
                    'durations': player.animation_durations_ms,
                    'events': [event.to_payload() for event in player.events],
                }
            )
    for models in _model_groups():
        cases.append(
            {
                'fn': 'groups',
                'models': models,
                'multi': [[model['stable_id'] for model in group] for group in runtime.multi_full_model_groups(models)],
                'first': [model['stable_id'] for model in runtime.first_skin_model_group(models)],
                'select': [
                    {'keys': keys, 'result': [model['stable_id'] for model in runtime.select_model_group(models, keys)]}
                    for keys in _RUNTIME_KEY_SETS
                ],
            }
        )
    return cases


def _event(sequence: Any, animation: str, *, loop: Any = True, track: Any = 0, kind: str = 'animationState', **extra: Any) -> dict[str, Any]:
    return {'sequence': sequence, 'type': kind, 'method': 'setAnimationWith', 'trackIndex': track, 'animation': animation, 'loop': loop, **extra}


def _player(index: int, key: str, events: list[dict[str, Any]], **extra: Any) -> dict[str, Any]:
    return {
        'playerIndex': index,
        'containerIndex': index,
        'zIndex': 9 - index,
        'rawZIndex': str(9 - index),
        'atlasUrl': f'https://cdn.example.com/live2d/nikke/{key}/model.atlas',
        'skelUrl': f'https://cdn.example.com/live2d/nikke/{key}/model.skel',
        'configuredAnimation': 'idle',
        'events': events,
        **extra,
    }


def _build_inputs() -> list[dict[str, Any]]:
    three = [_model(1, 'stable-main', 'main'), _model(1, 'stable-front', 'front'), _model(1, 'stable-back', 'back')]
    others = [_model(0, 'other-main', 'other-main'), _model(0, 'other-front', 'other-front')]
    z_containers = [
        {'index': 0, 'zIndex': 9, 'rawZIndex': '9', 'canvasCount': 1},
        {'index': 1, 'zIndex': '10', 'rawZIndex': '10', 'canvasCount': 1},
        {'index': 2, 'zIndex': 8, 'rawZIndex': '8', 'canvasCount': 1},
    ]
    return [
        {'content_id': 711133, 'title': 'Test', 'models': [*others, *three], 'containers': z_containers, 'requested_live2d_keys': ['main', 'front', 'back']},
        {'content_id': 1, 'models': [], 'containers': [], 'requested_live2d_keys': []},
        {'content_id': 2, 'models': three, 'containers': [{'index': 0, 'zIndex': 5}], 'requested_live2d_keys': [' main ', 'main', '', 'zzz']},
        {
            'content_id': 3,
            'models': three[:2],
            'containers': [{'index': 0, 'zIndex': 5, 'rawZIndex': '5'}, {'index': 1, 'zIndex': 'auto', 'rawZIndex': 'auto'}],
            'requested_live2d_keys': ['main'],
        },
        {
            'content_id': 4,
            'models': [
                {**_model(0, 'stable-main', 'main'), 'runtime_animations': _runtime_animations('main'), 'runtime_animation_match_method': 'm', 'runtime_animation_match_confidence': 'high'},
                {
                    **_model(0, 'stable-front', 'front'),
                    'runtimeAnimations': {'idle': {'animation': ' i ', 'enabled': 1, 'loop': False, 'matchConfidence': 'odd', 'durationMs': '5'}, 'click': {}},
                    'runtimeAnimationMatchConfidence': 'medium',
                },
            ],
            'containers': [{'index': 0, 'zIndex': 9}, {'index': 1, 'zIndex': 8}],
            'requested_live2d_keys': ['main', 'front'],
            'source_url': 'https://example.test/page',
        },
        {
            'content_id': 5,
            'models': three,
            'containers': z_containers,
            'runtime_players': [
                _player(0, 'main', [_event(1, 'idle'), _event(5, 'action', loop=False)], animationDurationsMs={'action': 1333}),
                _player(1, 'front', [_event(2, 'bg_idle'), _event(6, 'action', loop=None, delayMs=250)], animationDurationsMs={'action': 900}),
                _player(2, 'back', [_event(3, 'bg_idle')]),
            ],
            'runtime_click_start_sequence': 4,
            'requested_live2d_keys': ['main', 'front', 'back'],
        },
        {
            'content_id': 6,
            'models': [_model(0, 'stable-main', 'main')],
            'containers': [{'index': 0, 'zIndex': 9}],
            'runtime_players': [_player(0, 'other', [_event(1, 'idle')])],
            'runtime_click_start_sequence': 2,
            'requested_live2d_keys': ['main'],
        },
        {
            'content_id': 7,
            'models': [_model(0, 'stable-main', 'main'), {'stable_id': 'bare', 'resource_urls': {'atlas': '/live2d/nikke/x/m.atlas', 'list': ['//a/b', 3, ' ']}}],
            'containers': [{'index': 0, 'zIndex': 1}, {'index': 1, 'zIndex': 1}, {'index': 2}],
            'runtime_players': [
                _player(0, 'main', [_event(3, 'a'), _event(3, 'b'), _event(1, 'c', track=1), _event(2, 'd', kind='playerMethod'), _event(4, '')]),
                {**_player(1, 'x', []), 'atlasUrl': 'https://h/no-key/model.atlas', 'skelUrl': '/live2d/nikke/x/m.atlas', 'configuredAnimation': ''},
                {**_player(1, 'dup', [_event(9, 'late')]), 'containerIndex': 1},
            ],
            'runtime_click_start_sequence': None,
            'requested_live2d_keys': [],
        },
        {
            'content_id': 8,
            'models': [_model(0, 'm', 'main')],
            'containers': [{'index': 0, 'zIndex': 2}],
            'runtime_players': [_player(0, 'main', [_event(7, 'after', loop=None), _event(6, 'first-after')], animationDurationsMs={'after': 10})],
            'runtime_click_start_sequence': 0,
            'requested_live2d_keys': ['main'],
            'captured_at': '2020-01-01T00:00:00+00:00',
        },
    ]


def _build(case: dict[str, Any]) -> dict[str, Any]:
    return runtime.build_layer_capture_payload(
        runtime.LayerCaptureBuildInput(
            content_id=case['content_id'],
            title=case.get('title', ''),
            models=case['models'],
            containers=case['containers'],
            runtime_players=case.get('runtime_players', []),
            runtime_click_start_sequence=case.get('runtime_click_start_sequence'),
            requested_live2d_keys=case['requested_live2d_keys'],
            captured_at=case.get('captured_at', ''),
            source_url=case.get('source_url', ''),
        )
    )


def _capture_scenarios() -> list[dict[str, Any]]:
    first = [_model(0, 'first-main', 'first-main'), _model(0, 'first-back', 'first-back')]
    second = [_model(1, 'second-main', 'second-main'), _model(1, 'second-back', 'second-back')]
    return [
        {'request': {'content_id': 711133, 'models': [*first, *second], 'title': 'T'}, 'groups': []},
        {
            'request': {'content_id': 711133, 'models': first, 'source_url': 'https://example.test/p'},
            'groups': [
                {
                    'models': first,
                    'containers': [{'index': 0, 'zIndex': 9, 'rawZIndex': '9'}, {'index': 1, 'zIndex': 8, 'rawZIndex': '8'}],
                    'requested_live2d_keys': ['first-main', 'first-back'],
                    'runtime_click_start_sequence': 3,
                    'runtime_players': [
                        _player(0, 'first-main', [_event(1, 'idle'), _event(4, 'action', loop=False)], animationDurationsMs={'action': 1200}),
                        _player(1, 'first-back', [_event(2, 'bg_idle')]),
                    ],
                }
            ],
        },
        {
            'request': {'content_id': 711133, 'models': [*first, *second], 'title': 'Two'},
            'groups': [
                {
                    'models': first,
                    'containers': [{'index': 0, 'zIndex': 9, 'rawZIndex': '9'}, {'index': 1, 'zIndex': 8, 'rawZIndex': '8'}],
                    'requested_live2d_keys': ['first-main', 'first-back'],
                },
                {
                    'models': second,
                    'containers': [{'index': 0, 'zIndex': 7, 'rawZIndex': '7'}],
                    'requested_live2d_keys': ['second-main', 'first-main', ''],
                    'runtime_click_start_sequence': 0,
                },
            ],
        },
    ]


def _capture(scenario: dict[str, Any]) -> dict[str, Any]:
    async def fake_groups(_request: runtime.RuntimeCaptureRequest) -> list[runtime.RuntimeGroupCapture]:
        return [
            runtime.RuntimeGroupCapture(
                models=group['models'],
                containers=[
                    runtime.RuntimeContainerSnapshot.from_raw(item, fallback_index=index) for index, item in enumerate(group['containers'])
                ],
                requested_live2d_keys=group['requested_live2d_keys'],
                runtime_players=[
                    runtime.RuntimePlayerSnapshot.from_raw(item, fallback_index=index)
                    for index, item in enumerate(group.get('runtime_players', []))
                ],
                runtime_click_start_sequence=group.get('runtime_click_start_sequence'),
            )
            for group in scenario['groups']
        ]

    original = runtime._capture_runtime_groups  # noqa: SLF001
    runtime._capture_runtime_groups = fake_groups  # noqa: SLF001
    try:
        return asyncio.run(runtime.capture_gamekee_runtime_layers(runtime.RuntimeCaptureRequest(**scenario['request'])))
    finally:
        runtime._capture_runtime_groups = original  # noqa: SLF001


def _payloads() -> list[dict[str, Any]]:
    original = runtime._utc_now_iso  # noqa: SLF001
    runtime._utc_now_iso = lambda: _NOW  # noqa: SLF001
    try:
        cases: list[dict[str, Any]] = [{'fn': 'build', 'input': case, 'result': _build(case)} for case in _build_inputs()]
        cases.extend({'fn': 'capture', 'scenario': scenario, 'result': _capture(scenario)} for scenario in _capture_scenarios())
    finally:
        runtime._utc_now_iso = original  # noqa: SLF001
    return [{'now': _NOW, **case} for case in cases]


_RECORDER = """#!/bin/sh
for arg in "$@"; do printf '%s\\0' "$arg"; done > "$(dirname "$0")/argv"
exit 1
"""


async def _capture_launch(headless: bool, workdir: Path) -> list[str]:  # noqa: FBT001
    from playwright.async_api import async_playwright  # noqa: PLC0415 - only this golden needs it

    recorder = workdir / 'chrome'
    recorder.write_text(_RECORDER, encoding='utf-8')
    recorder.chmod(recorder.stat().st_mode | stat.S_IXUSR)
    (workdir / 'argv').unlink(missing_ok=True)
    async with async_playwright() as playwright:
        # The recorder always exits 1; the arguments are what matter.
        with contextlib.suppress(Exception):
            await playwright.chromium.launch(executable_path=str(recorder), headless=headless, timeout=10_000)
    argv = (workdir / 'argv').read_bytes().split(b'\0')[:-1]
    return [re.sub(r'^--user-data-dir=.*playwright_chromiumdev_profile-[^/]*$', '--user-data-dir=<profile>', arg.decode()) for arg in argv]


def _launch_argv() -> list[dict[str, Any]]:
    with tempfile.TemporaryDirectory() as directory:
        return [{'headless': headless, 'argv': asyncio.run(_capture_launch(headless, Path(directory)))} for headless in (True, False)]


GOLDENS = {
    'nikke_runtime/helpers.json': _helpers,
    'nikke_runtime/payloads.json': _payloads,
    'nikke_runtime/launch_argv.json': _launch_argv,
}

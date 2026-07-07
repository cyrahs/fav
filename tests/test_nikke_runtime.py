# ruff: noqa: INP001, S101, PLR2004

import asyncio
from pathlib import Path
from typing import Any, Self

import playwright.async_api as playwright_async_api
import pytest

import src.web.nikke_runtime as runtime_module
from src.web.nikke_layer_metadata import live2d_layer_fingerprint, merge_live2d_layer_captures
from src.web.nikke_runtime import (
    LayerCaptureBuildInput,
    RuntimeCaptureRequest,
    RuntimeGroupCapture,
    _capture_runtime_groups,
    build_layer_capture_payload,
    first_skin_model_group,
    layer_capture_fingerprint,
    live2d_key_from_url,
    multi_full_model_groups,
    unique_live2d_keys,
)


def _model(*, skin_index: int, stable_id: str, live2d_key: str) -> dict[str, object]:
    return {
        'label': 'live2d(full)',
        'skin_index': skin_index,
        'stable_id': stable_id,
        'live2d_key': live2d_key,
        'urls': {
            'atlas': f'https://cdn.example.com/live2d/{live2d_key}/model.atlas',
            'skel': f'https://cdn.example.com/live2d/{live2d_key}/model.skel',
        },
    }


def test_live2d_key_from_url_extracts_runtime_resource_key() -> None:
    urls = [
        'https://cdn.gamekee.com/live2d/nikke/main/model.atlas?version=1',
        'https://cdn.gamekee.com/live2d/nikke/front/model.skel',
        'https://cdn.gamekee.com/live2d/nikke/main/model.png',
        'https://cdn.example.com/not-live2d/model.png',
    ]

    assert live2d_key_from_url(urls[0]) == 'main'
    assert live2d_key_from_url(urls[-1]) == ''
    assert unique_live2d_keys(urls) == ['main', 'front']


def test_capture_runtime_containers_passes_wait_for_function_arg_by_keyword() -> None:
    source = (Path(__file__).resolve().parents[1] / 'src/web/nikke_runtime.py').read_text(encoding='utf-8')

    assert 'await page.wait_for_function(' in source
    assert 'arg=expected_layer_count' in source


def test_first_skin_model_group_ignores_aim_and_cover_models() -> None:
    models = [
        _model(skin_index=0, stable_id='stable-main', live2d_key='main'),
        _model(skin_index=0, stable_id='stable-front', live2d_key='front'),
        _model(skin_index=0, stable_id='stable-back', live2d_key='back'),
        {**_model(skin_index=0, stable_id='stable-aim', live2d_key='aim'), 'label': 'live2d(aim)'},
        {**_model(skin_index=0, stable_id='stable-cover', live2d_key='cover'), 'label': 'live2d(cover)'},
    ]

    assert [model['stable_id'] for model in first_skin_model_group(models)] == [
        'stable-main',
        'stable-front',
        'stable-back',
    ]


def test_multi_full_model_groups_selects_only_layered_full_skins() -> None:
    models = [
        _model(skin_index=0, stable_id='base-main', live2d_key='base-main'),
        {**_model(skin_index=3, stable_id='layer-main', live2d_key='layer-main'), 'skin_title': 'Layered'},
        {**_model(skin_index=3, stable_id='layer-back', live2d_key='layer-back'), 'skin_title': 'Layered'},
        {**_model(skin_index=3, stable_id='layer-aim', live2d_key='layer-aim'), 'label': 'live2d(aim)', 'skin_title': 'Layered'},
        {**_model(skin_index=3, stable_id='layer-cover', live2d_key='layer-cover'), 'label': 'live2d(cover)', 'skin_title': 'Layered'},
    ]

    groups = multi_full_model_groups(models)

    assert [[model['stable_id'] for model in group] for group in groups] == [['layer-main', 'layer-back']]


def test_capture_runtime_groups_switches_to_non_default_skin(monkeypatch: pytest.MonkeyPatch) -> None:  # noqa: C901
    class FakeRequest:
        def __init__(self, url: str) -> None:
            self.url = url

    class FakeClickTarget:
        def __init__(self, page: 'FakePage', action: str) -> None:
            self.page = page
            self.action = action

        @property
        def last(self) -> 'FakeClickTarget':
            return self

        async def click(self, **kwargs: Any) -> None:
            timeout = kwargs['timeout']
            self.page.clicks.append((self.action, timeout))
            if self.action.startswith('skin:'):
                for key in ('target-main', 'target-back'):
                    self.page.emit_request(f'https://live2d-img.gamekee.com/wiki2.0/live2d/1253/{key}/model.skel')

    class FakePage:
        def __init__(self) -> None:
            self.clicks: list[tuple[str, int]] = []
            self.wait_args: list[int] = []
            self.request_callback: Any = None

        def on(self, event: str, callback: Any) -> None:
            assert event == 'request'
            self.request_callback = callback

        def emit_request(self, url: str) -> None:
            self.request_callback(FakeRequest(url))

        async def goto(self, _url: str, **kwargs: Any) -> None:
            assert kwargs == {'wait_until': 'domcontentloaded', 'timeout': 60_000}
            self.emit_request('https://live2d-img.gamekee.com/wiki2.0/live2d/1253/base-main/model.skel')

        def locator(self, selector: str) -> FakeClickTarget:
            assert selector == 'button.action-item[data-report-key*="clothes"]'
            return FakeClickTarget(self, 'open-clothes')

        def get_by_text(self, text: str, *, exact: bool) -> FakeClickTarget:
            assert exact is True
            return FakeClickTarget(self, f'skin:{text}')

        async def wait_for_function(self, _expression: str, **kwargs: Any) -> None:
            arg = kwargs['arg']
            assert kwargs['timeout'] == 60_000
            self.wait_args.append(arg)

        async def wait_for_timeout(self, _timeout: int) -> None:
            return None

        async def evaluate(self, _expression: str) -> list[dict[str, Any]]:
            return [
                {'index': 0, 'zIndex': 9, 'rawZIndex': '9', 'canvasCount': 1},
                {'index': 1, 'zIndex': 8, 'rawZIndex': '8', 'canvasCount': 1},
            ]

    class FakeBrowser:
        def __init__(self, page: FakePage) -> None:
            self.page = page
            self.closed = False

        async def new_page(self, *, viewport: dict[str, int]) -> FakePage:
            assert viewport == {'width': 1440, 'height': 1000}
            return self.page

        async def close(self) -> None:
            self.closed = True

    class FakeChromium:
        def __init__(self, browser: FakeBrowser) -> None:
            self.browser = browser

        async def launch(self, *, headless: bool) -> FakeBrowser:
            assert headless is True
            return self.browser

    class FakePlaywright:
        def __init__(self, browser: FakeBrowser) -> None:
            self.chromium = FakeChromium(browser)

        async def __aenter__(self) -> Self:
            return self

        async def __aexit__(self, *_args: object) -> None:
            return None

    page = FakePage()
    browser = FakeBrowser(page)

    monkeypatch.setattr(playwright_async_api, 'async_playwright', lambda: FakePlaywright(browser))
    models = [
        {**_model(skin_index=0, stable_id='base-main', live2d_key='base-main'), 'skin_title': '基础时装'},
        {**_model(skin_index=3, stable_id='target-main', live2d_key='target-main'), 'skin_title': 'Target Skin'},
        {**_model(skin_index=3, stable_id='target-back', live2d_key='target-back'), 'skin_title': 'Target Skin'},
    ]

    captures = asyncio.run(_capture_runtime_groups(RuntimeCaptureRequest(content_id=711133, models=models)))

    assert browser.closed is True
    assert page.clicks == [('open-clothes', 10_000), ('skin:Target Skin', 10_000)]
    assert page.wait_args == [2]
    assert len(captures) == 1
    assert [model['stable_id'] for model in captures[0].models] == ['target-main', 'target-back']
    assert captures[0].requested_live2d_keys == ['target-main', 'target-back']


def test_capture_gamekee_runtime_layers_combines_multiple_group_payloads(monkeypatch: pytest.MonkeyPatch) -> None:
    first_group = [
        _model(skin_index=0, stable_id='first-main', live2d_key='first-main'),
        _model(skin_index=0, stable_id='first-back', live2d_key='first-back'),
    ]
    second_group = [
        _model(skin_index=1, stable_id='second-main', live2d_key='second-main'),
        _model(skin_index=1, stable_id='second-back', live2d_key='second-back'),
    ]

    async def fake_capture_runtime_groups(_request: RuntimeCaptureRequest) -> list[RuntimeGroupCapture]:
        return [
            RuntimeGroupCapture(
                models=first_group,
                containers=[
                    {'index': 0, 'zIndex': 9, 'rawZIndex': '9'},
                    {'index': 1, 'zIndex': 8, 'rawZIndex': '8'},
                ],
                requested_live2d_keys=['first-main', 'first-back'],
            ),
            RuntimeGroupCapture(
                models=second_group,
                containers=[
                    {'index': 0, 'zIndex': 7, 'rawZIndex': '7'},
                    {'index': 1, 'zIndex': 6, 'rawZIndex': '6'},
                ],
                requested_live2d_keys=['second-main', 'second-back'],
            ),
        ]

    monkeypatch.setattr(runtime_module, '_capture_runtime_groups', fake_capture_runtime_groups)

    payload = asyncio.run(
        runtime_module.capture_gamekee_runtime_layers(RuntimeCaptureRequest(content_id=711133, models=[*first_group, *second_group])),
    )

    assert payload['runtime'] == {
        'requested_live2d_keys': ['first-main', 'first-back', 'second-main', 'second-back'],
        'matched_live2d_keys': ['first-main', 'first-back', 'second-main', 'second-back'],
        'container_count': 4,
    }
    assert payload['fingerprint'] == live2d_layer_fingerprint(711133, [*first_group, *second_group])
    assert [(layer['skin_index'], layer['stable_id'], layer['layer_order']) for layer in payload['layers']] == [
        (0, 'first-main', 2),
        (0, 'first-back', 1),
        (1, 'second-main', 2),
        (1, 'second-back', 1),
    ]


def test_build_layer_capture_payload_orders_runtime_layers_by_z_index() -> None:
    models = [
        _model(skin_index=0, stable_id='other-main', live2d_key='other-main'),
        _model(skin_index=0, stable_id='other-front', live2d_key='other-front'),
        _model(skin_index=1, stable_id='stable-main', live2d_key='main'),
        _model(skin_index=1, stable_id='stable-front', live2d_key='front'),
        _model(skin_index=1, stable_id='stable-back', live2d_key='back'),
    ]
    containers = [
        {'index': 0, 'zIndex': 9, 'rawZIndex': '9', 'canvasCount': 1},
        {'index': 1, 'zIndex': '10', 'rawZIndex': '10', 'canvasCount': 1},
        {'index': 2, 'zIndex': 8, 'rawZIndex': '8', 'canvasCount': 1},
    ]

    payload = build_layer_capture_payload(
        LayerCaptureBuildInput(
            content_id=711133,
            title='Test',
            models=models,
            containers=containers,
            requested_live2d_keys=['main', 'front', 'back'],
            captured_at='2026-07-07T00:00:00+00:00',
        ),
    )

    assert payload['runtime'] == {
        'requested_live2d_keys': ['main', 'front', 'back'],
        'matched_live2d_keys': ['main', 'front', 'back'],
        'container_count': 3,
    }
    assert payload['status'] == 'success'
    assert payload['warnings'] == []
    assert payload['fingerprint'] == live2d_layer_fingerprint(711133, models[2:])
    assert layer_capture_fingerprint(711133, models[2:]) == live2d_layer_fingerprint(711133, models[2:])
    assert [
        (
            layer['skin_index'],
            layer['stable_id'],
            layer['live2d_key'],
            layer['source_layer_index'],
            layer['source_z_index'],
            layer['layer_order'],
            layer['is_primary'],
            layer['layer_match_confidence'],
        )
        for layer in payload['layers']
    ] == [
        (1, 'stable-main', 'main', 0, 9, 2, True, 'high'),
        (1, 'stable-front', 'front', 1, 10, 3, False, 'high'),
        (1, 'stable-back', 'back', 2, 8, 1, False, 'high'),
    ]
    assert payload['layers'][0]['resource_urls']['atlas'] == 'https://cdn.example.com/live2d/main/model.atlas'
    assert payload['layers'][0]['raw_container']['zIndex'] == 9


def test_build_layer_capture_payload_marks_low_confidence_when_runtime_key_is_missing() -> None:
    payload = build_layer_capture_payload(
        LayerCaptureBuildInput(
            content_id=711133,
            models=[
                _model(skin_index=0, stable_id='stable-main', live2d_key='main'),
                _model(skin_index=0, stable_id='stable-front', live2d_key='front'),
            ],
            containers=[
                {'index': 0, 'zIndex': 5, 'rawZIndex': '5'},
                {'index': 1, 'zIndex': 'auto', 'rawZIndex': 'auto'},
            ],
            requested_live2d_keys=['main'],
            captured_at='2026-07-07T00:00:00+00:00',
        ),
    )

    assert payload['runtime']['matched_live2d_keys'] == ['main']
    assert payload['warnings'] == ['fewer runtime live2d keys than visible containers']
    assert payload['layers'][0]['layer_match_confidence'] == 'high'
    assert payload['layers'][0]['layer_order'] == 1
    assert payload['layers'][1]['stable_id'] == 'stable-front'
    assert payload['layers'][1]['source_z_index'] is None
    assert payload['layers'][1]['layer_match_confidence'] == 'low'
    assert 'layer_order' not in payload['layers'][1]


def test_build_layer_capture_payload_marks_low_confidence_when_z_index_is_non_finite() -> None:
    payload = build_layer_capture_payload(
        LayerCaptureBuildInput(
            content_id=711133,
            models=[
                _model(skin_index=0, stable_id='stable-main', live2d_key='main'),
                _model(skin_index=0, stable_id='stable-front', live2d_key='front'),
            ],
            containers=[
                {'index': 0, 'zIndex': 5, 'rawZIndex': '5'},
                {'index': 1, 'zIndex': 'auto', 'rawZIndex': 'auto'},
            ],
            requested_live2d_keys=['main', 'front'],
            captured_at='2026-07-07T00:00:00+00:00',
        ),
    )

    assert payload['runtime']['matched_live2d_keys'] == ['main', 'front']
    assert payload['warnings'] == []
    assert payload['layers'][0]['layer_match_confidence'] == 'high'
    assert payload['layers'][0]['layer_order'] == 1
    assert payload['layers'][1]['live2d_key'] == 'front'
    assert payload['layers'][1]['source_z_index'] is None
    assert payload['layers'][1]['layer_match_confidence'] == 'low'
    assert 'layer_order' not in payload['layers'][1]


def test_build_layer_capture_payload_is_accepted_by_existing_merge_boundary() -> None:
    models = [
        _model(skin_index=0, stable_id='stable-main', live2d_key='main'),
        _model(skin_index=0, stable_id='stable-front', live2d_key='front'),
        _model(skin_index=0, stable_id='stable-back', live2d_key='back'),
    ]
    payload = build_layer_capture_payload(
        LayerCaptureBuildInput(
            content_id=711133,
            models=models,
            containers=[
                {'index': 0, 'zIndex': 9, 'rawZIndex': '9'},
                {'index': 1, 'zIndex': 10, 'rawZIndex': '10'},
                {'index': 2, 'zIndex': 8, 'rawZIndex': '8'},
            ],
            requested_live2d_keys=['main', 'front', 'back'],
            captured_at='2026-07-07T00:00:00+00:00',
        ),
    )

    report = merge_live2d_layer_captures(models, payload, content_id=711133, dry_run=False)

    assert report['matched'] == 3
    assert report['skipped'] == []
    assert report['quality_issues'] == []
    assert [
        (
            model['stable_id'],
            model['layer_order'],
            model['source_z_index'],
            model['source_layer_index'],
            model['is_primary'],
            model['layer_match_confidence'],
        )
        for model in models
    ] == [
        ('stable-main', 2, 9, 0, True, 'high'),
        ('stable-front', 3, 10, 1, False, 'high'),
        ('stable-back', 1, 8, 2, False, 'high'),
    ]

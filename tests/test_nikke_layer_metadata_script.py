# ruff: noqa: INP001, S101

from __future__ import annotations

import asyncio
import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import pytest

from src.web.nikke_layer_metadata import RUNTIME_ANIMATION_CAPTURE_SCHEMA, live2d_layer_fingerprint

CONTENT_ID = 711133
SECOND_CONTENT_ID = 711134
MAIN_LAYER_ORDER = 2
LIMIT_ONE = 1
ROOT_COUNT = 2
TIMEOUT_MS = 12_500

SCRIPT_PATH = Path(__file__).resolve().parents[1] / 'script/nikke_layer_metadata.py'
SPEC = importlib.util.spec_from_file_location('nikke_layer_metadata_script', SCRIPT_PATH)
assert SPEC is not None
cli = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(cli)


def _model(stable_id: str, live2d_key: str) -> dict[str, Any]:
    return {
        'label': 'live2d(full)',
        'skin_index': 0,
        'stable_id': stable_id,
        'live2d_key': live2d_key,
        'urls': {
            'atlas': f'https://cdn.example.com/{live2d_key}/model.atlas',
            'skel': f'https://cdn.example.com/{live2d_key}/model.skel',
        },
    }


def _models() -> list[dict[str, Any]]:
    return [_model('stable-main', 'main'), _model('stable-back', 'back')]


def _second_skin_models() -> list[dict[str, Any]]:
    return [
        {**_model('extra-main', 'extra-main'), 'skin_index': 1},
        {**_model('extra-back', 'extra-back'), 'skin_index': 1},
    ]


def _write_character(root: Path, *, content_id: int = CONTENT_ID, models: list[dict[str, Any]] | None = None) -> None:
    root.mkdir(parents=True)
    live2d_models = [dict(model) for model in (models or _models())]
    payload = {'content_id': content_id, 'title': 'Test', 'live2d_models': live2d_models}
    (root / 'manifest.json').write_text(json.dumps(payload), encoding='utf-8')
    (root / 'character.json').write_text(json.dumps(payload), encoding='utf-8')


def _runtime_animations() -> dict[str, Any]:
    return {
        'idle': {
            'animation': 'idle',
            'enabled': True,
            'loop': True,
            'match_method': 'gamekee-runtime-player',
            'match_confidence': 'high',
        },
        'click': {
            'animation': 'action',
            'enabled': True,
            'loop': False,
            'match_method': 'gamekee-runtime-player-event',
            'match_confidence': 'high',
            'duration_ms': 1200,
        },
    }


def _success_capture(content_id: int, models: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        'content_id': content_id,
        'status': 'success',
        'layer_capture_schema': RUNTIME_ANIMATION_CAPTURE_SCHEMA,
        'fingerprint': live2d_layer_fingerprint(content_id, models),
        'layers': [
            {
                'stable_id': 'stable-main',
                'live2d_key': 'main',
                'layer_order': 2,
                'source_z_index': 9,
                'source_layer_index': 0,
                'is_primary': True,
                'layer_match_confidence': 'high',
                'runtime_animations': _runtime_animations(),
            },
            {
                'stable_id': 'stable-back',
                'live2d_key': 'back',
                'layer_order': 1,
                'source_z_index': 8,
                'source_layer_index': 1,
                'is_primary': False,
                'layer_match_confidence': 'high',
                'runtime_animations': _runtime_animations(),
            },
        ],
    }


def _runtime_args(tmp_path: Path, root: Path, *, write: bool) -> SimpleNamespace:
    return SimpleNamespace(
        nikke_root=tmp_path,
        character_root=[root],
        content_id=[],
        write=write,
        backup_dir=None,
        limit=0,
        timeout_seconds=60.0,
        headful=False,
        no_fail=False,
    )


def test_capture_missing_dry_run_only_plans_missing_multi_full(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    root = tmp_path / '711133 - Test'
    _write_character(root)

    async def unexpected_capture(*_args: Any, **_kwargs: Any) -> dict[str, Any]:
        msg = 'dry-run capture-missing must not launch browser capture'
        raise AssertionError(msg)

    monkeypatch.setattr(cli, 'capture_gamekee_runtime_layers', unexpected_capture)

    report = asyncio.run(cli.run_runtime_capture_command(_runtime_args(tmp_path, root, write=False), force_refresh=False))

    assert report['dry_run'] is True
    assert report['failed_count'] == 0
    assert report['results'][0]['action'] == 'would_capture'
    assert report['results'][0]['reuse_decision']['reason'] == 'no_previous_capture'
    assert not (root / 'raw/live2d-layer-capture.json').exists()


def test_capture_missing_reuses_previous_capture_without_launching_browser(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    root = tmp_path / '711133 - Test'
    models = _models()
    _write_character(root, models=models)
    raw_capture_path = root / 'raw/live2d-layer-capture.json'
    raw_capture_path.parent.mkdir()
    raw_capture_path.write_text(json.dumps(_success_capture(CONTENT_ID, models)), encoding='utf-8')

    async def unexpected_capture(*_args: Any, **_kwargs: Any) -> dict[str, Any]:
        msg = 'browser capture should not run when raw capture is reusable'
        raise AssertionError(msg)

    monkeypatch.setattr(cli, 'capture_gamekee_runtime_layers', unexpected_capture)

    report = asyncio.run(cli.run_runtime_capture_command(_runtime_args(tmp_path, root, write=True), force_refresh=False))

    assert report['failed_count'] == 0
    assert report['results'][0]['action'] == 'reused'
    manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    assert manifest['live2d_models'][0]['layer_order'] == MAIN_LAYER_ORDER
    assert manifest['live2d_layer_capture']['status'] == 'success'


def test_capture_missing_does_not_reuse_previous_capture_that_leaves_groups_incomplete(tmp_path: Path) -> None:
    root = tmp_path / '711133 - Test'
    first_group = _models()
    second_group = _second_skin_models()
    second_group[0].update(
        {
            'layer_order': 2,
            'source_z_index': 7,
            'source_layer_index': 0,
            'is_primary': True,
            'layer_match_method': 'gamekee-runtime-container',
            'layer_match_confidence': 'high',
        },
    )
    second_group[1].update(
        {
            'layer_order': 1,
            'source_z_index': 6,
            'source_layer_index': 1,
            'is_primary': False,
            'layer_match_method': 'gamekee-runtime-container',
            'layer_match_confidence': 'high',
        },
    )
    models = first_group + second_group
    _write_character(root, models=models)
    raw_capture_path = root / 'raw/live2d-layer-capture.json'
    raw_capture_path.parent.mkdir()
    raw_capture_path.write_text(json.dumps(_success_capture(CONTENT_ID, first_group)), encoding='utf-8')

    report = asyncio.run(cli.run_runtime_capture_command(_runtime_args(tmp_path, root, write=False), force_refresh=False))

    result = report['results'][0]
    assert report['failed_count'] == 0
    assert result['reuse_decision']['reusable'] is False
    assert result['reuse_decision']['reason'] == 'capture_missing_multi_full_groups'
    assert result['reuse_decision']['missing_groups'] == [{'skin_index': 1, 'model_kind': 'full'}]
    assert result['action'] == 'would_capture'
    assert result['reason'] == 'capture_missing_multi_full_groups'


def test_backfill_runtime_layers_captures_writes_raw_and_merges(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    root = tmp_path / '711133 - Test'
    models = _models()
    _write_character(root, models=models)
    capture = _success_capture(CONTENT_ID, models)

    async def fake_capture(request: Any) -> dict[str, Any]:
        assert request.content_id == CONTENT_ID
        assert request.timeout_ms == TIMEOUT_MS
        return capture

    monkeypatch.setattr(cli, 'capture_gamekee_runtime_layers', fake_capture)
    args = _runtime_args(tmp_path, root, write=True)
    args.timeout_seconds = 12.5

    report = asyncio.run(cli.run_runtime_capture_command(args, force_refresh=False))

    assert report['failed_count'] == 0
    assert report['results'][0]['action'] == 'captured'
    raw_capture = json.loads((root / 'raw/live2d-layer-capture.json').read_text(encoding='utf-8'))
    assert raw_capture == capture
    manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    assert manifest['live2d_models'][0]['layer_order'] == MAIN_LAYER_ORDER
    assert manifest['live2d_layer_capture']['status'] == 'success'
    character = json.loads((root / 'character.json').read_text(encoding='utf-8'))
    assert character['live2d_models'][1]['is_primary'] is False


def test_capture_missing_limit_stops_after_requested_count(tmp_path: Path) -> None:
    first_root = tmp_path / '711133 - Test'
    second_root = tmp_path / '711134 - Test 2'
    _write_character(first_root, content_id=CONTENT_ID)
    _write_character(second_root, content_id=SECOND_CONTENT_ID)
    args = _runtime_args(tmp_path, first_root, write=False)
    args.character_root = [first_root, second_root]
    args.limit = LIMIT_ONE

    report = asyncio.run(cli.run_runtime_capture_command(args, force_refresh=False))

    assert report['checked'] == ROOT_COUNT
    assert report['processed'] == LIMIT_ONE
    assert [result['content_id'] for result in report['results']] == [CONTENT_ID]


def test_backfill_runtime_layers_force_refresh_captures_even_when_previous_capture_is_reusable(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    root = tmp_path / '711133 - Test'
    models = _models()
    _write_character(root, models=models)
    reusable_capture = _success_capture(CONTENT_ID, models)
    raw_capture_path = root / 'raw/live2d-layer-capture.json'
    raw_capture_path.parent.mkdir()
    raw_capture_path.write_text(json.dumps(reusable_capture), encoding='utf-8')
    captured: list[int] = []

    async def fake_capture(request: Any) -> dict[str, Any]:
        captured.append(request.content_id)
        return reusable_capture

    monkeypatch.setattr(cli, 'capture_gamekee_runtime_layers', fake_capture)

    report = asyncio.run(cli.run_runtime_capture_command(_runtime_args(tmp_path, root, write=True), force_refresh=True))

    assert captured == [CONTENT_ID]
    assert report['failed_count'] == 0
    assert report['results'][0]['action'] == 'captured'
    assert report['results'][0]['reuse_decision']['reason'] == 'force_refresh'


def test_backfill_runtime_layers_restores_backup_when_write_merge_fails(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    root = tmp_path / '711133 - Test'
    models = _models()
    _write_character(root, models=models)
    before_manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    before_character = json.loads((root / 'character.json').read_text(encoding='utf-8'))
    capture = _success_capture(CONTENT_ID, models)

    async def fake_capture(_request: Any) -> dict[str, Any]:
        return capture

    def failing_merge_layer_capture_files(*, root: Path, capture_payload: dict[str, Any], dry_run: bool) -> dict[str, Any]:
        _ = capture_payload, dry_run
        broken = {'content_id': CONTENT_ID, 'title': 'BROKEN', 'live2d_models': []}
        (root / 'manifest.json').write_text(json.dumps(broken), encoding='utf-8')
        msg = 'merge failed after manifest write'
        raise RuntimeError(msg)

    monkeypatch.setattr(cli, 'capture_gamekee_runtime_layers', fake_capture)
    monkeypatch.setattr(cli, 'merge_layer_capture_files', failing_merge_layer_capture_files)

    report = asyncio.run(cli.run_runtime_capture_command(_runtime_args(tmp_path, root, write=True), force_refresh=False))

    result = report['results'][0]
    assert report['failed_count'] == 1
    assert result['action'] == 'failed'
    assert result['error_class'] == 'RuntimeError'
    assert result['backup']['manifest.json']
    assert result['backup']['character.json']
    assert result['restored'] == {
        'manifest.json': 'restored',
        'character.json': 'restored',
        'raw/live2d-layer-capture.json': 'removed',
    }
    assert json.loads((root / 'manifest.json').read_text(encoding='utf-8')) == before_manifest
    assert json.loads((root / 'character.json').read_text(encoding='utf-8')) == before_character
    assert not (root / 'raw/live2d-layer-capture.json').exists()


def test_main_dispatches_backfill_force_refresh_and_limit(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    root = tmp_path / '711133 - Test'
    _write_character(root)
    seen: dict[str, Any] = {}

    async def fake_run_runtime_capture_command(args: Any, *, force_refresh: bool) -> dict[str, Any]:
        seen['command'] = args.command
        seen['limit'] = args.limit
        seen['force_refresh'] = force_refresh
        seen['root'] = args.character_root[0]
        return {'failed_count': 0, 'results': []}

    monkeypatch.setattr(cli, 'run_runtime_capture_command', fake_run_runtime_capture_command)
    monkeypatch.setattr(
        cli.sys,
        'argv',
        [
            'nikke_layer_metadata.py',
            'backfill-runtime-layers',
            '--character-root',
            root.as_posix(),
            '--limit',
            str(LIMIT_ONE),
            '--force-refresh',
        ],
    )

    assert cli.main() == 0

    assert seen == {
        'command': 'backfill-runtime-layers',
        'limit': LIMIT_ONE,
        'force_refresh': True,
        'root': root,
    }
    assert json.loads(capsys.readouterr().out)['failed_count'] == 0


def test_main_dispatches_capture_missing_without_force_refresh(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
    capsys: pytest.CaptureFixture[str],
) -> None:
    root = tmp_path / '711133 - Test'
    _write_character(root)
    seen: dict[str, Any] = {}

    async def fake_run_runtime_capture_command(args: Any, *, force_refresh: bool) -> dict[str, Any]:
        seen['command'] = args.command
        seen['write'] = args.write
        seen['force_refresh'] = force_refresh
        seen['root'] = args.character_root[0]
        return {'failed_count': 0, 'results': []}

    monkeypatch.setattr(cli, 'run_runtime_capture_command', fake_run_runtime_capture_command)
    monkeypatch.setattr(
        cli.sys,
        'argv',
        [
            'nikke_layer_metadata.py',
            'capture-missing',
            '--character-root',
            root.as_posix(),
            '--write',
        ],
    )

    assert cli.main() == 0

    assert seen == {
        'command': 'capture-missing',
        'write': True,
        'force_refresh': False,
        'root': root,
    }
    assert json.loads(capsys.readouterr().out)['failed_count'] == 0


def test_dockerfile_copies_layer_metadata_script_and_runs_playwright_smoke() -> None:
    dockerfile = (Path(__file__).resolve().parents[1] / 'Dockerfile').read_text(encoding='utf-8')

    assert 'python -m playwright install --with-deps chromium' in dockerfile
    assert 'sync_playwright().start()' in dockerfile
    assert 'COPY script/ script/' in dockerfile
    assert 'python -m compileall src/ script/ run.py' in dockerfile

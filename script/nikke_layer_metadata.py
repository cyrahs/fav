#!/usr/bin/env python
# ruff: noqa: E402, T201
from __future__ import annotations

import argparse
import json
import shutil
import sys
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from src.web.nikke import (
    merge_layer_capture_files,
    strip_layer_metadata_files,
    validate_live2d_layer_metadata,
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description='Merge, validate, or strip NIKKE GameKee Live2D layer metadata.')
    subparsers = parser.add_subparsers(dest='command', required=True)

    merge = subparsers.add_parser('merge', help='Merge a GameKee runtime layer capture into manifest.json and character.json.')
    merge.add_argument('--nikke-root', type=Path, default=Path('./collection/nikke'), help='NIKKE collection root.')
    merge.add_argument('--character-root', type=Path, help='Specific character directory containing manifest.json.')
    merge.add_argument('--content-id', type=int, help='Content id used to find the character directory.')
    merge.add_argument('--capture', type=Path, required=True, help='Layer capture JSON file.')
    merge.add_argument('--write', action='store_true', help='Write changes. Without this flag the command is a dry-run.')
    merge.add_argument('--backup-dir', type=Path, help='Directory for pre-write backups. Defaults under the NIKKE root.')

    merge_dir = subparsers.add_parser('merge-dir', help='Merge every capture JSON in a directory.')
    merge_dir.add_argument('--nikke-root', type=Path, default=Path('./collection/nikke'), help='NIKKE collection root.')
    merge_dir.add_argument('--capture-dir', type=Path, required=True, help='Directory containing layer capture JSON files.')
    merge_dir.add_argument('--pattern', default='*.json', help='Glob pattern inside --capture-dir.')
    merge_dir.add_argument('--write', action='store_true', help='Write changes. Without this flag the command is a dry-run.')
    merge_dir.add_argument('--backup-dir', type=Path, help='Directory for pre-write backups. Defaults under the NIKKE root.')

    validate = subparsers.add_parser('validate', help='Validate layer metadata quality gates.')
    validate.add_argument('--nikke-root', type=Path, default=Path('./collection/nikke'), help='NIKKE collection root.')
    validate.add_argument('--character-root', type=Path, action='append', default=[], help='Specific character directory to validate.')
    validate.add_argument(
        '--content-id',
        type=int,
        action='append',
        default=[],
        help='Content id to validate. Can be passed multiple times.',
    )
    validate.add_argument('--no-fail', action='store_true', help='Return exit code 0 even when validation errors are found.')

    strip = subparsers.add_parser('strip', help='Remove layer metadata fields for rollback.')
    strip.add_argument('--nikke-root', type=Path, default=Path('./collection/nikke'), help='NIKKE collection root.')
    strip.add_argument('--character-root', type=Path, help='Specific character directory containing manifest.json.')
    strip.add_argument('--content-id', type=int, help='Content id used to find the character directory.')
    strip.add_argument('--write', action='store_true', help='Write changes. Without this flag the command is a dry-run.')
    strip.add_argument('--backup-dir', type=Path, help='Directory for pre-write backups. Defaults under the NIKKE root.')

    return parser.parse_args()


def read_json_object(path: Path) -> dict[str, Any]:
    data = json.loads(path.read_text(encoding='utf-8'))
    if not isinstance(data, dict):
        msg = f'{path} does not contain a JSON object'
        raise TypeError(msg)
    return data


def capture_content_id(path: Path) -> int | None:
    value = read_json_object(path).get('content_id')
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return None


def manifest_content_id(path: Path) -> int | None:
    value = read_json_object(path).get('content_id')
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return None


def find_character_root(nikke_root: Path, content_id: int) -> Path:
    matches: list[Path] = []
    for manifest_path in sorted(nikke_root.glob('*/manifest.json')):
        try:
            if manifest_content_id(manifest_path) == content_id:
                matches.append(manifest_path.parent)
        except (OSError, json.JSONDecodeError, TypeError):
            continue
    if len(matches) == 1:
        return matches[0]
    if not matches:
        msg = f'No NIKKE manifest found for content_id={content_id} under {nikke_root}'
        raise SystemExit(msg)
    msg = f'Multiple NIKKE manifests found for content_id={content_id}: {", ".join(path.as_posix() for path in matches)}'
    raise SystemExit(msg)


def roots_from_args(args: argparse.Namespace) -> list[Path]:
    roots = [Path(path) for path in getattr(args, 'character_root', []) if path]
    roots.extend(find_character_root(args.nikke_root, content_id) for content_id in getattr(args, 'content_id', []) or [])
    if roots:
        return sorted(set(roots))
    return sorted(path.parent for path in args.nikke_root.glob('*/manifest.json'))


def target_root(args: argparse.Namespace, *, capture_path: Path | None = None) -> Path:
    if args.character_root:
        return args.character_root
    content_id = args.content_id
    if content_id is None and capture_path is not None:
        content_id = capture_content_id(capture_path)
    if content_id is None:
        msg = 'Pass --character-root, --content-id, or a capture JSON with content_id.'
        raise SystemExit(msg)
    return find_character_root(args.nikke_root, content_id)


def default_backup_dir(nikke_root: Path) -> Path:
    stamp = datetime.now(UTC).strftime('%Y%m%dT%H%M%SZ')
    return nikke_root / '_layer-backups' / stamp


def backup_files(root: Path, backup_dir: Path) -> dict[str, str]:
    target_dir = backup_dir / root.name
    target_dir.mkdir(parents=True, exist_ok=True)
    copied: dict[str, str] = {}
    for filename in ('manifest.json', 'character.json'):
        source = root / filename
        if not source.exists():
            continue
        destination = target_dir / filename
        shutil.copy2(source, destination)
        copied[filename] = destination.as_posix()
    return copied


def command_merge(args: argparse.Namespace) -> int:
    capture_payload = read_json_object(args.capture)
    root = target_root(args, capture_path=args.capture)
    backup: dict[str, str] = {}
    if args.write:
        backup = backup_files(root, args.backup_dir or default_backup_dir(args.nikke_root))
    report = merge_layer_capture_files(root=root, capture_payload=capture_payload, dry_run=not args.write)
    report['backup'] = backup
    print(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True))
    errors = [
        issue
        for issue in report.get('manifest', {}).get('quality_issues', [])
        if isinstance(issue, dict) and issue.get('severity') == 'error'
    ]
    return 1 if errors else 0


def report_errors(report: dict[str, Any]) -> list[dict[str, Any]]:
    return [
        issue
        for issue in report.get('manifest', {}).get('quality_issues', [])
        if isinstance(issue, dict) and issue.get('severity') == 'error'
    ]


def merge_capture_path(
    *,
    nikke_root: Path,
    capture_path: Path,
    write: bool,
    backup_dir: Path | None,
) -> dict[str, Any]:
    try:
        capture_payload = read_json_object(capture_path)
        content_id = capture_content_id(capture_path)
        if content_id is None:
            return {
                'capture_path': capture_path.as_posix(),
                'ok': False,
                'error': 'capture JSON is missing content_id',
            }
        root = find_character_root(nikke_root, content_id)
        backup = backup_files(root, backup_dir) if write and backup_dir is not None else {}
        report = merge_layer_capture_files(root=root, capture_payload=capture_payload, dry_run=not write)
    except (OSError, json.JSONDecodeError, TypeError, ValueError, SystemExit) as exc:
        return {
            'capture_path': capture_path.as_posix(),
            'ok': False,
            'error': str(exc),
        }
    report['backup'] = backup
    return {
        'capture_path': capture_path.as_posix(),
        'ok': not report_errors(report),
        'report': report,
    }


def command_merge_dir(args: argparse.Namespace) -> int:
    capture_paths = sorted(path for path in args.capture_dir.glob(args.pattern) if path.is_file())
    backup_root = args.backup_dir or default_backup_dir(args.nikke_root)
    results = [
        merge_capture_path(
            nikke_root=args.nikke_root,
            capture_path=capture_path,
            write=args.write,
            backup_dir=backup_root,
        )
        for capture_path in capture_paths
    ]
    failed = [result for result in results if not result.get('ok')]
    report = {
        'dry_run': not args.write,
        'capture_count': len(capture_paths),
        'failed_count': len(failed),
        'results': results,
    }
    print(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True))
    return 1 if failed else 0


def command_validate(args: argparse.Namespace) -> int:
    entries: list[dict[str, Any]] = []
    error_count = 0
    roots = roots_from_args(args)
    for root in roots:
        manifest_path = root / 'manifest.json'
        try:
            manifest = read_json_object(manifest_path)
            models = manifest.get('live2d_models')
            issues = (
                validate_live2d_layer_metadata(models)
                if isinstance(models, list)
                else [{'severity': 'error', 'code': 'invalid_manifest_models', 'message': 'manifest live2d_models is not a list'}]
            )
        except (OSError, json.JSONDecodeError, TypeError) as exc:
            issues = [{'severity': 'error', 'code': 'manifest_read_failed', 'message': str(exc)}]
            manifest = {'content_id': None, 'title': root.name}
        error_count += sum(1 for issue in issues if issue.get('severity') == 'error')
        if issues:
            entries.append(
                {
                    'root': root.as_posix(),
                    'content_id': manifest.get('content_id'),
                    'title': manifest.get('title') or root.name,
                    'issues': issues,
                },
            )
    report = {
        'checked': len(roots),
        'error_count': error_count,
        'entries': entries,
    }
    print(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True))
    return 0 if args.no_fail or error_count == 0 else 1


def command_strip(args: argparse.Namespace) -> int:
    root = target_root(args)
    backup: dict[str, str] = {}
    if args.write:
        backup = backup_files(root, args.backup_dir or default_backup_dir(args.nikke_root))
    report = strip_layer_metadata_files(root=root, dry_run=not args.write)
    report['backup'] = backup
    print(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True))
    return 0


def main() -> int:
    args = parse_args()
    if args.command == 'merge':
        return command_merge(args)
    if args.command == 'merge-dir':
        return command_merge_dir(args)
    if args.command == 'validate':
        return command_validate(args)
    if args.command == 'strip':
        return command_strip(args)
    return 1


if __name__ == '__main__':
    raise SystemExit(main())

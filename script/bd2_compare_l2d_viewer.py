#!/usr/bin/env python
# ruff: noqa: E402, T201
from __future__ import annotations

import argparse
import asyncio
import json
import sys
from pathlib import Path
from typing import Any

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from src.tool.bd2_l2d_viewer import (
    ComparisonResult,
    GameKeeResource,
    ViewerResource,
    compare_resources,
    fetch_gamekee_resources,
    fetch_viewer_resources,
    load_gamekee_resources_from_manifests,
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description='Compare BD2 GameKee Live2D resources with Jelosus2 BD2-L2D-Viewer resources.',
    )
    parser.add_argument(
        '--source',
        choices=('local', 'gamekee-live'),
        default='local',
        help='Use local BD2 manifests or fetch current GameKee detail pages without downloading assets.',
    )
    parser.add_argument(
        '--bd2-root',
        type=Path,
        default=Path('./collection/bd2'),
        help='Local BD2 collection root used when --source=local.',
    )
    parser.add_argument(
        '--content-id',
        action='append',
        type=int,
        default=[],
        help='Limit --source=gamekee-live to one GameKee content id. Can be passed multiple times.',
    )
    parser.add_argument('--limit', type=int, help='Limit the number of GameKee pages read.')
    parser.add_argument('--concurrency', type=int, default=2, help='GameKee detail fetch concurrency for --source=gamekee-live.')
    parser.add_argument('--timeout', type=float, default=60.0, help='HTTP timeout in seconds.')
    parser.add_argument('--format', choices=('text', 'json'), default='text', help='Output format.')
    parser.add_argument('--show', type=int, default=40, help='Maximum rows to show per text section.')
    return parser.parse_args()


async def load_gamekee(args: argparse.Namespace) -> tuple[tuple[GameKeeResource, ...], int]:
    if args.source == 'gamekee-live':
        return await fetch_gamekee_resources(
            content_ids=set(args.content_id) or None,
            limit=args.limit,
            concurrency=args.concurrency,
            request_timeout=args.timeout,
        )
    resources, manifest_count = load_gamekee_resources_from_manifests(args.bd2_root)
    if args.limit is not None:
        resources = resources[: max(0, args.limit)]
    return resources, manifest_count


def main() -> None:
    args = parse_args()
    viewer_resources = fetch_viewer_resources(timeout=args.timeout)
    gamekee_resources, page_count = asyncio.run(load_gamekee(args))
    result = compare_resources(gamekee_resources=gamekee_resources, viewer_resources=viewer_resources)

    if args.format == 'json':
        print(json.dumps(_json_payload(result, args=args, page_count=page_count), ensure_ascii=False, indent=2, sort_keys=True))
        return

    print(format_text_report(result, args=args, page_count=page_count))


def _json_payload(result: ComparisonResult, *, args: argparse.Namespace, page_count: int) -> dict[str, Any]:
    payload = result.to_dict()
    payload['source'] = args.source
    payload['gamekee_page_count'] = page_count
    return payload


def format_text_report(result: ComparisonResult, *, args: argparse.Namespace, page_count: int) -> str:
    summary = result.summary()
    lines = [
        'BD2 L2D Viewer comparison',
        f'GameKee source: {args.source} ({page_count} pages/manifests)',
        f'Viewer resources: {summary["viewer_resource_count"]} total, {summary["viewer_unique_stem_count"]} unique stems',
        f'GameKee resources: {summary["gamekee_resource_count"]} total, {summary["gamekee_unique_stem_count"]} unique stems',
        f'Matched unique stems: {summary["matched_unique_stem_count"]}',
        f'Only in viewer: {summary["viewer_only_resource_count"]}',
        f'Only in GameKee: {summary["gamekee_only_resource_count"]}',
        f'Viewer missing core files: {summary["viewer_missing_core_file_count"]}',
    ]

    lines.extend(_viewer_section('Only in viewer', result.viewer_only, args.show))
    lines.extend(_gamekee_section('Only in GameKee', result.gamekee_only, args.show))
    missing = tuple(resource for resource in result.viewer_resources if resource.missing_core_files)
    lines.extend(_viewer_section('Viewer resources missing core files', missing, args.show, include_missing=True))
    return '\n'.join(lines)


def _viewer_section(
    title: str,
    resources: tuple[ViewerResource, ...],
    limit: int,
    *,
    include_missing: bool = False,
) -> list[str]:
    lines = ['', f'{title}:']
    if not resources:
        lines.append('  none')
        return lines

    for resource in resources[: max(0, limit)]:
        line = f'  {resource.stem} [{resource.category}] {resource.char_name} / {resource.costume_name} (viewer_id={resource.entry_id})'
        lines.append(line)
        if include_missing:
            lines.extend(f'    missing: {path}' for path in resource.missing_core_files)
    if len(resources) > limit:
        lines.append(f'  ... {len(resources) - limit} more')
    return lines


def _gamekee_section(title: str, resources: tuple[GameKeeResource, ...], limit: int) -> list[str]:
    lines = ['', f'{title}:']
    if not resources:
        lines.append('  none')
        return lines

    lines.extend(
        f'  {resource.stem} [{resource.category}] {resource.title} (content_id={resource.content_id}, model_key={resource.model_key})'
        for resource in resources[: max(0, limit)]
    )
    if len(resources) > limit:
        lines.append(f'  ... {len(resources) - limit} more')
    return lines


if __name__ == '__main__':
    main()

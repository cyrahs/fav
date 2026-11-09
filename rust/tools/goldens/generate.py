"""Generate the golden files the Rust tests compare against, from the Python baseline.

Run from the repository root on the `rust` branch, whose Python tree is the baseline
commit in `rust/BASELINE` (CI's tree guard enforces that):

    UV_PYTHON=3.12 uv run python rust/tools/goldens/generate.py          # rewrite rust/goldens/
    UV_PYTHON=3.12 uv run python rust/tools/goldens/generate.py --check  # fail on any drift

Every golden is produced by calling the real Python code. The Rust side may read these
files but never writes them, and no golden is ever edited by hand: change the case list
in `areas/<area>.py` and regenerate. Production runs Python 3.12, so goldens are made
with 3.12 too (`str`, `repr` and `re` details can differ between versions).

Each module in `areas/` exposes `GOLDENS: dict[str, Callable[[], list]]`, mapping a path
under `rust/goldens/` to a function returning the list of cases. Cases must be
deterministic: no clock, no randomness, no network, no database.
"""

from __future__ import annotations

import argparse
import difflib
import importlib
import json
import os
import pkgutil
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
RUST_DIR = HERE.parents[1]
REPO_ROOT = RUST_DIR.parent
GOLDEN_DIR = RUST_DIR / 'goldens'

# The same bootstrap tests/conftest.py uses: src.core.env refuses to import without
# these, and nothing here ever connects to the database they name.
os.environ.setdefault('POSTGRES_DSN', 'postgresql://goldens@127.0.0.1:1/never-connected')
os.environ.setdefault('API_TOKEN', 'token-for-goldens')
os.environ['TZ'] = 'UTC'
sys.path.insert(0, str(REPO_ROOT))
sys.path.insert(0, str(HERE))


def _pin_default_settings() -> None:
    from src.core import settings  # noqa: PLC0415 - after the bootstrap above

    settings.use(settings.Settings())


def _render(area: str, cases: list[object]) -> str:
    document = {
        '_generated': 'by rust/tools/goldens/generate.py from the Python baseline; do not edit',
        'baseline': (RUST_DIR / 'BASELINE').read_text(encoding='utf-8').strip(),
        'python': f'{sys.version_info.major}.{sys.version_info.minor}',
        'area': area,
        'cases': cases,
    }
    return json.dumps(document, ensure_ascii=False, indent=2) + '\n'


def build() -> dict[str, str]:
    _pin_default_settings()
    import areas  # noqa: PLC0415

    outputs: dict[str, str] = {}
    for info in sorted(pkgutil.iter_modules(areas.__path__), key=lambda item: item.name):
        module = importlib.import_module(f'areas.{info.name}')
        for relative, producer in module.GOLDENS.items():
            if relative in outputs:
                msg = f'golden {relative} is produced twice'
                raise SystemExit(msg)
            outputs[relative] = _render(info.name, producer())
    return outputs


def _existing() -> dict[str, str]:
    if not GOLDEN_DIR.exists():
        return {}
    return {path.relative_to(GOLDEN_DIR).as_posix(): path.read_text(encoding='utf-8') for path in sorted(GOLDEN_DIR.rglob('*.json'))}


def check(outputs: dict[str, str]) -> int:
    existing = _existing()
    problems = 0
    for relative in sorted(set(outputs) | set(existing)):
        want, have = outputs.get(relative), existing.get(relative)
        if want == have:
            continue
        problems += 1
        if have is None:
            print(f'missing: rust/goldens/{relative} (run generate.py and commit it)')
        elif want is None:
            print(f'stale: rust/goldens/{relative} has no generator (delete it or restore its producer)')
        else:
            print(f'drift: rust/goldens/{relative} differs from what the Python baseline produces')
            diff = difflib.unified_diff(have.splitlines(), want.splitlines(), 'committed', 'generated', lineterm='', n=1)
            for line in list(diff)[:40]:
                print(f'  {line}')
    if problems:
        print(f'{problems} golden file(s) out of date')
        return 1
    print(f'{len(outputs)} golden files match the Python baseline')
    return 0


def write(outputs: dict[str, str]) -> None:
    for relative in _existing().keys() - outputs.keys():
        (GOLDEN_DIR / relative).unlink()
    for relative, text in outputs.items():
        path = GOLDEN_DIR / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding='utf-8', newline='\n')
    print(f'wrote {len(outputs)} golden files under rust/goldens/')


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split('\n', 1)[0])
    parser.add_argument('--check', action='store_true', help='compare instead of writing; exit 1 on any difference')
    args = parser.parse_args()
    outputs = build()
    if args.check:
        return check(outputs)
    write(outputs)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())

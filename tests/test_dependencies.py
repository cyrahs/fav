# ruff: noqa: INP001, S101

"""Guards on how the two downloaders are declared.

Both are ordinary uv dependencies rather than binaries baked into the image, which
is what lets Dependabot upgrade them. The parts worth pinning down here are the ones
whose failure mode is silent.
"""

import tomllib
from pathlib import Path

PYPROJECT = Path(__file__).resolve().parents[1] / 'pyproject.toml'
DOCKERFILE = Path(__file__).resolve().parents[1] / 'Dockerfile'


def _dependencies() -> list[str]:
    with PYPROJECT.open('rb') as handle:
        return tomllib.load(handle)['project']['dependencies']


def _requirement(name: str) -> str:
    for dependency in _dependencies():
        if dependency.split('[')[0].split('>')[0].split('=')[0].strip() == name:
            return dependency
    msg = f'{name} is not declared in pyproject.toml'
    raise AssertionError(msg)


def test_yt_dlp_keeps_its_default_extra() -> None:
    """Plain `yt-dlp` declares no dependencies at all.

    Dropping the extra looks like a harmless simplification and installs cleanly, but
    it silently leaves out pycryptodomex, websockets, certifi, requests/urllib3 and
    yt-dlp-ejs. Nothing fails until a download needs one of them -- encrypted HLS, in
    Hanime1's case. The standalone binary this replaced bundled all of them.
    """
    assert '[default]' in _requirement('yt-dlp')


def test_both_downloaders_are_uv_dependencies() -> None:
    # If either is ever reinstated as an image-level binary, its version leaves the
    # lockfile and Dependabot stops being able to upgrade it.
    declared = {dependency.split('[')[0].split('>')[0].split('=')[0].strip() for dependency in _dependencies()}

    assert {'yt-dlp', 'gallery-dl'} <= declared


def test_the_image_does_not_also_install_a_yt_dlp_binary() -> None:
    # Two sources of truth for one tool: whichever comes first on PATH wins, and the
    # lockfile would no longer describe what actually runs.
    dockerfile = DOCKERFILE.read_text(encoding='utf-8')

    assert 'yt-dlp_linux' not in dockerfile
    assert '/usr/local/bin/yt-dlp' not in dockerfile
    # ffmpeg is not bundled by either Python package and still has to come from apt.
    assert 'ffmpeg' in dockerfile

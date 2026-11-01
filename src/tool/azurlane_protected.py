"""l2d.su's AZP2 envelope for Azur Lane data and text assets.

Since 2026-10-31 l2d.su serves Azur Lane from static.l2d.su/al/ only, and every JSON file there
(the ship index, the per-ship details, model3/motion/physics files, spine part manifests and
atlases, painting indexes and meshes) is wrapped in an AZP2 envelope. Images, moc3, skel and ogg
files are served as they are.

The envelope, as the site's own front end opens it (protectedCodec-*.js):

    b"AZP2" | nonce (12 bytes) | AES-256-GCM ciphertext + 16-byte tag

with the additional data ``b"azp2:" + identity`` (the identity as lowercase hex text). The
plaintext is ``seed: u32 LE | codec: u8 | length: u32 LE | masked body``. The body is stored
reversed and masked with an xorshift32 stream seeded by ``seed``; codec 0 is the raw file and
codec 1 a gzip member whose ISIZE trailer equals ``length``.

The 32-byte key and identity come from two places:

- Asset references carry them in the URL fragment: ``<path>#azp2=<key hex>.<identity hex>``.
  The fragment is never sent to the server, so a URL with it is still the URL to request.
- Files under /data/ have no fragment; their descriptor is derived from the path below /data/
  (``ships-CN.json``, ``ships/CN/10117.json``) with a key the front end ships in
  protectedData-*.js.
"""

from __future__ import annotations

import gzip
import hashlib
import hmac
import re
import struct
import zlib
from dataclasses import dataclass
from urllib.parse import urlsplit

from Crypto.Cipher import AES

AZP2_MAGIC = b'AZP2'
# The data key l2d.su's front end embeds (protectedData-*.js); /data/ files are derived from it.
AZP2_DATA_MASTER_KEY = bytes.fromhex('05b341c7e4149af5c65beed3fe6ec486c5c5c2b3ecdfe415409000fac1571034')
# Limits the front end enforces: the envelope at most 64 MiB + 128 KiB, the plaintext at most 64 MiB.
AZP2_MAX_ENVELOPE_SIZE = 67_239_936
AZP2_MAX_PLAINTEXT_SIZE = 67_108_864

_NONCE_END = 16
_TAG_SIZE = 16
_MIN_ENVELOPE_SIZE = 41
_HEADER = struct.Struct('<IBI')
_CODEC_RAW = 0
_CODEC_GZIP = 1
_MIN_GZIP_SIZE = 18
_FRAGMENT_RE = re.compile(r'^azp2=([0-9a-f]{64})\.([0-9a-f]{64})$')
_DATA_SEGMENT = '/data/'
_U32_MASK = 0xFFFFFFFF


class ProtectedResourceError(ValueError):
    """An AZP2 envelope that cannot be opened: wrong descriptor, tampered, or not AZP2 at all."""


@dataclass(frozen=True, slots=True)
class Azp2Descriptor:
    key: str
    identity: str


def data_descriptor(relative_path: str) -> Azp2Descriptor:
    """The descriptor of a /data/ file, from its path below /data/ (``ships-CN.json``)."""
    path = relative_path.strip()
    segments = path.split('/')
    if not path or path.startswith('/') or '\\' in path or any(segment in {'', '.', '..'} for segment in segments):
        msg = f'Invalid protected data path: {relative_path!r}'
        raise ProtectedResourceError(msg)
    key = hmac.new(AZP2_DATA_MASTER_KEY, f'azp2:data:key:{path}'.encode(), hashlib.sha256).hexdigest()
    identity = hashlib.sha256(f'azp2:data:identity:{path}'.encode()).hexdigest()
    return Azp2Descriptor(key=key, identity=identity)


def data_url_descriptor(url: str) -> Azp2Descriptor | None:
    """The derived descriptor for a URL under a /data/ directory, or None for any other URL."""
    path = urlsplit(url).path
    index = path.rfind(_DATA_SEGMENT)
    if index < 0:
        return None
    return data_descriptor(path[index + len(_DATA_SEGMENT) :])


def fragment_descriptor(url: str) -> Azp2Descriptor | None:
    """The descriptor an asset URL carries in its ``#azp2=`` fragment, or None without one."""
    fragment = urlsplit(url).fragment
    if not fragment.startswith('azp2='):
        return None
    match = _FRAGMENT_RE.match(fragment)
    if match is None:
        msg = f'Invalid AZP2 resource descriptor in {url}'
        raise ProtectedResourceError(msg)
    return Azp2Descriptor(key=match.group(1), identity=match.group(2))


def is_protected_url(url: str) -> bool:
    return urlsplit(url).fragment.startswith('azp2=')


def strip_fragment(url: str) -> str:
    return url.split('#', 1)[0]


def is_azp2_envelope(content: bytes) -> bool:
    return content[: len(AZP2_MAGIC)] == AZP2_MAGIC


def decrypt(content: bytes, descriptor: Azp2Descriptor) -> bytes:
    """Open one AZP2 envelope and return the original file."""
    size = len(content)
    if size < _MIN_ENVELOPE_SIZE or size > AZP2_MAX_ENVELOPE_SIZE or not is_azp2_envelope(content):
        msg = 'Invalid AZP2 envelope'
        raise ProtectedResourceError(msg)
    try:
        key = bytes.fromhex(descriptor.key)
        cipher = AES.new(key, AES.MODE_GCM, nonce=content[len(AZP2_MAGIC) : _NONCE_END])
        cipher.update(f'azp2:{descriptor.identity}'.encode())
        plaintext = cipher.decrypt_and_verify(content[_NONCE_END:-_TAG_SIZE], content[-_TAG_SIZE:])
    except ValueError as exc:
        msg = f'AZP2 envelope failed authentication: {exc}'
        raise ProtectedResourceError(msg) from exc

    if len(plaintext) < _HEADER.size:
        msg = 'Invalid AZP2 resource header'
        raise ProtectedResourceError(msg)
    seed, codec, length = _HEADER.unpack_from(plaintext)
    if length > AZP2_MAX_PLAINTEXT_SIZE:
        msg = 'Invalid AZP2 resource length'
        raise ProtectedResourceError(msg)

    return _decode_body(_unmask(plaintext[_HEADER.size :], seed), codec=codec, length=length)


def _decode_body(body: bytes, *, codec: int, length: int) -> bytes:
    if codec == _CODEC_RAW:
        if len(body) != length:
            msg = 'Invalid AZP2 resource length'
            raise ProtectedResourceError(msg)
        return body
    if codec == _CODEC_GZIP:
        if len(body) < _MIN_GZIP_SIZE or struct.unpack_from('<I', body, len(body) - 4)[0] != length:
            msg = 'Invalid AZP2 compressed resource'
            raise ProtectedResourceError(msg)
        try:
            inflated = gzip.decompress(body)
        except (OSError, EOFError, zlib.error) as exc:
            msg = f'Invalid AZP2 compressed resource: {exc}'
            raise ProtectedResourceError(msg) from exc
        if len(inflated) != length:
            msg = 'Invalid AZP2 compressed resource length'
            raise ProtectedResourceError(msg)
        return inflated
    msg = f'Unsupported AZP2 codec {codec}'
    raise ProtectedResourceError(msg)


def decrypt_for_url(content: bytes, url: str) -> bytes:
    """Open ``content`` when ``url`` names a protected resource; return anything else unchanged.

    A fragment descriptor wins; a /data/ URL falls back to the derived one. Only an actual AZP2
    envelope is opened, so a file the CDN still serves in the clear passes through.
    """
    descriptor = fragment_descriptor(url) or data_url_descriptor(url)
    if descriptor is None:
        return content
    if not is_azp2_envelope(content):
        if fragment_descriptor(url) is not None:
            msg = f'Expected an AZP2 envelope from {strip_fragment(url)}'
            raise ProtectedResourceError(msg)
        return content
    return decrypt(content, descriptor)


def _unmask(masked: bytes, seed: int) -> bytes:
    length = len(masked)
    output = bytearray(length)
    state = seed & _U32_MASK
    last = length - 1
    for index, value in enumerate(masked):
        state ^= (state << 13) & _U32_MASK
        state ^= state >> 17
        state ^= (state << 5) & _U32_MASK
        output[last - index] = (value - state) & 0xFF
    return bytes(output)

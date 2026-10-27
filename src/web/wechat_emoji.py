"""Stickers (表情) forwarded to the agent's WeChat account.

A sticker is a type-47 message whose ``<emoji>`` element names the file twice
on WeChat's CDN. ``cdnurl`` serves it as it is, with no login, and ``md5``
hashes those bytes. ``encrypturl`` serves the same file encrypted with
AES-128-CBC, the 16 bytes of ``aeskey`` (hex) being both key and IV. Store
stickers can leave ``cdnurl`` empty, so the encrypted copy is the fallback.
Both addresses have ``*#*`` where the scheme's ``:`` belongs.
"""

from __future__ import annotations

import hashlib
import html
import re
from dataclasses import dataclass

from Crypto.Cipher import AES
from Crypto.Util.Padding import unpad

_EMOJI_RE = re.compile(r'<emoji\s([^>]*)>', re.DOTALL)
_ATTRIBUTE_RE = re.compile(r'([A-Za-z0-9_]+)="([^"]*)"')
_MD5_RE = re.compile(r'[0-9a-f]{32}')
_AES_KEY_RE = re.compile(r'[0-9a-fA-F]{32}')


@dataclass(frozen=True)
class Sticker:
    md5: str
    cdn_url: str
    encrypted_url: str
    aes_key: str


def _url(value: str) -> str:
    url = html.unescape(value).replace('*#*', ':').strip()
    return url if url.startswith(('http://', 'https://')) else ''


def parse_sticker(content: str) -> Sticker | None:
    """The sticker a message carries, or None when it names no file to fetch."""
    match = _EMOJI_RE.search(content or '')
    if match is None:
        return None
    attributes = {key.lower(): value for key, value in _ATTRIBUTE_RE.findall(match.group(1))}
    md5 = attributes.get('md5', '').strip().lower()
    aes_key = attributes.get('aeskey', '').strip()
    sticker = Sticker(
        md5=md5 if _MD5_RE.fullmatch(md5) else '',
        cdn_url=_url(attributes.get('cdnurl', '')),
        encrypted_url=_url(attributes.get('encrypturl', '')),
        aes_key=aes_key if _AES_KEY_RE.fullmatch(aes_key) else '',
    )
    if not sticker.cdn_url and not (sticker.encrypted_url and sticker.aes_key):
        return None
    return sticker


def decrypt_sticker(content: bytes, aes_key: str) -> bytes:
    """The file behind ``encrypturl``. Raises ValueError for bytes the key does not fit."""
    key = bytes.fromhex(aes_key)
    return unpad(AES.new(key, AES.MODE_CBC, iv=key).decrypt(content), AES.block_size)


def matches(sticker: Sticker, content: bytes) -> bool:
    """Whether ``content`` is the file the message names; True when it names no hash."""
    return bool(content) and (not sticker.md5 or hashlib.md5(content).hexdigest() == sticker.md5)  # noqa: S324 - WeChat's own checksum

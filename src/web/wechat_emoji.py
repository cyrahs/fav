"""Stickers (表情) forwarded to the agent's WeChat account.

A sticker is a type-47 message whose ``<emoji>`` element names the file twice
on WeChat's CDN. ``cdnurl`` serves it as it is, with no login, and ``md5``
hashes those bytes. ``encrypturl`` serves the same file encrypted with
AES-128-CBC, the 16 bytes of ``aeskey`` (hex) being both key and IV. Store
stickers can leave ``cdnurl`` empty, so the encrypted copy is the fallback.
Both addresses have ``*#*`` where the scheme's ``:`` belongs.

A large sticker (several MB, which WeChat only loads when it is tapped) comes
as an app message of ``<type>8</type>`` instead. Its ``<emojiinfo>`` is a
base64 protobuf with the same three things: field 1 the md5, field 2 the plain
address, field 5 the encrypted one and field 6 its key.
"""

from __future__ import annotations

import base64
import binascii
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
_APP_TYPE_RE = re.compile(r'<appmsg[\s>].*?<type>\s*(\d+)\s*</type>', re.DOTALL)
_EMOJI_INFO_RE = re.compile(r'<emojiinfo>\s*([A-Za-z0-9+/=\s]+?)\s*</emojiinfo>')
_LARGE_STICKER_APP_TYPE = '8'
_LENGTH_DELIMITED = 2  # protobuf wire type
# The <emojiinfo> fields this reads, by protobuf field number.
_INFO_MD5, _INFO_URL, _INFO_ENCRYPTED_URL, _INFO_AES_KEY = 1, 2, 5, 6


@dataclass(frozen=True)
class Sticker:
    md5: str
    cdn_url: str
    encrypted_url: str
    aes_key: str


def _url(value: str) -> str:
    url = html.unescape(value).replace('*#*', ':').strip()
    return url if url.startswith(('http://', 'https://')) else ''


def _sticker(md5: str, cdn_url: str, encrypted_url: str, aes_key: str) -> Sticker | None:
    md5 = md5.strip().lower()
    aes_key = aes_key.strip()
    sticker = Sticker(
        md5=md5 if _MD5_RE.fullmatch(md5) else '',
        cdn_url=_url(cdn_url),
        encrypted_url=_url(encrypted_url),
        aes_key=aes_key if _AES_KEY_RE.fullmatch(aes_key) else '',
    )
    if not sticker.cdn_url and not (sticker.encrypted_url and sticker.aes_key):
        return None
    return sticker


def _varint(data: bytes, index: int) -> tuple[int, int]:
    value = shift = 0
    while True:
        byte = data[index]
        index += 1
        value |= (byte & 0x7F) << shift
        shift += 7
        if not byte & 0x80:
            return value, index


def _protobuf_strings(data: bytes) -> dict[int, str]:
    """The first value of each length-delimited field that is text; stops at anything it cannot read."""
    fields: dict[int, str] = {}
    index = 0
    try:
        while index < len(data):
            key, index = _varint(data, index)
            wire_type = key & 7
            if wire_type == 0:
                _, index = _varint(data, index)
            elif wire_type == _LENGTH_DELIMITED:
                length, index = _varint(data, index)
                value = data[index : index + length]
                index += length
                try:
                    fields.setdefault(key >> 3, value.decode())
                except UnicodeDecodeError:
                    continue
            else:
                break
    except IndexError:
        pass
    return fields


def _large_sticker(content: str) -> Sticker | None:
    app_type = _APP_TYPE_RE.search(content)
    info = _EMOJI_INFO_RE.search(content)
    if app_type is None or app_type.group(1) != _LARGE_STICKER_APP_TYPE or info is None:
        return None
    try:
        data = base64.b64decode(''.join(info.group(1).split()), validate=True)
    except (binascii.Error, ValueError):
        return None
    fields = _protobuf_strings(data)
    return _sticker(
        fields.get(_INFO_MD5, ''), fields.get(_INFO_URL, ''), fields.get(_INFO_ENCRYPTED_URL, ''), fields.get(_INFO_AES_KEY, '')
    )


def parse_sticker(content: str) -> Sticker | None:
    """The sticker a message carries, or None when it names no file to fetch."""
    match = _EMOJI_RE.search(content or '')
    if match is None:
        return _large_sticker(content or '')
    attributes = {key.lower(): value for key, value in _ATTRIBUTE_RE.findall(match.group(1))}
    return _sticker(attributes.get('md5', ''), attributes.get('cdnurl', ''), attributes.get('encrypturl', ''), attributes.get('aeskey', ''))


def decrypt_sticker(content: bytes, aes_key: str) -> bytes:
    """The file behind ``encrypturl``. Raises ValueError for bytes the key does not fit."""
    key = bytes.fromhex(aes_key)
    return unpad(AES.new(key, AES.MODE_CBC, iv=key).decrypt(content), AES.block_size)


def matches(sticker: Sticker, content: bytes) -> bool:
    """Whether ``content`` is the file the message names; True when it names no hash."""
    return bool(content) and (not sticker.md5 or hashlib.md5(content).hexdigest() == sticker.md5)  # noqa: S324 - WeChat's own checksum

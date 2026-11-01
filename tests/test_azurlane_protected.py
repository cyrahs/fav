# ruff: noqa: INP001, S101, PLR2004

from __future__ import annotations

import gzip
import struct

import pytest
from Crypto.Cipher import AES

from src.tool.azurlane_protected import (
    Azp2Descriptor,
    ProtectedResourceError,
    data_descriptor,
    data_url_descriptor,
    decrypt,
    decrypt_for_url,
    fragment_descriptor,
    is_protected_url,
    strip_fragment,
)

_DESCRIPTOR = Azp2Descriptor(key='11' * 32, identity='22' * 32)


def azp2_wrap(content: bytes, descriptor: Azp2Descriptor, *, codec: int = 0, seed: int = 0x1234_5678) -> bytes:
    """Build an envelope the way l2d.su's protectedCodec opens it: reversed, xorshift-masked, AES-GCM."""
    body = gzip.compress(content) if codec == 1 else content
    state = seed
    masked = bytearray(len(body))
    for index in range(len(body)):
        state ^= (state << 13) & 0xFFFFFFFF
        state ^= state >> 17
        state ^= (state << 5) & 0xFFFFFFFF
        masked[index] = (body[len(body) - 1 - index] + state) & 0xFF
    plaintext = struct.pack('<IBI', seed, codec, len(content)) + bytes(masked)
    nonce = bytes(range(12))
    cipher = AES.new(bytes.fromhex(descriptor.key), AES.MODE_GCM, nonce=nonce)
    cipher.update(f'azp2:{descriptor.identity}'.encode())
    ciphertext, tag = cipher.encrypt_and_digest(plaintext)
    return b'AZP2' + nonce + ciphertext + tag


@pytest.mark.parametrize('codec', [0, 1])
def test_decrypt_opens_both_codecs(codec: int) -> None:
    content = b'{"Version":3,"FileReferences":{"Moc":"a.moc3"}}' * 3
    assert decrypt(azp2_wrap(content, _DESCRIPTOR, codec=codec), _DESCRIPTOR) == content


def test_decrypt_rejects_a_wrong_identity_and_non_envelopes() -> None:
    envelope = azp2_wrap(b'{"a": 1}', _DESCRIPTOR)
    with pytest.raises(ProtectedResourceError):
        decrypt(envelope, Azp2Descriptor(key=_DESCRIPTOR.key, identity='33' * 32))
    with pytest.raises(ProtectedResourceError):
        decrypt(b'{"not": "an envelope", "padding": "................................"}', _DESCRIPTOR)


def test_data_descriptor_is_derived_from_the_path_below_data() -> None:
    descriptor = data_descriptor('ships-CN.json')
    assert len(descriptor.key) == len(descriptor.identity) == 64
    assert descriptor != data_descriptor('ships-EN.json')
    assert data_url_descriptor('https://static.l2d.su/al/data/ships/CN/10117.json') == data_descriptor('ships/CN/10117.json')
    assert data_url_descriptor('https://static.l2d.su/al/assets/x.json') is None
    with pytest.raises(ProtectedResourceError):
        data_descriptor('../ships-CN.json')


def test_fragment_descriptor_reads_the_azp2_fragment() -> None:
    url = f'https://static.l2d.su/al/assets/4306804565fc75b8/b0df.model3.json#azp2={"ab" * 32}.{"cd" * 32}'
    assert fragment_descriptor(url) == Azp2Descriptor(key='ab' * 32, identity='cd' * 32)
    assert is_protected_url(url)
    assert strip_fragment(url) == 'https://static.l2d.su/al/assets/4306804565fc75b8/b0df.model3.json'
    assert fragment_descriptor('https://static.l2d.su/al/assets/a0722ee10fbfd588/fe86.webp') is None
    with pytest.raises(ProtectedResourceError):
        fragment_descriptor('https://static.l2d.su/al/assets/x.json#azp2=nothex.nothex')


def test_decrypt_for_url_opens_data_and_fragment_urls_and_passes_others_through() -> None:
    data_url = 'https://static.l2d.su/al/data/ships-CN.json'
    payload = b'{"ships": []}'
    assert decrypt_for_url(azp2_wrap(payload, data_descriptor('ships-CN.json')), data_url) == payload
    # A data file still served in the clear is left alone.
    assert decrypt_for_url(payload, data_url) == payload

    asset_url = f'https://static.l2d.su/al/assets/a/b.json#azp2={_DESCRIPTOR.key}.{_DESCRIPTOR.identity}'
    assert decrypt_for_url(azp2_wrap(payload, _DESCRIPTOR), asset_url) == payload
    # A URL that names a descriptor must answer with an envelope; an error page is not the file.
    with pytest.raises(ProtectedResourceError):
        decrypt_for_url(b'<!doctype html>', asset_url)

    assert decrypt_for_url(b'RIFF....WEBP', 'https://static.l2d.su/al/assets/a/b.webp') == b'RIFF....WEBP'

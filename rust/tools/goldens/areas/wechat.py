# ruff: noqa: RUF001
# (The full-width characters in this file are part of the inputs under test.)
"""WeChat (WP14): the source's pure functions, 视频号 videos and stickers.

`parsing.json` calls the pure functions of `src/web/wechat.py`, `wechat_channels.py` and
`wechat_emoji.py` on the inputs of the Python tests plus awkward variants (other JSON
types where the bridge's events hold any, entities and CDATA, Unicode digits, broken
base64 and protobuf, out-of-range timestamps), as `{fn, args, result}`; a raise is
`{"error": "Class: message"}`, bytes are `{"hex": ...}`. `keystream.json` holds the
ISAAC-64 keystream for several keys (its SHA-256 and first bytes) and `decrypt_head` on
known inputs. `constants.json` the module constants a run depends on.
"""

from __future__ import annotations

import base64
import hashlib
from dataclasses import asdict, is_dataclass
from datetime import datetime
from pathlib import PurePath
from typing import Any

from Crypto.Cipher import AES
from Crypto.Util.Padding import pad
from src.web import wechat, wechat_channels, wechat_emoji
from src.web.wechat_channels import ChannelVideo
from src.web.wechat_emoji import Sticker


def _plain(value: Any) -> Any:  # noqa: PLR0911 - one return per type
    if is_dataclass(value) and not isinstance(value, type):
        return {key: _plain(item) for key, item in asdict(value).items()}
    if isinstance(value, tuple | list):
        return [_plain(item) for item in value]
    if isinstance(value, dict):
        return {str(key): _plain(item) for key, item in value.items()}
    if isinstance(value, datetime):
        return value.isoformat()
    if isinstance(value, bytes):
        return {'hex': value.hex()}
    if isinstance(value, PurePath):
        return str(value)
    return value


def _attempt(call: Any) -> Any:
    try:
        return _plain(call())
    except Exception as exc:  # noqa: BLE001 - the error is the recorded answer
        return {'error': f'{exc.__class__.__name__}: {exc}'}


_CREATE_TIME = 1_759_634_000_000


def _message(msg_id: Any = 1, **updates: Any) -> dict[str, Any]:
    message: dict[str, Any] = {
        'msg_id': msg_id,
        'type': 3,
        'kind': 'image',
        'chat': 'private',
        'create_time': _CREATE_TIME + (msg_id if isinstance(msg_id, int) else 0),
        'talker': 'wxid_sender',
        'sender': 'wxid_sender',
        'is_send': False,
        'content': '<msg><img aeskey="secret-aes" cdnbigimgurl="cdn-url" /></msg>',
        'image': {'md5': 'wechat-md5', 'key': 'secret-aes', 'mid_img_url': 'cdn-url'},
    }
    message.update(updates)
    return message


def _event(message: Any, kind: Any = 'message', seq: Any = 1) -> dict[str, Any]:
    return {'seq': seq, 'kind': kind, 'at': _CREATE_TIME, 'data': message}


_SHARE_URL = 'http://mp.weixin.qq.com/s?__biz=MzA5==&amp;mid=2247&amp;idx=1&amp;sn=abc123&amp;chksm=feed#rd'


def _share(url: str = _SHARE_URL, kind: str = '5', title: str = '每日分享') -> str:
    return (
        f'<msg><appmsg appid="" sdkver="0"><title>{title}</title><des>今天的</des><action>view</action>'
        f'<type>{kind}</type><url>{url}</url><sourcedisplayname>小叶菌ovo</sourcedisplayname>'
        '<refermsg><type>1</type></refermsg></appmsg><fromusername>wxid_sender</fromusername></msg>'
    )


# ---------- stickers ----------

_STICKER = b'GIF89a-sticker-frames'
_STICKER_MD5 = hashlib.md5(_STICKER).hexdigest()  # noqa: S324
_STICKER_KEY = '00112233445566778899aabbccddeeff'
_STICKER_CDN = 'http://wxapp.tc.qq.com/262/20304/stodownload?m=plain&filekey=aaa&bizid=1023'
_STICKER_ENCRYPTED = 'http://wxapp.tc.qq.com/262/20304/stodownload?m=enc&filekey=bbb&bizid=1023'


def _attribute(url: str) -> str:
    return url.replace('://', '*#*//').replace('&', '&amp;')


def _sticker_share(*, cdn: str = _STICKER_CDN, md5: str = _STICKER_MD5, key: str = _STICKER_KEY, enc: str = _STICKER_ENCRYPTED) -> str:
    return (
        '<msg><emoji fromusername="wxid_sender" tousername="wxid_me" type="2" idbuffer="media*#*0_0" '
        f'md5="{md5}" len="{len(_STICKER)}" productid="" cdnurl="{_attribute(cdn) if cdn else ""}" thumburl="" '
        f'encrypturl="{_attribute(enc) if enc else ""}" aeskey="{key}" width="240" height="240"></emoji>'
        '<gameext type="0" content="0"></gameext></msg>'
    )


def _field(number: int, value: bytes | str) -> bytes:
    data = value.encode() if isinstance(value, str) else value
    return _varint(number << 3 | 2) + _varint(len(data)) + data


def _varint(value: int) -> bytes:
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def _large(info: bytes | str, app_type: str = '8') -> str:
    text = info if isinstance(info, str) else base64.b64encode(info).decode()
    return (
        '<?xml version="1.0"?>\n<msg>\n\t<appmsg appid="" sdkver="0">\n\t\t<title />\n\t\t'
        f'<type>{app_type}</type>\n\t\t<appattach>'
        f'<totallen>{len(_STICKER)}</totallen><emoticonmd5>{_STICKER_MD5}</emoticonmd5><fileext>pic</fileext>'
        f'<emojiinfo>{text}</emojiinfo></appattach>\n\t</appmsg>\n'
        '\t<fromusername>wxid_sender</fromusername>\n</msg>\n'
    )


_INFO = b''.join(
    (
        _field(1, _STICKER_MD5),
        _field(2, _STICKER_CDN),
        bytes([3 << 3, 1]),
        _field(5, _STICKER_ENCRYPTED),
        _field(6, _STICKER_KEY),
        _field(7, ''),
        _field(9, 'f' * 32),
    )
)

STICKERS = [
    _sticker_share(),
    _sticker_share(cdn=''),
    _sticker_share(cdn='', key=''),
    _sticker_share(cdn='', enc=''),
    _sticker_share(md5=_STICKER_MD5.upper()),
    _sticker_share(md5='x'),
    _sticker_share(md5=' ' + _STICKER_MD5 + ' '),
    _sticker_share(key='00112233445566778899AABBCCDDEEFF'),
    _sticker_share(key='0011223344556677'),
    _sticker_share(key='zz112233445566778899aabbccddeeff'),
    _sticker_share(cdn='ftp://wxapp.tc.qq.com/x'),
    '<msg><emoji md5="x" cdnurl="" /></msg>',
    '<msg><emoji MD5="' + _STICKER_MD5 + '" CdnUrl="http*#*//a/b" CDNURL="https*#*//c/d"></emoji></msg>',
    '<msg><emoji cdnurl=" http*#*//a/b?x=1&amp;y=2 "></emoji></msg>',
    '<msg><emoji cdnurl="http://a/*#*/b"></emoji></msg>',
    '<msg><emoji\ncdnurl="http*#*//a/b"\n></emoji></msg>',
    '<msg><emojix cdnurl="http*#*//a/b"></emojix></msg>',
    '<msg><emoji></emoji></msg>',
    "<msg><emoji cdnurl='http*#*//a/b'></emoji></msg>",
    '<msg><emoji a-b="1" cdnurl="http*#*//a/b"></emoji></msg>',
    '<msg><emoji cdnurl="&#104;ttp*#*//a/b"></emoji></msg>',
    _large(_INFO),
    _large(_INFO, app_type='6'),
    _large(_INFO, app_type=' 8 '),
    _large('!!' + base64.b64encode(_INFO).decode()),
    _large(base64.b64encode(_INFO).decode()[:-1]),
    _large(base64.b64encode(_INFO).decode()[:40] + '\n  ' + base64.b64encode(_INFO).decode()[40:]),
    _large('=' + base64.b64encode(_INFO).decode()),
    _large('QUJD=QUJD'),
    _large('QUJDRA=='),
    _large('QUJDRA==QQ=='),
    _large('QUJDR'),
    _large('QUJD='),
    _large('QUJDRA==='),
    _large('QUJDRA='),
    _large('QU JD\tRA ==\n'),
    _large(''),
    _large(b''.join((_field(1, _STICKER_MD5), _field(2, b'\xff\xfe'), _field(2, _STICKER_CDN)))),
    _large(b''.join((_field(1, _STICKER_MD5), _field(2, _STICKER_CDN), bytes([5 << 3 | 5, 1, 2, 3, 4]), _field(5, 'x')))),
    _large(b''.join((_field(1, _STICKER_MD5), _field(2, _STICKER_CDN), bytes([6 << 3 | 2, 100]), b'short'))),
    _large(b''.join((_field(2, _STICKER_CDN), bytes([3 << 3, 0x80])))),
    _large(b''.join((_field(2, _STICKER_CDN), bytes([1 << 3 | 1]) + b'12345678', _field(1, _STICKER_MD5)))),
    _large(b''.join((_field(1, _STICKER_MD5), _field(1, 'other'), _field(2, _STICKER_CDN)))),
    _large(b''.join((_field(2, 'x' * 200), _field(1, _STICKER_MD5)))),
    _large(b''.join((_field(2, _STICKER_CDN), _field(16, 'sixteen')))),
    '<appmsg><type>8</type><emojiinfo>' + base64.b64encode(_INFO).decode() + '</emojiinfo></appmsg>',
    '<emojiinfo>' + base64.b64encode(_INFO).decode() + '</emojiinfo><appmsg><type>8</type></appmsg>',
    _share(),
    'plain',
    '',
]


def _encrypted(content: bytes, key: str = _STICKER_KEY) -> bytes:
    raw = bytes.fromhex(key)
    return AES.new(raw, AES.MODE_CBC, iv=raw).encrypt(pad(content, AES.block_size))


def _raw_encrypted(content: bytes, key: str = _STICKER_KEY) -> bytes:
    raw = bytes.fromhex(key)
    return AES.new(raw, AES.MODE_CBC, iv=raw).encrypt(content)


DECRYPTS = [
    (_encrypted(_STICKER), _STICKER_KEY),
    (_encrypted(b''), _STICKER_KEY),
    (_encrypted(b'x' * 16), _STICKER_KEY),
    (_encrypted(_STICKER, '00112233445566778899AABBCCDDEEFF'), '00112233445566778899AABBCCDDEEFF'),
    (_encrypted(_STICKER), 'ffeeddccbbaa99887766554433221100'),
    (_raw_encrypted(b'a' * 15 + b'\x00'), _STICKER_KEY),
    (_raw_encrypted(b'a' * 15 + b'\x11'), _STICKER_KEY),
    (_raw_encrypted(b'a' * 14 + b'\x01\x02'), _STICKER_KEY),
    (_raw_encrypted(b'a' * 16 + b'\x10' * 16), _STICKER_KEY),
    (_raw_encrypted(b'a' * 13 + b'\x02\x03\x03'), _STICKER_KEY),
    (b'', _STICKER_KEY),
    (b'short', _STICKER_KEY),
    (b'x' * 17, _STICKER_KEY),
]


def _sticker(**fields: str) -> Sticker:
    base = {'md5': _STICKER_MD5, 'cdn_url': _STICKER_CDN, 'encrypted_url': '', 'aes_key': ''}
    base.update(fields)
    return Sticker(**base)


MATCHES = [
    ({}, _STICKER),
    ({}, b'other'),
    ({}, b''),
    ({'md5': ''}, b'anything'),
    ({'md5': ''}, b''),
]


# ---------- 视频号 videos ----------

_OBJECT_ID = '14983747958594083406'
_NONCE_ID = '12832287456652343062_0_25_90_26_1791657144215330_efda1c2e-c4d8-11f1-83d0-83b106a41c8a'
_COVER_URL = 'https://wxapp.tc.qq.com/251/20304/stodownload?encfilekey=abc&amp;token=cover-token'


def _finder(  # noqa: PLR0913 - one parameter per field of the share under test
    *,
    media_type: str = '4',
    live_id: str = '0',
    object_id: str = _OBJECT_ID,
    nonce: str = _NONCE_ID,
    cover: str = f'<coverUrl>{_COVER_URL}</coverUrl>',
    thumb: str = _COVER_URL,
    duration: str = '9',
    width: str = '1080',
    height: str = '1920',
    desc: str = '镜头之外才是故事真正的开始 偶遇着他们的幸福#徐绿杨',
    nickname: str = 'jiang57l',
    media: bool = True,
) -> str:
    media_xml = (
        '<mediaList><media>'
        f'<thumbUrl>{thumb}</thumbUrl><videoPlayDuration>{duration}</videoPlayDuration>'
        '<url>http://wxapp.tc.qq.com/251/20302/stodownload?encfilekey=def&amp;m=1</url>'
        f'{cover}<height>{height}</height><mediaType>{media_type}</mediaType><width>{width}</width>'
        '</media></mediaList>'
        if media
        else ''
    )
    return (
        '<msg><appmsg appid="" sdkver="0"><title>当前版本不支持展示该内容</title><type>51</type>'
        '<url>https://support.weixin.qq.com/update/</url>'
        f'<finderFeed><objectId>{object_id}</objectId><feedType>4</feedType><nickname>{nickname}</nickname>'
        '<avatar><![CDATA[https://wx.qlogo.cn/finderhead/ver_1/x/0]]></avatar>'
        f'<desc>{desc}</desc><mediaCount>1</mediaCount>'
        f'<objectNonceId>{nonce}</objectNonceId><liveId>{live_id}</liveId>'
        f'<username>v2_0600@finder</username>{media_xml}<megaVideo><objectId></objectId><objectNonceId></objectNonceId></megaVideo>'
        '</finderFeed></appmsg><fromusername>wxid_sender</fromusername></msg>'
    )


FINDERS = [
    _finder(),
    _finder(media_type='2'),
    _finder(media_type=' 4 '),
    _finder(live_id='2054'),
    _finder(live_id=''),
    _finder(live_id='<![CDATA[0]]>'),
    _finder(object_id='99999999999999999999'),
    _finder(object_id='18446744073709551615'),
    _finder(object_id='18446744073709551616'),
    _finder(object_id='0'),
    _finder(object_id='١٢٣'),
    _finder(object_id='12a'),
    _finder(object_id=''),
    _finder(object_id=' 123 '),
    _finder(object_id='<![CDATA[123]]>'),
    _finder(object_id='123456789012345678901'),
    _finder(nonce='11875607166103288523'),
    _finder(nonce=''),
    _finder(nonce='a' * 200),
    _finder(nonce='a' * 201),
    _finder(nonce='has space'),
    _finder(nonce='abc.def'),
    _finder(cover=''),
    _finder(cover='', thumb=''),
    _finder(cover='<coverUrl>ftp://x/y</coverUrl>'),
    _finder(cover='<coverUrl>//wxapp.tc.qq.com/x</coverUrl>', thumb=''),
    _finder(cover='<coverUrl>HTTPS://wxapp.tc.qq.com/x</coverUrl>'),
    _finder(cover='<coverUrl>[::1</coverUrl>'),
    _finder(duration='x'),
    _finder(duration='²'),
    _finder(duration='１２'),
    _finder(duration=''),
    _finder(duration='-1'),
    _finder(width='0', height=' 7 '),
    _finder(desc='<![CDATA[line one\nline two]]>'),
    _finder(desc='  &lt;b&gt; &amp; x  '),
    _finder(desc='', nickname='&amp;author'),
    _finder(media=False),
    _finder().replace('<finderFeed>', '<finderfeed>').replace('</finderFeed>', '</finderfeed>'),
    _finder().replace('</finderFeed>', ''),
    '<finderFeed><objectId>1</objectId><objectNonceId>n</objectNonceId>'
    '<media><mediaType>4</mediaType></media></finderFeed><finderFeed><objectId>2</objectId></finderFeed>',
    '<finderFeed><objectNonceId>n</objectNonceId><objectId>5</objectId><media><mediaType>2</mediaType></media>'
    '<media><mediaType>4</mediaType></media></finderFeed>',
    _share(),
    '',
]


def _video(**fields: Any) -> dict[str, Any]:
    base: dict[str, Any] = {
        'object_id': _OBJECT_ID,
        'nonce_id': _NONCE_ID,
        'author': 'jiang57l',
        'description': '镜头之外才是故事真正的开始',
        'duration': 9,
        'width': 1080,
        'height': 1920,
        'cover_url': '',
    }
    base.update(fields)
    return base


LOOKUPS = [
    _video(),
    _video(nonce_id='11875607166103288523'),
    _video(nonce_id='1' * 32),
    _video(nonce_id='1' * 33),
    _video(nonce_id='١٢٣'),
    _video(nonce_id='0'),
    _video(nonce_id=''),
]

TITLES = [
    _video(),
    _video(description='first line\nsecond line'),
    _video(description='\n\n  second  '),
    _video(description='  padded  \nnext'),
    _video(description='a\rb'),
    _video(description='a b'),
    _video(description='a\x1cb'),
    _video(description='a\x0bb'),
    _video(description='   '),
    _video(description='　x　'),
    _video(description='', author=''),
    _video(description='', author='author'),
    _video(description=' \n ', author=''),
]

_VIDEO_URL = 'https://finder.video.qq.com/251/20302/stodownload?encfilekey=enc&token=video-token'
_OFFERED = b'\x08\x03\x12\x06xWT113"6\n\n\n\x06xWT113\x10\x00\n\n\n\x06xWT112\x10\x01\n\n\n\x06xWT111\x10\x00\n\x07\n\x03xA0\x10\x00'
_BASEDATA = base64.urlsafe_b64encode(_OFFERED).decode().rstrip('=')
_BASE_URL = f'https://finder.video.qq.com/251/20302/stodownload?encfilekey=enc&basedata={_BASEDATA}&sign=s'

QUALITY = [
    _BASE_URL,
    f'{_BASE_URL}&X-snsvideoflag=xWT113',
    f'{_BASE_URL}&x-SnsVideoFlag=',
    f'{_BASE_URL}&x-snsvideoflag=1',
    _VIDEO_URL,
    f'{_VIDEO_URL}&basedata=%%%',
    f'{_VIDEO_URL}&basedata=',
    f'{_VIDEO_URL}&basedata={base64.b64encode(_OFFERED).decode()}',
    f'{_VIDEO_URL}&basedata={base64.b64encode(_OFFERED).decode().replace("+", "%2B")}',
    f'{_VIDEO_URL}&basedata={base64.b64encode(b"xWT200 xWT020 xWT1").decode()}',
    f'{_VIDEO_URL}&basedata={base64.b64encode(b"xWT999xWT0999").decode()}',
    f'{_VIDEO_URL}&basedata={base64.b64encode(b"no specs here").decode()}',
    f'{_VIDEO_URL}&basedata={base64.b64encode(b"xWT12").decode()}',
    f'{_VIDEO_URL}&basedata=eFdUMTEx&basedata={base64.b64encode(b"xWT100").decode()}',
    f'{_VIDEO_URL}&basedata=e',
    f'{_VIDEO_URL}&basedata=eFdUMTEx===',
    f'{_VIDEO_URL}&basedata=eF*dUM!TEx',
    f'{_VIDEO_URL}&basedata=eFdU%E4%B8%ADMTEx',
    f'{_VIDEO_URL}&basedata=eFdU+MTEx',
    f'{_VIDEO_URL}&basedata=eFdUMTEx#frag',
    f'{_VIDEO_URL}#basedata=eFdUMTEx',
    f'https://finder.video.qq.com/x?basedata=eFdUMTEx&{"a" * 10}',
    'https://finder.video.qq.com/x?basedata=eFdUMTEx',
    'https://finder.video.qq.com/x?basedata=eFdUMTEx&X-snsvideoflag=xWT113',
    'https://[::1/x?basedata=eFdUMTEx',
    'not a url',
    '',
]

SOURCES: list[Any] = [
    {
        'code': 200,
        'data': {
            'id': _OBJECT_ID,
            'media': {
                'url': _VIDEO_URL.split('&token=', maxsplit=1)[0],
                'url_token': '&token=video-token',
                'full_url': _VIDEO_URL,
                'decode_key': '2136343393',
            },
        },
    },
    {'data': {'media': {'url': 'https://finder.video.qq.com/x?a=1', 'url_token': '&token=t', 'decode_key': '42'}}},
    {'data': {'media': {'url': 'https://finder.video.qq.com/x?a=1', 'decode_key': '42'}}},
    {'data': {'media': {'url': 'https://finder.video.qq.com/x?a=1', 'url_token': None, 'decode_key': 42}}},
    {'data': {'data': {'media': [{'full_url': _VIDEO_URL, 'decode_key': '7'}]}}},
    {'data': {'data': {'media': []}}},
    {'data': {'media': [], 'data': {'media': {'full_url': _VIDEO_URL, 'decode_key': '7'}}}},
    {'data': {'data': 'x'}},
    {'data': {'media': {'full_url': _VIDEO_URL, 'decode_key': ''}}},
    {'data': {'media': {'full_url': _VIDEO_URL, 'decode_key': ' 7 '}}},
    {'data': {'media': {'full_url': _VIDEO_URL, 'decode_key': '18446744073709551615'}}},
    {'data': {'media': {'full_url': _VIDEO_URL, 'decode_key': '18446744073709551616'}}},
    {'data': {'media': {'full_url': _VIDEO_URL, 'decode_key': '-7'}}},
    {'data': {'media': {'full_url': _VIDEO_URL, 'decode_key': '١٢'}}},
    {'data': {'media': {'full_url': _VIDEO_URL, 'decode_key': 7.5}}},
    {'data': {'media': {'full_url': _VIDEO_URL, 'decode_key': True}}},
    {'data': {'media': {'full_url': _VIDEO_URL, 'decode_key': 0}}},
    {'data': {'media': {'full_url': _VIDEO_URL, 'decode_key': '0'}}},
    {'data': {'media': {'full_url': 'ftp://x/y', 'decode_key': '7'}}},
    {'data': {'media': {'full_url': '', 'url': 0, 'decode_key': '7'}}},
    {'data': {'media': {'full_url': 5, 'decode_key': '7'}}},
    {'data': {'media': {'full_url': f'{_BASE_URL}', 'decode_key': '7'}}},
    {'data': {'media': {'full_url': 'https://[::1/x', 'decode_key': '7'}}},
    {'data': {'media': [{'decode_key': '7'}, {'full_url': _VIDEO_URL}]}},
    {'data': {'media': ['x']}},
    {'data': {'media': 'x'}},
    {'data': {}},
    {'data': None},
    {'data': []},
    {},
    [],
    'x',
    None,
]

_DECODE_KEY = 2136343393
# Inputs up to this size are recorded whole, longer ones by hash.
_SHOWN_BYTES = 64
_HEAD = bytes.fromhex('23766a16ff8ffe1a1ca6cd5f994846ab')


def _keystreams() -> list[dict[str, Any]]:
    out = []
    for key in (0, 1, 7, 42, _DECODE_KEY, 2**63, 2**64 - 1):
        stream = wechat_channels.keystream(key)
        out.append(
            {
                'decode_key': key,
                'size': len(stream),
                'sha256': hashlib.sha256(stream).hexdigest(),
                'head': stream[:64].hex(),
                'tail': stream[-64:].hex(),
            }
        )
    for size in (0, 1, 7, 8, 2047, 2048, 2049, 4096):
        stream = wechat_channels.keystream(_DECODE_KEY, size)
        out.append({'decode_key': _DECODE_KEY, 'size': size, 'sha256': hashlib.sha256(stream).hexdigest(), 'head': stream[:64].hex()})
    plain = bytes(range(256)) * 600
    for head, key in (
        (_HEAD, _DECODE_KEY),
        (b'', _DECODE_KEY),
        (b'\x00' * 3, 1),
        (plain, _DECODE_KEY),
        (plain[: wechat_channels.ENCRYPTED_BYTES], 5),
    ):
        result = wechat_channels.decrypt_head(head, key)
        shown = {'hex': head.hex()} if len(head) <= _SHOWN_BYTES else {'sha256': hashlib.sha256(head).hexdigest(), 'size': len(head)}
        out.append(
            {
                'decrypt_head': shown,
                'decode_key': key,
                'result_sha256': hashlib.sha256(result).hexdigest(),
                'result_head': result[:64].hex(),
                'result_size': len(result),
            }
        )
    return out


# ---------- the source ----------

_ITEM = {
    'msg_id': 101,
    'create_time': _CREATE_TIME + 101,
    'type': 'image',
    'talker': 'wxid_sender',
    'sender': 'wxid_sender',
    'md5': 'wechat-md5',
    'file': '101.jpg',
    'status': 'ok',
    'has_file': True,
}


def _item(**updates: Any) -> dict[str, Any]:
    return {**_ITEM, **updates}


PENDING: list[Any] = [
    [
        _item(msg_id=2, create_time=_CREATE_TIME + 50),
        _item(msg_id=3, create_time=_CREATE_TIME),
        _item(msg_id=4, type='video'),
        _item(msg_id=5, has_file=False),
        _item(msg_id=6, status='error: download failed'),
        _item(msg_id='7'),
        'not an item',
    ],
    [_item(msg_id=1, status='ok:backfill')],
    [_item(msg_id=1, status='OK')],
    [_item(msg_id=1, status=None), _item(msg_id=2, status=0), _item(msg_id=3, status=['ok'])],
    [_item(msg_id=1, has_file=1), _item(msg_id=2, has_file='yes'), _item(msg_id=3, has_file=[]), _item(msg_id=4, has_file='')],
    [_item(msg_id=True), _item(msg_id=1.0), _item(create_time='1'), _item(create_time=None)],
    [_item(msg_id=9, create_time=5), _item(msg_id=8, create_time=5), _item(msg_id=1, create_time=6), _item(msg_id=2, create_time=-1)],
    [_item(type='Image'), {'type': 'image', 'has_file': True, 'status': 'ok'}],
    [],
    [None, 1, [], {}],
]

_CARD = _share(url='https://example.com/p/1')
_STICKER_MESSAGE = _sticker_share()
_FINDER_MESSAGE = _finder()

EVENTS: list[Any] = [
    _event(_message(1)),
    _event(_message(2, type=49, kind='app', content=_share(), image=None)),
    _event(_message(3, is_send=True)),
    _event(_message(3, is_send=1)),
    _event(_message(3, is_send=0)),
    _event(_message(3, is_send='')),
    _event(_message(3, is_send='false')),
    _event(_message(4, chat='group')),
    _event(_message(5, chat='official')),
    _event(_message(5, chat=None)),
    _event(_message(6, kind='text', content='hello')),
    _event(_message(7, kind='app', content=_share(kind='6'))),
    _event(_message(7, kind='app', content=_share(kind='4'))),
    _event(_message(10, kind='system', content='you added a friend')),
    _event(_message(10, kind='recalled', content='x')),
    _event(_message(10, kind='pat', content='')),
    _event(_message(10, kind='voip', content='')),
    _event(_message(10, kind='System', content='x')),
    _event(_message(10, kind='system', content=_share())),
    _event(_message(8, create_time=None)),
    _event(_message(8, create_time='1')),
    _event(_message(8, create_time=1.5)),
    _event(_message(8, create_time=True)),
    _event(_message('8')),
    _event(_message(msg_id=True)),
    _event(_message(2**70)),
    _event({'wxid': 'wxid_new'}, kind='new_friend'),
    _event(_message(9), kind='Message'),
    _event(None),
    _event('x'),
    _event([]),
    _event(_message(11, kind='emoji', content=_STICKER_MESSAGE)),
    _event(_message(12, kind='other', content=_large(_INFO))),
    _event(_message(13, kind='emoji', content='<msg><emoji md5="x" cdnurl="" /></msg>')),
    _event(_message(14, kind='channel_video', content=_FINDER_MESSAGE)),
    _event(_message(15, kind='channel_video', content=_finder(media_type='2'))),
    _event(_message(16, kind='image', content='')),
    _event(_message(17, kind='app', content=_share(url='https://tieba.baidu.com/p/11093034639?&amp;share=9105'))),
    _event(_message(18, kind='app', content=_share(url='https://bbs.nga.cn/read.php?tid=47695975'))),
    _event(_message(19, kind='app', content=_share(url='https://www.zhihu.com/question/1/answer/2'))),
    _event(_message(20, kind='app', content=_share(url='https://[::1/x'))),
    _event(_message(21, kind='app', content=None)),
    _event(_message(22, kind='text', content=12)),
    _event(_message(23, kind=None, content='')),
    _event(_message(24, kind='emoji', content=_sticker_share().replace('<emoji', '<emoji ') + _share())),
    _event(_message(25, kind='app', content=_share() + _finder())),
    _event(_message(26, kind='app', content=_finder() + _share(url='https://tieba.baidu.com/p/1'))),
    'x',
    None,
    {'kind': 'message'},
    {'kind': 'message', 'data': None},
]

REASONS: list[Any] = [
    _message(1, kind='text', content='see https://example.com/a'),
    _message(2, kind='text', content='hello'),
    _message(2, kind='text', content='  ' + 'x' * 150 + '  '),
    _message(2, kind='text', content=''),
    _message(2, kind='text', content=None),
    _message(2, kind='text', content=12345),
    _message(2, kind='text', content='一' * 120),
    _message(3, kind='app', content=_CARD),
    _message(3, kind='app', content=_share(url='https://example.com/v', kind='4')),
    _message(4, kind='app', content=_share(kind='6')),
    _message(4, kind='app', content=_share(kind='6', title='<![CDATA[卡片 &amp; 标题]]>')),
    _message(4, kind='app', content=_share(kind='6', title='   ')),
    _message(4, kind='app', content=_share(kind='6', title='t' * 130)),
    _message(4, kind='app', content='<appmsg>\n<title>multi\nline</title></appmsg>'),
    _message(4, kind='app', content='<appmsgx><title>no</title></appmsgx>'),
    _message(5, kind='video', content=''),
    _message(5, kind=None, content=''),
    _message(5, kind=0, content=''),
    _message(5, kind=7, content=''),
    _message(5, kind='file', content='<msg><appmsg><title>report.pdf</title><type>6</type></appmsg></msg>'),
    _message(5, kind='text', content='看 https://example.com/a）后面'),
    _message(5, kind='emoji', content='<msg><emoji md5="x" cdnurl="" /></msg>'),
    {'msg_id': 1},
]

QUEUED: list[tuple[Any, str]] = [
    (_message(1), 'image'),
    (_message(1, image=None), 'image'),
    (_message(1, image={'md5': None}), 'image'),
    (_message(1, image={'md5': 5}), 'image'),
    (_message(1, image={}), 'image'),
    (_message(1, image='x'), 'image'),
    (_message(2, type=49, kind='app', content=_share(), image=None), 'article'),
    (_message(3, kind='emoji', content=_STICKER_MESSAGE), 'sticker'),
    (_message(4, kind='channel_video', content=_FINDER_MESSAGE), 'channel_video'),
    (_message(5, kind='text', content='hi'), 'unsupported'),
    ({'msg_id': 6}, 'unsupported'),
    ({'msg_id': 6, 'extra': 1, 'image': {'md5': 'm'}}, 'image'),
    (_message(7), 'other'),
]

IMAGE_ITEMS: list[Any] = [
    _message(1),
    {'msg_id': 2, 'create_time': 3},
    {'msg_id': 2, 'create_time': 3, 'talker': 'a', 'sender': ''},
    {'msg_id': 2, 'create_time': 3, 'talker': None, 'sender': None, 'md5': None},
    {'msg_id': 2, 'create_time': 3, 'talker': 0, 'sender': 5, 'md5': 7},
    {'msg_id': 2},
]

SNIFFS = [
    b'\xff\xd8\xff\xe0original-bytes',
    b'\x89PNG\r\n\x1a\nfull-size',
    b'GIF89a',
    b'GIF8',
    b'wxgf....',
    b'wxgf',
    b'RIFF\x00\x00\x00\x00WEBPVP8 ',
    b'RIFF\x00\x00\x00\x00WEBX',
    b'RIFF1234WEB',
    b'\xff\xd8',
    b'????',
    b'',
]

EXTENSIONS: list[tuple[Any, str]] = [
    (_item(file='1.png'), 'image/jpeg'),
    (_item(file='1.wxgf'), ''),
    (_item(file='1.PNG'), ''),
    (_item(file='1.jpeg'), 'image/jpeg'),
    (_item(file=''), 'image/webp; charset=binary'),
    (_item(file='1.exe'), 'application/octet-stream'),
    (_item(file='1.exe'), ''),
    (_item(file='dir.jpg/1'), 'IMAGE/GIF'),
    (_item(file='a/b/c.bin'), 'image/png'),
    (_item(file='.jpg'), 'image/png'),
    (_item(file='x.tar.gif'), ''),
    (_item(file='x.'), ' image/png '),
    (_item(file=None), 'image/jpeg ;x'),
    (_item(file=12), 'image/jpeg'),
    ({}, 'image/png'),
    ({'file': 'x.Jpg'}, ''),
]

TIMES = [
    _CREATE_TIME,
    _CREATE_TIME + 999,
    0,
    999,
    -1,
    -1000,
    -1001,
    253402300799999,
    253402300800000,
    -62135596800000,
    -62135596800001,
    -61000000000000,
    10**20,
]

DIRECTORIES: list[tuple[Any, str]] = [
    ({'msg_id': 44, 'create_time': _CREATE_TIME}, '每日Coser分享'),
    ({'msg_id': 44, 'create_time': _CREATE_TIME}, ''),
    ({'msg_id': 44, 'create_time': _CREATE_TIME}, '  a/b:c?  '),
    ({'msg_id': 44, 'create_time': _CREATE_TIME}, '长' * 60),
    ({'msg_id': 44, 'create_time': _CREATE_TIME}, '\n'),
    ({'msg_id': 'x', 'create_time': _CREATE_TIME}, 't'),
    ({'msg_id': 44, 'create_time': -61000000000000}, 't'),
    ({'msg_id': 44, 'create_time': 10**20}, 't'),
]

OPEN_TARGETS: list[tuple[Any, list[str]]] = [
    (_message(44), ['每日分享']),
    (_message(44), ['a', '', 'a', 'b', 'a']),
    (_message(44), ['']),
    (_message(44), []),
    ({'msg_id': 1, 'create_time': 2, 'talker': None}, ['x']),
    ({'msg_id': 1, 'create_time': 2, 'talker': 7}, ['x']),
    ({'msg_id': 1, 'create_time': 2}, ['x']),
]


def _parsing() -> list[dict[str, Any]]:  # noqa: C901, PLR0912 - one loop per function under test
    out: list[dict[str, Any]] = []

    def add(fn: str, args: dict[str, Any], call: Any) -> None:
        out.append({'fn': fn, 'args': _plain(args), 'result': _attempt(call)})

    for content in STICKERS:
        add('parse_sticker', {'content': content}, lambda content=content: wechat_emoji.parse_sticker(content))
    for content, key in DECRYPTS:
        add('decrypt_sticker', {'content': content, 'aes_key': key}, lambda c=content, k=key: wechat_emoji.decrypt_sticker(c, k))
    for fields, content in MATCHES:
        sticker = _sticker(**fields)
        add('matches', {'sticker': asdict(sticker), 'content': content}, lambda s=sticker, c=content: wechat_emoji.matches(s, c))
    for content in FINDERS:
        add('parse_channel_video', {'content': content}, lambda content=content: wechat_channels.parse_channel_video(content))
    for video in LOOKUPS:
        add('tikhub_lookup', {'video': video}, lambda video=video: wechat_channels.tikhub_lookup(ChannelVideo(**video)))
    for video in TITLES:
        add('video_title', {'video': video}, lambda video=video: wechat_channels.video_title(ChannelVideo(**video)))
    for url in QUALITY:
        add('best_quality', {'url': url}, lambda url=url: wechat_channels.best_quality(url))
    for payload in SOURCES:
        add('parse_video_source', {'payload': payload}, lambda payload=payload: wechat_channels.parse_video_source(payload))
    for items in PENDING:
        add('pending_images', {'items': items}, lambda items=items: wechat.pending_images(items))
    for event in EVENTS:
        add('work_for', {'event': event}, lambda event=event: wechat.work_for(event))
    for message in REASONS:
        add('unsupported_reason', {'message': message}, lambda message=message: wechat.unsupported_reason(message))
    for message, work in QUEUED:
        add('queued_message', {'message': message, 'work': work}, lambda m=message, w=work: wechat.queued_message(m, w))
    for message in IMAGE_ITEMS:
        add('image_item', {'message': message}, lambda message=message: wechat.image_item(message))
    for content in SNIFFS:
        add('sniff_extension', {'content': content}, lambda content=content: wechat.sniff_extension(content))
    for item, content_type in EXTENSIONS:
        add('file_extension', {'item': item, 'content_type': content_type}, lambda i=item, c=content_type: wechat.file_extension(i, c))
    for create_time in TIMES:
        item = {'msg_id': 42, 'create_time': create_time}
        add('relative_path', {'item': item, 'ext': 'jpg'}, lambda item=item: wechat.relative_path(item, 'jpg'))
        add('sent_at', {'create_time_ms': create_time}, lambda t=create_time: wechat.sent_at(t))
    for message, title in DIRECTORIES:
        add('article_directory', {'message': message, 'title': title}, lambda m=message, t=title: wechat.article_directory(m, t))
    for message, _title in DIRECTORIES:
        for video in (_video(), _video(description='a/b\nc'), _video(description='', author='', object_id='77')):
            add(
                'video_path',
                {'message': message, 'video': video},
                lambda m=message, v=video: wechat.video_path(m, ChannelVideo(**v)),
            )
    for message, texts in OPEN_TARGETS:
        add('open_target', {'message': message, 'texts': texts}, lambda m=message, t=texts: wechat.open_target(m, tuple(t)))
    for index, count in ((1, 1), (1, 2), (2, 2), (10, 12), (100, 120)):
        add('post_video_name', {'index': index, 'count': count}, lambda i=index, c=count: wechat.post_video_name(i, c))
    return out


def _constants() -> list[dict[str, Any]]:
    return [
        {
            'wechat._KNOWN_EXTENSIONS': sorted(wechat._KNOWN_EXTENSIONS),  # noqa: SLF001
            'wechat._CONTENT_TYPE_EXTENSIONS': wechat._CONTENT_TYPE_EXTENSIONS,  # noqa: SLF001
            'wechat._REQUEST_TIMEOUT_SECONDS': wechat._REQUEST_TIMEOUT_SECONDS,  # noqa: SLF001
            'wechat._EVENT_PAGE_SIZE': wechat._EVENT_PAGE_SIZE,  # noqa: SLF001
            'wechat._MAX_ATTEMPTS': wechat._MAX_ATTEMPTS,  # noqa: SLF001
            'wechat._OPEN_WAIT_SECONDS': wechat._OPEN_WAIT_SECONDS,  # noqa: SLF001
            'wechat._LINK_WAIT_SECONDS': wechat._LINK_WAIT_SECONDS,  # noqa: SLF001
            'wechat._TAP_OPEN_SECONDS': wechat._TAP_OPEN_SECONDS,  # noqa: SLF001
            'wechat._CURSOR_KEY': wechat._CURSOR_KEY,  # noqa: SLF001
            'wechat._WORK_VERSION': wechat._WORK_VERSION,  # noqa: SLF001
            'wechat._WORK_VERSION_KEY': wechat._WORK_VERSION_KEY,  # noqa: SLF001
            'wechat._UNSUPPORTED_SINCE_KEY': wechat._UNSUPPORTED_SINCE_KEY,  # noqa: SLF001
            'wechat._NGA_LOGIN_KEY': wechat._NGA_LOGIN_KEY,  # noqa: SLF001
            'wechat._TIKHUB_KEY': wechat._TIKHUB_KEY,  # noqa: SLF001
            'wechat._NEEDS_KEYS': wechat._NEEDS_KEYS,  # noqa: SLF001
            'wechat._TIKHUB_VIDEO_DETAIL': wechat._TIKHUB_VIDEO_DETAIL,  # noqa: SLF001
            'wechat._TIKHUB_READ_SECONDS': wechat._TIKHUB_READ_SECONDS,  # noqa: SLF001
            'wechat._TIKHUB_ACCOUNT_STATUSES': sorted(wechat._TIKHUB_ACCOUNT_STATUSES),  # noqa: SLF001
            'wechat._POST_VIDEO_READ_SECONDS': wechat._POST_VIDEO_READ_SECONDS,  # noqa: SLF001
            'wechat._SILENT_KINDS': sorted(wechat._SILENT_KINDS),  # noqa: SLF001
            'wechat.WORK': [
                wechat.WORK_IMAGE,
                wechat.WORK_STICKER,
                wechat.WORK_ARTICLE,
                wechat.WORK_CHANNEL_VIDEO,
                wechat.WORK_UNSUPPORTED,
            ],
            'wechat_channels.ENCRYPTED_BYTES': wechat_channels.ENCRYPTED_BYTES,
        },
    ]


GOLDENS = {
    'wechat/parsing.json': _parsing,
    'wechat/keystream.json': _keystreams,
    'wechat/constants.json': _constants,
}

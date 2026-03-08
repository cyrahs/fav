# ruff: noqa: INP001, S101

import json
import asyncio

import httpx

import src.web.jandan as jandan_module
from src.web.jandan import (
    Jandan,
    build_fav_request,
    build_image_download_candidates,
    decrypt_data_field,
    detect_deleted_placeholder,
    encrypt_data_field,
    extract_fav_items,
    extract_last_pic_date,
    extract_pic_images,
    infer_image_extension,
    should_mark_unavailable_http,
    zulu_to_offset,
)

_PIC_A_ID = 6095815
_PIC_B_ID = 6095816
_EXPECTED_IMAGE_COUNT = 2


def test_encrypt_and_decrypt_data_field_roundtrip() -> None:
    request_payload = build_fav_request(user_id=42920, fav_type=1, fav_num_limit=45)
    plain_payload = json.dumps(request_payload, ensure_ascii=False, separators=(',', ':'))

    encrypted = encrypt_data_field(plain_payload)
    decrypted = decrypt_data_field(encrypted)

    assert decrypted == plain_payload


def test_build_fav_request_supports_cursor() -> None:
    payload = build_fav_request(
        user_id=42920,
        fav_type=1,
        fav_num_limit=45,
        fav_start_date='2026-02-13T18:06:39.000-0800',
    )

    assert payload == {
        'fav': {
            'favType': '1',
            'favNumLimit': 45,
            'actionType': 'get',
            'userID': 42920,
            'favStartDate': '2026-02-13T18:06:39.000-0800',
        },
    }


def test_zulu_to_offset_matches_appendix_example() -> None:
    assert zulu_to_offset('2026-02-14T02:06:39.000Z') == '2026-02-13T18:06:39.000-0800'


def test_extract_pic_images_keeps_only_image_contents() -> None:
    pics = [
        {
            'id': _PIC_A_ID,
            'date': '2026-02-25T02:14:46.000Z',
            'author': 'author-a',
            'contents': [
                {
                    'contentType': 'image',
                    'content': 'http://img.wangmoyu.com/mw600/demo-a.jpg',
                    'md5': 'abc',
                    'imageType': 'jpeg',
                },
                {
                    'contentType': 'video',
                    'content': 'http://img.wangmoyu.com/demo.mp4',
                },
            ],
        },
        {
            'id': str(_PIC_B_ID),
            'date': '2026-02-25T02:16:46.000Z',
            'author': '',
            'contents': [
                {
                    'contentType': 'image',
                    'content': '//img.wangmoyu.com/mw600/demo-b.png',
                    'imageType': 'png',
                },
            ],
        },
    ]

    images = extract_pic_images(fav_type=1, pics=pics)

    assert len(images) == _EXPECTED_IMAGE_COUNT
    assert images[0].pic_id == _PIC_A_ID
    assert images[0].content_index == 1
    assert images[0].content_url == 'http://img.wangmoyu.com/mw600/demo-a.jpg'
    assert images[0].author == 'author-a'
    assert images[0].image_type == 'jpeg'
    assert images[1].pic_id == _PIC_B_ID
    assert images[1].content_url == 'https://img.wangmoyu.com/mw600/demo-b.png'
    assert images[1].author == 'unknown'


def test_extract_last_pic_date_returns_last_valid_date() -> None:
    pics = [
        {'id': 1, 'date': '2026-02-14T10:00:00.000Z'},
        {'id': 2, 'date': ''},
        {'id': 3, 'date': '2026-02-14T02:06:39.000Z'},
    ]

    assert extract_last_pic_date(pics) == '2026-02-14T02:06:39.000Z'


def test_extract_fav_items_by_fav_type_key() -> None:
    fav = {
        'nzs': [{'id': 1}, {'id': 2}],
    }

    items = extract_fav_items(fav_type=6, fav=fav)

    assert items == [{'id': 1}, {'id': 2}]


def test_extract_fav_items_fallbacks_to_any_list() -> None:
    fav = {
        'unexpected_key': [{'id': 10}, {'id': 20}],
        'meta': {'foo': 'bar'},
    }

    items = extract_fav_items(fav_type=1, fav=fav)

    assert items == [{'id': 10}, {'id': 20}]


def test_infer_image_extension_prefers_image_type_then_url_suffix() -> None:
    assert infer_image_extension('https://img.example.com/path/abc.unknown', image_type='jpeg') == 'jpg'
    assert infer_image_extension('https://img.example.com/path/abc.gif?token=1', image_type=None) == 'gif'
    assert infer_image_extension('https://img.example.com/path/no-ext', image_type=None) == 'jpg'


def test_detect_deleted_placeholder_by_redirect_history() -> None:
    source_url = 'http://wx1.sinaimg.cn/mw600/66b3de17ly1i3z3e7uk5uj216o1kw7g0.jpg'
    redirect = httpx.Response(
        301,
        request=httpx.Request('GET', source_url),
        headers={
            'location': '//wx1.sinaimg.cn/images/default_d_h_mw600.gif#101',
            'x-image-errno': '101',
        },
    )
    final = httpx.Response(
        200,
        request=httpx.Request('GET', 'http://wx1.sinaimg.cn/images/default_d_h_mw600.gif#101'),
        headers={'content-type': 'image/gif'},
        history=[redirect],
        content=b'GIF89a',
    )

    is_deleted, reason = detect_deleted_placeholder(final)

    assert is_deleted is True
    assert reason is not None
    assert 'placeholder' in reason


def test_detect_deleted_placeholder_returns_false_for_normal_image() -> None:
    response = httpx.Response(
        200,
        request=httpx.Request('GET', 'https://img.wangmoyu.com/mw600/example.jpg'),
        headers={'content-type': 'image/jpeg'},
        content=b'\xff\xd8\xff',
    )

    is_deleted, reason = detect_deleted_placeholder(response)

    assert is_deleted is False
    assert reason is None


def test_should_mark_unavailable_http_supports_sinaimg_403() -> None:
    assert should_mark_unavailable_http(status_code=404, url_host='example.com') is True
    assert should_mark_unavailable_http(status_code=410, url_host='example.com') is True
    assert should_mark_unavailable_http(status_code=403, url_host='tva4.sinaimg.cn') is True
    assert should_mark_unavailable_http(status_code=403, url_host='img.wangmoyu.com') is False
    assert should_mark_unavailable_http(status_code=429, url_host='tva4.sinaimg.cn') is False


def test_build_image_download_candidates_prefers_large_for_sinaimg_mw600() -> None:
    candidates = build_image_download_candidates('http://wx1.sinaimg.cn/mw600/abc.jpg')

    assert candidates == [
        'http://wx1.sinaimg.cn/large/abc.jpg',
        'http://wx1.sinaimg.cn/mw600/abc.jpg',
    ]


def test_build_image_download_candidates_prefers_large_for_wangmoyu_mw600() -> None:
    candidates = build_image_download_candidates('http://img.wangmoyu.com/mw600/abc.png')

    assert candidates == [
        'http://img.wangmoyu.com/large/abc.png',
        'http://img.wangmoyu.com/mw600/abc.png',
    ]


def test_build_image_download_candidates_prefers_large_for_wangmoyu_mw1024() -> None:
    candidates = build_image_download_candidates('http://img.wangmoyu.com/mw1024/abc.png')

    assert candidates == [
        'http://img.wangmoyu.com/large/abc.png',
        'http://img.wangmoyu.com/mw1024/abc.png',
    ]


def test_build_image_download_candidates_preserves_query_for_mw1024() -> None:
    candidates = build_image_download_candidates('http://wx4.sinaimg.cn/mw1024/abc.jpg?x=1&y=2')

    assert candidates == [
        'http://wx4.sinaimg.cn/large/abc.jpg?x=1&y=2',
        'http://wx4.sinaimg.cn/mw1024/abc.jpg?x=1&y=2',
    ]


def test_build_image_download_candidates_keeps_single_when_not_mw600() -> None:
    candidates = build_image_download_candidates('http://wx1.sinaimg.cn/large/abc.jpg')

    assert candidates == ['http://wx1.sinaimg.cn/large/abc.jpg']


def test_build_image_download_candidates_keeps_single_for_unknown_host() -> None:
    candidates = build_image_download_candidates('https://cdn.example.com/mw600/abc.jpg')

    assert candidates == ['https://cdn.example.com/mw600/abc.jpg']


def test_notify_summary_enqueues_structured_payload(monkeypatch) -> None:
    notifications: list[dict[str, object]] = []

    async def _fake_enqueue_notification(**payload) -> None:  # noqa: ANN003
        notifications.append(payload)

    monkeypatch.setattr(jandan_module, 'enqueue_notification', _fake_enqueue_notification)

    job = Jandan.__new__(Jandan)
    asyncio.run(
        job._notify_summary(
            total_downloaded=3,
            total_api_images=8,
            fav_type_stats=[(1, 2, 5, 4, 3)],
        ),
    )

    assert notifications == [
        {
            'kind': 'summary',
            'source': 'jandan',
            'title': 'Jandan update completed',
            'body': 'Downloaded 3 images from 8 API images.',
            'payload': {
                'total_downloaded': 3,
                'total_api_images': 8,
                'fav_type_stats': [
                    {
                        'fav_type': 1,
                        'page_count': 2,
                        'api_total_count': 5,
                        'collected_count': 4,
                        'downloaded_count': 3,
                    },
                ],
            },
        },
    ]

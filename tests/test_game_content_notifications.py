# ruff: noqa: ANN001, ANN003, INP001, S101, SLF001

import asyncio

import src.web.bd2 as bd2_module
import src.web.nikke as nikke_module
from src.tool.content_discovery import CharacterSnapshot


def test_nikke_new_character_notification_includes_character_and_skin_names(tmp_path, monkeypatch) -> None:
    notifications: list[dict[str, object]] = []

    async def _fake_enqueue_notification(**payload) -> None:
        notifications.append(payload)

    monkeypatch.setattr(nikke_module, 'enqueue_notification', _fake_enqueue_notification)
    crawler = nikke_module.Nikke(path=tmp_path, client=object())
    crawler._notify_discoveries = True
    character = {
        'content_id': 101,
        'title': 'Rapi',
        'skins': [
            {'name': 'default', 'title': 'Default'},
            {'name': 'summer', 'title': 'Summer Vacation'},
        ],
    }

    asyncio.run(crawler._notify_content_discovery(snapshot=CharacterSnapshot(exists=False, character=None), character=character))

    assert notifications == [
        {
            'kind': 'content_discovered',
            'source': 'nikke',
            'header': 'Nikke',
            'title': 'New character: Rapi',
            'body': 'Character: Rapi\nSkin: Default, Summer Vacation',
            'link_url': 'https://www.gamekee.com/nikke/tj/101.html',
            'payload': {
                'content_id': 101,
                'character_name': 'Rapi',
                'skin_names': ['Default', 'Summer Vacation'],
                'discovery': 'character',
            },
        },
    ]


def test_nikke_existing_character_only_notifies_for_new_skin(tmp_path, monkeypatch) -> None:
    notifications: list[dict[str, object]] = []

    async def _fake_enqueue_notification(**payload) -> None:
        notifications.append(payload)

    monkeypatch.setattr(nikke_module, 'enqueue_notification', _fake_enqueue_notification)
    crawler = nikke_module.Nikke(path=tmp_path, client=object())
    crawler._notify_discoveries = True
    previous = CharacterSnapshot(
        exists=True,
        character={'skins': [{'name': 'default', 'title': 'Default'}]},
    )
    character = {
        'content_id': 101,
        'title': 'Rapi',
        'skins': [
            {'name': 'default', 'title': 'Default (updated translation)'},
            {'name': 'summer', 'title': 'Summer Vacation'},
        ],
    }

    asyncio.run(crawler._notify_content_discovery(snapshot=previous, character=character))

    assert len(notifications) == 1
    assert notifications[0]['header'] == 'Nikke'
    assert notifications[0]['title'] == 'New skin: Rapi'
    assert notifications[0]['body'] == 'Character: Rapi\nSkin: Summer Vacation'
    assert notifications[0]['payload']['skin_names'] == ['Summer Vacation']


def test_bd2_existing_character_notification_includes_new_skin_name(tmp_path, monkeypatch) -> None:
    notifications: list[dict[str, object]] = []

    async def _fake_enqueue_notification(**payload) -> None:
        notifications.append(payload)

    monkeypatch.setattr(bd2_module, 'enqueue_notification', _fake_enqueue_notification)
    crawler = bd2_module.BD2(path=tmp_path, client=object(), viewer_resources=())
    crawler._notify_discoveries = True
    previous = CharacterSnapshot(
        exists=True,
        character={'costumes': [{'style_name': 'office', 'title': 'Office Worker'}]},
    )
    character = {
        'content_id': 202,
        'title': 'Justia',
        'costumes': [
            {'style_name': 'office', 'title': 'Office Worker'},
            {'style_name': 'summer', 'title': 'Summer Knight'},
        ],
    }

    asyncio.run(crawler._notify_content_discovery(snapshot=previous, character=character))

    assert len(notifications) == 1
    assert notifications[0]['header'] == 'BD2'
    assert notifications[0]['title'] == 'New skin: Justia'
    assert notifications[0]['body'] == 'Character: Justia\nSkin: Summer Knight'
    assert notifications[0]['link_url'] == 'https://www.gamekee.com/zsca2/tj/202.html'


def test_discovery_notifications_are_suppressed_without_an_existing_archive_baseline(tmp_path, monkeypatch) -> None:
    async def _unexpected_enqueue_notification(**_payload) -> None:
        raise AssertionError

    monkeypatch.setattr(bd2_module, 'enqueue_notification', _unexpected_enqueue_notification)
    crawler = bd2_module.BD2(path=tmp_path, client=object(), viewer_resources=())

    asyncio.run(
        crawler._notify_content_discovery(
            snapshot=CharacterSnapshot(exists=False, character=None),
            character={'content_id': 202, 'title': 'Justia', 'costumes': [{'style_name': 'default', 'title': 'Default'}]},
        ),
    )

# ruff: noqa: ANN001, ANN002, ANN003, ANN202, EM101, INP001, PLR2004, S101, S106, SLF001, TRY003

import asyncio
import json
from pathlib import Path

import pytest

import src.service.jobs as jobs_module
import src.web.twitter as twitter_module
from src.api.archive import ARCHIVE_SOURCES, _external_url
from src.api.schemas import JobRequestTarget
from src.core import settings
from src.web.twitter import Twitter, build_command, parse_sidecar


def _configure_twitter(**updates: object) -> settings.Twitter:
    """Mutate the pinned settings snapshot that Twitter() reads in __init__."""
    cfg = settings.load().web.twitter
    for key, value in updates.items():
        setattr(cfg, key, value)
    return cfg


def _runnable_cfg(**updates: object) -> settings.Twitter:
    cookiecloud = settings.CookieCloud(server_url='https://cc.example', uuid='u', password='pw')
    return _configure_twitter(username='me', cookiecloud=cookiecloud, **updates)


def _option(command: list[str], key: str) -> str | None:
    """The value of a ``--option key=value`` pair, or None if the key is absent."""
    prefix = f'{key}='
    return next((arg[len(prefix) :] for arg in command if arg.startswith(prefix)), None)


def _sidecar_payload(**updates) -> dict:
    payload = {
        'tweet_id': 1234567890123456789,
        'num': 2,
        'author': {'id': 42, 'name': 'artist', 'nick': 'The Artist'},
        'content': 'a caption',
        'date': '2026-05-01 12:34:56',
        'type': 'photo',
        'extension': 'jpg',
    }
    payload.update(updates)
    return payload


class _FakeDatabase:
    """Records the SQL a run issues instead of talking to PostgreSQL."""

    def __init__(self) -> None:
        self.calls: list[tuple[str, tuple]] = []
        self.rows: list[dict] = []

    async def query_db(self, query: str, params: tuple = ()) -> list[dict]:
        self.calls.append((query, params))
        return self.rows if query.strip().upper().startswith('SELECT') else []


# ---------- configuration ----------


def test_a_source_without_a_username_is_not_runnable() -> None:
    cfg = _configure_twitter(cookiecloud=settings.CookieCloud(server_url='https://cc.example', uuid='u', password='pw'))

    assert cfg.validate_runnable() == ['username']


def test_missing_cookiecloud_fields_are_named_individually() -> None:
    cfg = _configure_twitter(username='me', cookiecloud=settings.CookieCloud(server_url='https://cc.example'))

    assert cfg.validate_runnable() == ['cookiecloud.uuid', 'cookiecloud.password']


def test_a_fully_configured_source_is_runnable() -> None:
    assert _runnable_cfg().validate_runnable() == []


def test_a_pasted_username_keeps_working_with_its_at_sign() -> None:
    assert settings.Twitter(username='  @Me_1 ').username == 'Me_1'


def test_a_username_with_a_slash_is_rejected_rather_than_pasted_into_the_url() -> None:
    with pytest.raises(ValueError, match='username'):
        settings.Twitter(username='me/likes')


def test_abort_after_must_leave_room_for_at_least_one_file() -> None:
    with pytest.raises(ValueError, match='abort_after'):
        settings.Twitter(abort_after=0)


# ---------- gallery-dl invocation ----------


def test_the_backfill_run_walks_the_whole_timeline() -> None:
    cfg = _runnable_cfg(path=Path('/data/twitter'), abort_after=20)

    command = build_command(cfg, cookie_path=Path('/run/cookies/c.txt'), archive_path=Path('/data/a.db'), incremental=False)

    # Stopping at the first known file would leave everything below it unreachable.
    assert _option(command, 'extractor.twitter.skip') is None
    assert command[-1] == 'https://x.com/me/likes'
    assert '--destination' in command
    assert command[command.index('--destination') + 1] == '/data/twitter'
    assert command[command.index('--cookies') + 1] == '/run/cookies/c.txt'
    assert command[command.index('--download-archive') + 1] == '/data/a.db'
    assert '--write-metadata' in command


def test_an_incremental_run_stops_after_the_configured_run_of_known_files() -> None:
    cfg = _runnable_cfg(abort_after=7)

    command = build_command(cfg, cookie_path=Path('/run/cookies/c.txt'), archive_path=Path('/a.db'), incremental=True)

    assert _option(command, 'extractor.twitter.skip') == 'abort:7'


def test_the_directory_option_is_json_so_gallery_dl_reads_it_as_a_list() -> None:
    command = build_command(_runnable_cfg(), cookie_path=Path('/c'), archive_path=Path('/a'), incremental=False)

    assert json.loads(_option(command, 'extractor.twitter.directory') or '') == ['{author[name]}']


def test_liked_retweets_are_filed_under_the_original_author_by_default() -> None:
    command = build_command(_runnable_cfg(), cookie_path=Path('/c'), archive_path=Path('/a'), incremental=False)

    assert _option(command, 'extractor.twitter.retweets') == 'original'


def test_turning_off_retweets_skips_them_entirely() -> None:
    cfg = _runnable_cfg(include_retweets=False)

    command = build_command(cfg, cookie_path=Path('/c'), archive_path=Path('/a'), incremental=False)

    assert _option(command, 'extractor.twitter.retweets') == 'false'


def test_the_proxy_is_passed_through_only_when_one_is_set() -> None:
    without = build_command(_runnable_cfg(proxy=''), cookie_path=Path('/c'), archive_path=Path('/a'), incremental=False)
    assert '--proxy' not in without

    cfg = _runnable_cfg(proxy='http://proxy.example:8080')
    with_proxy = build_command(cfg, cookie_path=Path('/c'), archive_path=Path('/a'), incremental=False)
    assert with_proxy[with_proxy.index('--proxy') + 1] == 'http://proxy.example:8080'


def test_the_request_interval_reaches_gallery_dl() -> None:
    cfg = _runnable_cfg(sleep_request_seconds=3.5)

    command = build_command(cfg, cookie_path=Path('/c'), archive_path=Path('/a'), incremental=False)

    assert _option(command, 'extractor.twitter.sleep-request') == '3.5'


# ---------- sidecar parsing ----------


def test_a_sidecar_becomes_a_row_with_a_path_relative_to_the_collection() -> None:
    root = Path('/data/twitter')

    row = parse_sidecar(_sidecar_payload(), media_path=root / 'artist' / 'pic.jpg', root=root)

    assert row is not None
    assert row['tweet_id'] == 1234567890123456789
    assert row['num'] == 2
    assert row['author'] == 'artist'
    assert row['author_nick'] == 'The Artist'
    assert row['author_id'] == '42'
    assert row['content'] == 'a caption'
    assert row['tweet_date'] == '2026-05-01 12:34:56'
    assert row['media_type'] == 'photo'
    # Relative, so moving the collection directory does not invalidate the archive.
    assert row['local_path'] == 'artist/pic.jpg'
    assert json.loads(row['metadata'])['tweet_id'] == 1234567890123456789


def test_a_file_outside_the_collection_keeps_its_absolute_path() -> None:
    row = parse_sidecar(_sidecar_payload(), media_path=Path('/elsewhere/pic.jpg'), root=Path('/data/twitter'))

    assert row is not None
    assert row['local_path'] == '/elsewhere/pic.jpg'


def test_an_author_gallery_dl_could_not_resolve_leaves_empty_strings_not_none() -> None:
    row = parse_sidecar(_sidecar_payload(author={}), media_path=Path('/p.jpg'), root=Path('/data'))

    assert row is not None
    assert row['author'] == ''
    assert row['author_id'] == ''


def test_json_that_is_not_a_tweet_is_ignored_rather_than_stored() -> None:
    assert parse_sidecar({'hello': 'world'}, media_path=Path('/p.jpg'), root=Path('/data')) is None
    assert parse_sidecar(_sidecar_payload(num='2'), media_path=Path('/p.jpg'), root=Path('/data')) is None


# ---------- ingest ----------


def _write_sidecar(directory: Path, name: str, payload: dict) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    media = directory / name
    media.write_bytes(b'binary')
    sidecar = directory / f'{name}.json'
    sidecar.write_text(json.dumps(payload), encoding='utf-8')
    return sidecar


def test_ingest_inserts_one_row_per_sidecar_and_clears_the_queue(monkeypatch, tmp_path) -> None:
    _runnable_cfg(path=tmp_path)
    fake_db = _FakeDatabase()
    monkeypatch.setattr(twitter_module, 'database', fake_db)

    first = _write_sidecar(tmp_path / 'artist', 'a.jpg', _sidecar_payload(num=1))
    second = _write_sidecar(tmp_path / 'other', 'b.mp4', _sidecar_payload(tweet_id=999, num=1, type='video'))

    inserted = asyncio.run(Twitter()._ingest_sidecars())

    assert inserted == 2
    # The sidecar is the queue entry: it goes away only once its row is in.
    assert not first.exists()
    assert not second.exists()
    assert (tmp_path / 'artist' / 'a.jpg').exists()
    paths = {call[1][8] for call in fake_db.calls}
    assert paths == {'artist/a.jpg', 'other/b.mp4'}


def test_ingest_leaves_an_unparseable_sidecar_alone_instead_of_losing_it(monkeypatch, tmp_path) -> None:
    _runnable_cfg(path=tmp_path)
    monkeypatch.setattr(twitter_module, 'database', _FakeDatabase())

    broken = tmp_path / 'artist' / 'a.jpg.json'
    broken.parent.mkdir(parents=True)
    broken.write_text('{not json', encoding='utf-8')

    assert asyncio.run(Twitter()._ingest_sidecars()) == 0
    assert broken.exists()


# ---------- run ----------


def test_an_unconfigured_source_does_nothing_rather_than_launching_gallery_dl(monkeypatch) -> None:
    _configure_twitter(username='', cookiecloud=settings.CookieCloud())

    def _explode(*_args, **_kwargs):
        raise AssertionError('should not touch the database or the network')

    monkeypatch.setattr(twitter_module, 'database', _explode)

    asyncio.run(Twitter().update())


# ---------- registration ----------


def test_the_job_is_registered_and_parked_until_configured() -> None:
    fake_config = settings.Settings()
    fake_config.web.twitter.enabled = True

    job = next(job for job in jobs_module.build_jobs(fake_config) if job.key == 'twitter')

    assert job.name == 'X (Twitter)'
    assert job.section == 'web.twitter'
    assert job.required_commands == ('gallery-dl',)
    assert job.factory is jobs_module.Twitter
    # Enabled in the UI but with no credentials, so it stays out of the scheduler.
    assert job.enabled is False
    assert 'username' in job.missing_fields


def test_api_job_enum_includes_twitter() -> None:
    assert JobRequestTarget.TWITTER.value == 'twitter'


def test_the_archive_links_a_row_to_the_tweet_without_needing_the_handle() -> None:
    source = ARCHIVE_SOURCES['twitter']

    assert _external_url(source, {'tweet_id': 1234, 'num': 1}) == 'https://x.com/i/status/1234'

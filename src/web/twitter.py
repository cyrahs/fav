"""Liked tweets on X, downloaded through gallery-dl.

X sells no affordable read access to a likes timeline, so the crawl drives the
gallery-dl CLI against ``x.com/<username>/likes`` with the browser session this
account is signed in with. gallery-dl owns the parts that break when X changes:
the GraphQL endpoints, the transaction-id header, and the 429 back-off. This
module owns scheduling, the session, and the archive database.

Two pieces of state make a run incremental:

* gallery-dl's own download archive, in the collection directory, decides which
  files it has already fetched.
* ``twitter_state`` records whether the first full walk of the timeline ever
  finished. Until it has, runs walk to the end so the history is filled in;
  after that they stop early once they have seen ``abort_after`` known files in
  a row.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import shutil
import tempfile
from pathlib import Path
from typing import Any

from src.core import logger, settings
from src.tool import CookieCloudClient, database
from src.tool.cookiecloud import TWITTER_PROFILE
from src.tool.notifications import enqueue_notification

log = logger.get('twitter')

# gallery-dl writes one JSON sidecar next to every file it downloads. They double as
# the ingest queue: a sidecar is removed only once its row is in the database, so a
# run that dies mid-ingest is picked up by the next one.
SIDECAR_SUFFIX = '.json'
ARCHIVE_FILENAME = '.gallery-dl-archive.db'
COOKIE_FILENAME = 'twitter-cookies.txt'
BACKFILL_STATE_KEY = 'backfill_complete'

# Matches the repository's `[uploader]title [media_id].ext` shape. The tweet text is
# deliberately absent: it is multi-line, mutable, and already stored in the database
# for the archive UI to search.
FILENAME_FORMAT = '[{author[name]}] {date:%Y-%m-%d} [{tweet_id}_{num}].{extension}'
DIRECTORY_FORMAT = ('{author[name]}',)

# gallery-dl's own media types. X stores a GIF as a short video, so it belongs here.
VIDEO_TYPES = frozenset({'video', 'animated_gif'})

# gallery-dl ORs an exit code per failure class; 16 is authentication/authorization.
EXIT_AUTH = 16
# How much of gallery-dl's stderr to carry into a failure notification.
_ERROR_TAIL_LINES = 5


class TwitterDownloadError(RuntimeError):
    def __init__(self, message: str, *, notification_dedupe_key: str = '') -> None:
        super().__init__(message)
        self.notification_dedupe_key = notification_dedupe_key


def build_command(
    cfg: settings.Twitter,
    *,
    cookie_path: Path,
    archive_path: Path,
    incremental: bool,
) -> list[str]:
    """The gallery-dl invocation for one run.

    ``incremental`` stops the run once ``abort_after`` consecutive files are already
    in the archive. It is off during backfill, where stopping at the top of the
    timeline would leave the older half permanently unreachable.
    """
    command = [
        'gallery-dl',
        '--destination',
        str(cfg.path),
        '--cookies',
        str(cookie_path),
        '--download-archive',
        str(archive_path),
        '--write-metadata',
        '--option',
        f'extractor.twitter.directory={json.dumps(list(DIRECTORY_FORMAT))}',
        '--option',
        f'extractor.twitter.filename={FILENAME_FORMAT}',
        '--option',
        f'extractor.twitter.sleep-request={cfg.sleep_request_seconds}',
        # "original" files a liked retweet under the account that wrote it rather
        # than the account that boosted it.
        '--option',
        f'extractor.twitter.retweets={"original" if cfg.include_retweets else "false"}',
        # Stated rather than inherited: this is gallery-dl's own default, and leaving
        # it implicit would make an upstream change to it silent here.
        '--option',
        f'extractor.twitter.videos={"true" if cfg.include_videos else "false"}',
    ]
    if incremental:
        command.extend(['--option', f'extractor.twitter.skip=abort:{cfg.abort_after}'])
    if cfg.proxy:
        command.extend(['--proxy', cfg.proxy])
    command.append(f'https://x.com/{cfg.username}/likes')
    return command


def parse_sidecar(payload: dict[str, Any], *, media_path: Path, root: Path) -> dict[str, Any] | None:
    """Turn one gallery-dl metadata sidecar into a row for the ``twitter`` table.

    Returns ``None`` for a sidecar without the identifiers the table is keyed on,
    which is how a metadata file from some other tool gets ignored.
    """
    tweet_id = payload.get('tweet_id')
    num = payload.get('num')
    if not isinstance(tweet_id, int) or not isinstance(num, int):
        return None

    author = payload.get('author')
    author = author if isinstance(author, dict) else {}
    with contextlib.suppress(ValueError):
        media_path = media_path.relative_to(root)

    return {
        'tweet_id': tweet_id,
        'num': num,
        'author': str(author.get('name') or ''),
        'author_nick': str(author.get('nick') or ''),
        'author_id': str(author.get('id') or ''),
        'content': str(payload.get('content') or ''),
        'tweet_date': str(payload.get('date') or ''),
        'media_type': str(payload.get('type') or ''),
        'local_path': str(media_path),
        'metadata': json.dumps(payload, ensure_ascii=False, default=str),
    }


class Twitter:
    def __init__(self) -> None:
        self.cfg = settings.load().web.twitter

    async def _ensure_table(self) -> None:
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS twitter (
                tweet_id BIGINT NOT NULL,
                num INTEGER NOT NULL,
                author TEXT NOT NULL DEFAULT '',
                author_nick TEXT NOT NULL DEFAULT '',
                author_id TEXT NOT NULL DEFAULT '',
                content TEXT NOT NULL DEFAULT '',
                tweet_date TEXT NOT NULL DEFAULT '',
                media_type TEXT NOT NULL DEFAULT '',
                local_path TEXT NOT NULL DEFAULT '',
                metadata JSONB,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (tweet_id, num)
            );
        """)
        await database.query_db("""
            CREATE TABLE IF NOT EXISTS twitter_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
        """)

    async def _backfill_complete(self) -> bool:
        rows = await database.query_db('SELECT value FROM twitter_state WHERE key = ?;', (BACKFILL_STATE_KEY,))
        return bool(rows) and str(rows[0].get('value')) == '1'

    async def _mark_backfill_complete(self) -> None:
        await database.query_db(
            'INSERT OR IGNORE INTO twitter_state (key, value) VALUES (?, ?);',
            (BACKFILL_STATE_KEY, '1'),
        )

    def _write_cookie_file(self, cookie_path: Path) -> None:
        """Refresh the X session from CookieCloud, so it tracks the browser."""
        cc_cfg = settings.resolve_cookiecloud(self.cfg.cookiecloud)
        client = CookieCloudClient(cc_cfg.server_url, cc_cfg.uuid, cc_cfg.password)
        try:
            client.save_to_netscape_format(TWITTER_PROFILE.domains, cookie_path)
        finally:
            client.client.close()

    async def _run_gallery_dl(self, command: list[str]) -> tuple[int, list[str]]:
        """Run gallery-dl to completion, returning its exit code and the tail of its errors.

        A backfill prints one stdout line per downloaded file, so those are logged at
        debug; stderr carries the throttling and authentication messages that explain
        a bad exit code, so it is logged and kept for the failure notification.
        """
        log.info('Running gallery-dl for %s likes', self.cfg.username)
        process = await asyncio.create_subprocess_exec(
            *command,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        assert process.stdout is not None and process.stderr is not None  # noqa: S101, PT018 - both are PIPEs

        async def drain_stdout() -> None:
            async for raw_line in process.stdout:  # type: ignore[union-attr]
                line = raw_line.decode(errors='replace').rstrip()
                if line:
                    log.debug('gallery-dl: %s', line)

        errors: list[str] = []

        async def drain_stderr() -> None:
            async for raw_line in process.stderr:  # type: ignore[union-attr]
                line = raw_line.decode(errors='replace').rstrip()
                if not line:
                    continue
                log.warning('gallery-dl: %s', line)
                errors.append(line)
                del errors[:-_ERROR_TAIL_LINES]

        await asyncio.gather(drain_stdout(), drain_stderr())
        return await process.wait(), errors

    def _destination_root(self, media_type: str) -> Path:
        """Which configured directory a file of this type belongs under."""
        if self.cfg.video_path is not None and media_type in VIDEO_TYPES:
            return self.cfg.video_path
        return self.cfg.path

    def _destination_path(self, media_path: Path, root: Path) -> Path:
        """Where a downloaded file belongs once its media type is known.

        gallery-dl resolves a directory once per tweet, before it knows whether the
        files in it are photos or videos, so the split cannot be expressed in its
        config and happens here instead.
        """
        if root == self.cfg.path:
            return media_path
        try:
            return root / media_path.relative_to(self.cfg.path)
        except ValueError:
            log.warning('Leaving %s where it is: not under the collection directory', media_path)
            return media_path

    @staticmethod
    def _relocate(media_path: Path, destination: Path) -> None:
        """Move a file to its media type's directory, if it is not already there.

        Safe with respect to re-downloading: gallery-dl's archive is keyed on the
        tweet rather than the path, so a relocated file is never fetched again. A
        missing source means an earlier run moved it and died before clearing the
        sidecar, which is why that is not an error.
        """
        if destination == media_path or not media_path.exists():
            return
        destination.parent.mkdir(parents=True, exist_ok=True)
        # shutil.move, not rename: the video directory is usually a different mount.
        shutil.move(str(media_path), str(destination))

    async def _ingest_sidecars(self) -> int:
        """Insert a row per downloaded file, then drop the sidecar that produced it."""
        sidecars = sorted(self.cfg.path.rglob(f'*{SIDECAR_SUFFIX}'))
        inserted = 0
        for sidecar in sidecars:
            media_path = sidecar.with_suffix('')
            try:
                payload = json.loads(sidecar.read_text(encoding='utf-8'))
            except (OSError, ValueError) as exc:
                log.warning('Skipping unreadable sidecar %s: %s', sidecar, exc)
                continue
            if not isinstance(payload, dict):
                log.warning('Skipping sidecar without tweet identifiers: %s', sidecar)
                continue

            root = self._destination_root(str(payload.get('type') or ''))
            destination = self._destination_path(media_path, root)
            # Recorded relative to the directory the file actually lives in, which the
            # media_type column identifies. Parsed before the move so that a sidecar
            # that turns out not to describe a tweet leaves the file untouched.
            row = parse_sidecar(payload, media_path=destination, root=root)
            if row is None:
                log.warning('Skipping sidecar without tweet identifiers: %s', sidecar)
                continue
            await asyncio.to_thread(self._relocate, media_path, destination)

            await database.query_db(
                """
                INSERT INTO twitter (
                    tweet_id, num, author, author_nick, author_id,
                    content, tweet_date, media_type, local_path, metadata
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT (tweet_id, num) DO UPDATE SET
                    local_path = EXCLUDED.local_path,
                    metadata = EXCLUDED.metadata;
                """,
                (
                    row['tweet_id'],
                    row['num'],
                    row['author'],
                    row['author_nick'],
                    row['author_id'],
                    row['content'],
                    row['tweet_date'],
                    row['media_type'],
                    row['local_path'],
                    row['metadata'],
                ),
            )
            await asyncio.to_thread(sidecar.unlink, missing_ok=True)
            inserted += 1
        return inserted

    async def _notify_summary(self, *, downloaded: int, backfilling: bool) -> None:
        try:
            await enqueue_notification(
                kind='summary',
                source='twitter',
                header='X',
                title='Likes update completed',
                body=f'Downloaded {downloaded} files from liked tweets.',
                payload={'downloaded': downloaded, 'backfilling': backfilling, 'username': self.cfg.username},
            )
        except Exception as exc:  # noqa: BLE001
            log.warning('Failed to enqueue twitter summary notification: %s', exc)

    async def update(self) -> None:
        missing = self.cfg.validate_runnable()
        if missing:
            log.warning('Twitter is not configured (missing %s), skip update', ', '.join(missing))
            return

        await self._ensure_table()
        await asyncio.to_thread(self.cfg.path.mkdir, parents=True, exist_ok=True)
        if self.cfg.video_path is not None:
            await asyncio.to_thread(self.cfg.video_path.mkdir, parents=True, exist_ok=True)

        backfill_complete = await self._backfill_complete()
        with tempfile.TemporaryDirectory() as tmp_dir:
            cookie_path = Path(tmp_dir) / COOKIE_FILENAME
            await asyncio.to_thread(self._write_cookie_file, cookie_path)
            command = build_command(
                self.cfg,
                cookie_path=cookie_path,
                archive_path=self.cfg.path / ARCHIVE_FILENAME,
                incremental=backfill_complete,
            )
            returncode, errors = await self._run_gallery_dl(command)

        # Ingest whatever was downloaded before judging the exit code: a run that
        # died halfway still fetched files, and their sidecars are on disk.
        downloaded = await self._ingest_sidecars()

        if returncode & EXIT_AUTH:
            msg = 'gallery-dl could not authenticate with X. Sign in again in the browser so CookieCloud picks up a fresh session.'
            raise TwitterDownloadError(msg, notification_dedupe_key='twitter:auth')
        if returncode != 0:
            detail = ' | '.join(errors)
            msg = f'gallery-dl exited with code {returncode}: {detail}' if detail else f'gallery-dl exited with code {returncode}'
            raise TwitterDownloadError(msg, notification_dedupe_key=f'twitter:exit:{returncode}')

        if not backfill_complete:
            # A clean exit without an early abort means the whole likes timeline was
            # walked, so later runs can stop as soon as they recognise the top of it.
            await self._mark_backfill_complete()
            log.info('Twitter likes backfill finished; later runs stop after %d known files', self.cfg.abort_after)

        log.info('Twitter downloaded %d new files', downloaded)
        if downloaded:
            await self._notify_summary(downloaded=downloaded, backfilling=not backfill_complete)

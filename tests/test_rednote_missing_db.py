# ruff: noqa: INP001, S101, SLF001

"""The retire-a-deleted-note bookkeeping, against a real PostgreSQL.

The rule is carried by one statement -- an upsert whose `DO UPDATE` is guarded by a
`WHERE` on the previous run id -- and that guard is the whole design: it is what makes
"three runs agreed" mean three runs rather than three sightings. A fake database
cannot check it, because a fake is written from the same understanding as the code.

CI runs these against the postgres service in .github/workflows/ci.yml.
They skip when there is no database to talk to, so the suite still runs on a laptop.

Each test is *one* `asyncio.run` that closes the pool on its way out. `src/tool/
database.py` keys its pool by event loop, so a test that reached for the database
from several `asyncio.run` calls would leave a pool behind for each one, holding
connections nobody will ever close.
"""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING, Any

import pytest

from src.core import settings
from src.tool import database
from src.web.rednote import RedNote

if TYPE_CHECKING:
    from collections.abc import Awaitable, Callable

# Distinctive enough that a failed run's leftovers are recognisable, and scoped so the
# tests can clean up after themselves without touching anything real.
PREFIX = 'test-missing-'
RETIRE_AT = 3
# The pattern is bound rather than inlined: psycopg reads a literal `%` in the SQL as
# the start of a placeholder, so `LIKE 'test-missing-%'` is a syntax error to it.
_CLEANUP_SQL = 'DELETE FROM rednote_missing WHERE note_id LIKE ?;'
_CLEANUP_PARAMS = (f'{PREFIX}%',)

# Asked once per session. Without the cache every test pays the connect timeout on a
# machine with no database, which is most of them.
_REACHABLE: bool | None = None


def _run_once[T](body: Callable[[], Awaitable[T]]) -> T:
    """Run one async body on its own loop and take the pool down with it."""

    async def main() -> T:
        try:
            return await body()
        finally:
            await database.close_pool()

    return asyncio.run(main())


def _database_available() -> bool:
    global _REACHABLE
    if _REACHABLE is None:
        try:
            _run_once(lambda: database.query_db('SELECT 1;'))
        except Exception:  # noqa: BLE001 - any failure to reach it means these cannot run
            _REACHABLE = False
        else:
            _REACHABLE = True
    return _REACHABLE


def in_database[T](body: Callable[[RedNote], Awaitable[T]]) -> T:
    """Set the table up, hand a worker to ``body``, and clear up after it."""
    if not _database_available():
        pytest.skip('No PostgreSQL to talk to; these run in CI.')

    worker = RedNote.__new__(RedNote)
    worker.cfg = settings.load().web.rednote

    async def main() -> T:
        await worker._ensure_table()
        await database.query_db(_CLEANUP_SQL, _CLEANUP_PARAMS)
        try:
            return await body(worker)
        finally:
            await database.query_db(_CLEANUP_SQL, _CLEANUP_PARAMS)

    return _run_once(main)


async def _runs_recorded(note_id: str) -> int:
    rows = await database.query_db('SELECT runs FROM rednote_missing WHERE note_id = ?;', (note_id,))
    return int(rows[0]['runs']) if rows else 0


def test_a_second_sighting_in_the_same_run_does_not_count_twice() -> None:
    """The same note can come round twice in one walk; that is one run's evidence."""

    async def body(job: RedNote) -> tuple[int, set[str]]:
        note_id = f'{PREFIX}same-run'
        for _ in range(3):
            await job._record_missing(note_id, run_id='run-1')
        return await _runs_recorded(note_id), await job._retired_note_ids([note_id])

    runs, retired = in_database(body)

    assert runs == 1
    assert retired == set()


def test_a_note_is_retired_only_once_three_separate_runs_agree() -> None:
    async def body(job: RedNote) -> tuple[list[set[str]], int, set[str]]:
        note_id = f'{PREFIX}three-runs'
        along_the_way: list[set[str]] = []
        for index in range(1, RETIRE_AT):
            await job._record_missing(note_id, run_id=f'run-{index}')
            along_the_way.append(await job._retired_note_ids([note_id]))
        await job._record_missing(note_id, run_id=f'run-{RETIRE_AT}')
        return along_the_way, await _runs_recorded(note_id), await job._retired_note_ids([note_id])

    along_the_way, runs, retired = in_database(body)

    assert along_the_way == [set()] * (RETIRE_AT - 1), 'retired before three runs agreed'
    assert runs == RETIRE_AT
    assert retired == {f'{PREFIX}three-runs'}


def test_reading_a_note_again_clears_what_was_counted_against_it() -> None:
    """Two unlucky runs plus a hiccup months later must not add up to a retirement."""

    async def body(job: RedNote) -> tuple[int, int, set[str]]:
        note_id = f'{PREFIX}recovered'
        await job._record_missing(note_id, run_id='run-1')
        await job._record_missing(note_id, run_id='run-2')
        await job._clear_missing(note_id)
        after_clearing = await _runs_recorded(note_id)

        await job._record_missing(note_id, run_id='run-3')
        return after_clearing, await _runs_recorded(note_id), await job._retired_note_ids([note_id])

    after_clearing, runs, retired = in_database(body)

    assert after_clearing == 0
    assert runs == 1
    assert retired == set()


def test_the_retired_lookup_answers_for_a_batch_and_ignores_the_rest() -> None:
    async def body(job: RedNote) -> tuple[set[str], set[str]]:
        retired, counting, unseen = f'{PREFIX}retired', f'{PREFIX}counting', f'{PREFIX}unseen'
        for index in range(1, RETIRE_AT + 1):
            await job._record_missing(retired, run_id=f'run-{index}')
        await job._record_missing(counting, run_id='run-1')
        # A batch of nothing must not become `IN ()`, which is a syntax error rather
        # than an empty answer.
        return await job._retired_note_ids([retired, counting, unseen]), await job._retired_note_ids([])

    found, empty = in_database(body)

    assert found == {f'{PREFIX}retired'}
    assert empty == set()


def test_the_first_sighting_is_stamped_and_later_ones_move_only_the_last() -> None:
    """`first_seen_at` is the evidence trail: when this note started looking gone."""

    async def body(job: RedNote) -> tuple[dict[str, Any], dict[str, Any]]:
        note_id = f'{PREFIX}timestamps'
        stamps = 'SELECT first_seen_at, last_seen_at FROM rednote_missing WHERE note_id = ?;'
        await job._record_missing(note_id, run_id='run-1')
        first = (await database.query_db(stamps, (note_id,)))[0]
        await job._record_missing(note_id, run_id='run-2')
        return first, (await database.query_db(stamps, (note_id,)))[0]

    first, second = in_database(body)

    assert second['first_seen_at'] == first['first_seen_at']
    assert second['last_seen_at'] >= first['last_seen_at']

# ruff: noqa: INP001, S101, S608, SLF001

"""The retire-a-deleted-note bookkeeping, against a real PostgreSQL.

The rule is carried by one statement -- an upsert whose `DO UPDATE` is guarded by a
`WHERE` on the previous run id -- and that guard is the whole design: it is what makes
"three runs agreed" mean three runs rather than three sightings. A fake database
cannot check it, because a fake is written from the same understanding as the code.

CI runs these against the postgres service in .github/workflows/docker-build.yml.
They skip when there is no database to talk to, so the suite still runs on a laptop.
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest

from src.core import settings
from src.tool import database
from src.web.rednote import RedNote

# Distinctive enough that a failed run's leftovers are recognisable, and scoped so the
# tests can clean up after themselves without touching anything real.
PREFIX = 'test-missing-'
RETIRE_AT = 3


def _run(coro: Any) -> Any:
    return asyncio.run(coro)


# Asked once per session. Without the cache every test pays the connect timeout on a
# machine with no database, which is most of them.
_REACHABLE: bool | None = None


def _database_available() -> bool:
    global _REACHABLE
    if _REACHABLE is None:
        try:
            _run(database.query_db('SELECT 1;'))
        except Exception:  # noqa: BLE001 - any failure to reach it means these cannot run
            _REACHABLE = False
        else:
            _REACHABLE = True
    return _REACHABLE


@pytest.fixture
def job() -> Any:
    if not _database_available():
        pytest.skip('No PostgreSQL to talk to; these run in CI.')

    worker = RedNote.__new__(RedNote)
    worker.cfg = settings.load().web.rednote
    _run(worker._ensure_table())
    _run(database.query_db(f"DELETE FROM rednote_missing WHERE note_id LIKE '{PREFIX}%';"))
    try:
        yield worker
    finally:
        _run(database.query_db(f"DELETE FROM rednote_missing WHERE note_id LIKE '{PREFIX}%';"))


async def _runs_recorded(note_id: str) -> int:
    rows = await database.query_db('SELECT runs FROM rednote_missing WHERE note_id = ?;', (note_id,))
    return int(rows[0]['runs']) if rows else 0


def test_a_second_sighting_in_the_same_run_does_not_count_twice(job: Any) -> None:
    """The same note can come round twice in one walk; that is one run's evidence."""
    note_id = f'{PREFIX}same-run'

    _run(job._record_missing(note_id, run_id='run-1'))
    _run(job._record_missing(note_id, run_id='run-1'))
    _run(job._record_missing(note_id, run_id='run-1'))

    assert _run(_runs_recorded(note_id)) == 1
    assert _run(job._retired_note_ids([note_id])) == set()


def test_a_note_is_retired_only_once_three_separate_runs_agree(job: Any) -> None:
    note_id = f'{PREFIX}three-runs'

    for index in range(1, RETIRE_AT):
        _run(job._record_missing(note_id, run_id=f'run-{index}'))
        assert _run(job._retired_note_ids([note_id])) == set(), f'retired after only {index} run(s)'

    _run(job._record_missing(note_id, run_id=f'run-{RETIRE_AT}'))

    assert _run(_runs_recorded(note_id)) == RETIRE_AT
    assert _run(job._retired_note_ids([note_id])) == {note_id}


def test_reading_a_note_again_clears_what_was_counted_against_it(job: Any) -> None:
    """Two unlucky runs plus a hiccup months later must not add up to a retirement."""
    note_id = f'{PREFIX}recovered'

    _run(job._record_missing(note_id, run_id='run-1'))
    _run(job._record_missing(note_id, run_id='run-2'))
    _run(job._clear_missing(note_id))

    assert _run(_runs_recorded(note_id)) == 0

    _run(job._record_missing(note_id, run_id='run-3'))

    assert _run(_runs_recorded(note_id)) == 1
    assert _run(job._retired_note_ids([note_id])) == set()


def test_the_retired_lookup_answers_for_a_batch_and_ignores_the_rest(job: Any) -> None:
    retired, counting, unseen = f'{PREFIX}retired', f'{PREFIX}counting', f'{PREFIX}unseen'
    for index in range(1, RETIRE_AT + 1):
        _run(job._record_missing(retired, run_id=f'run-{index}'))
    _run(job._record_missing(counting, run_id='run-1'))

    assert _run(job._retired_note_ids([retired, counting, unseen])) == {retired}
    # A batch of nothing must not become `IN ()`, which is a syntax error rather than
    # an empty answer.
    assert _run(job._retired_note_ids([])) == set()


def test_the_first_sighting_is_stamped_and_later_ones_move_only_the_last(job: Any) -> None:
    """`first_seen_at` is the evidence trail: when this note started looking gone."""
    note_id = f'{PREFIX}timestamps'

    _run(job._record_missing(note_id, run_id='run-1'))
    first = _run(database.query_db('SELECT first_seen_at, last_seen_at FROM rednote_missing WHERE note_id = ?;', (note_id,)))[0]

    _run(job._record_missing(note_id, run_id='run-2'))
    second = _run(database.query_db('SELECT first_seen_at, last_seen_at FROM rednote_missing WHERE note_id = ?;', (note_id,)))[0]

    assert second['first_seen_at'] == first['first_seen_at']
    assert second['last_seen_at'] >= first['last_seen_at']

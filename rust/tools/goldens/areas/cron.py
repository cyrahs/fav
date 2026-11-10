"""APScheduler 3 `CronTrigger.from_crontab` next-fire times (WP2's scheduler).

APScheduler is not standard cron: day-of-week 0 is Monday (so `* * * * 1` fires on
Tuesdays), `7` is rejected, and DST gaps and repeats are resolved its own way. The
worker builds its triggers in `run.py::_sync_schedule` and the API shows the next fire
time through `src/api/ops.py::next_fire_time`; both use `from_crontab(cron, timezone=TZ)`.

A chain is what the scheduler does after each run: `get_next_fire_time(previous, previous)`.
That is normally the next fire time after the previous one, but not always: APScheduler
3.11 can return the previous time again around a DST fall-back (`15 1 * * *` in
America/New_York on 2026-11-01 repeats 01:15-05:00). The goldens record what Python does;
`rust/docs/DIVERGENCES.md` (D3) says what the Rust scheduler does instead, and `strict`
records it: where Python's next time is not after the previous one, the chain restarts
from one second after the previous fire time, through the same Python trigger.

Each case is run twice: with `now` in UTC (`now_zone: "UTC"`), as `ops.next_fire_time`
passes it, and with `now` already in the trigger's zone (`now_zone: "trigger"`), as the
scheduler's `datetime.now(self.timezone)` does. `trigger` is `str(trigger)`, which the
worker compares to decide whether a job needs rescheduling; a cron that does not parse
records the `ValueError` message instead.
"""

from __future__ import annotations

from datetime import UTC, datetime
from typing import Any
from zoneinfo import ZoneInfo

from apscheduler.triggers.cron import CronTrigger

CRONS: list[str] = [
    '*/30 * * * *',
    '0 */6 * * *',
    '*/15 * * * *',
    '0 4 * * 0,2-6',
    '0 4 * * 1',
    '0 9 * * sun',
    '0 9 * * mon-fri',
    '0 0 1 * *',
    '0 0 29 2 *',
    '30 2 * * *',
    '15 1 * * *',
    '5,35 */2 1-7 * *',
    '0 0 * * 6',
    '0 0 * * 7',
    '0 12 31 4 *',
    '61 * * * *',
    '* * *',
    '',
    'bogus cron line here',
    '*/0 * * * *',
    '5-1 * * * *',
    '*/61 * * * *',
    '0 0 * foo *',
    '0 0 L * *',
    '0 0 last * *',
    '0 0 lastday * *',
    '0 0 * * 1-sun',
    '0 0 * * MON-wed/2',
    '1-5/2 * * jan-mar sun',
    '0 0 * jan/3 *',
    '0 0 */40 * *',
    '0 0 1-31/40 * *',
    '0 0 0 * *',
    '0 24 * * *',
    '0 0 * 13 *',
    '0 0 * 0 *',
    '0 0 * * fri,mon',
    '7,3 1,2 * * *',
    '10-50/15 9-17 * * 0-4',
    '0 0 last 2 *',
    '0 0 29-31 * *',
    '0 12 * * sat-sun',
    '0 0 ** * *',
    '0 0 *,5 * *',
    '0 0 5- * *',
    '0 0 -5 * *',
    '00 004 * * *',
    '\t0  12\n * * *  ',
]

TIMEZONES: list[str] = ['UTC', 'Asia/Shanghai', 'America/New_York']

STARTS: list[datetime] = [
    datetime(2026, 11, 9, 22, 56, 54, tzinfo=UTC),
    datetime(2026, 3, 7, 12, 0, tzinfo=UTC),
    datetime(2026, 10, 31, 12, 0, tzinfo=UTC),
    datetime(2026, 12, 31, 23, 59, 59, 999000, tzinfo=UTC),
]

CHAIN = 5


def _chain(trigger: CronTrigger, start: datetime, *, strict: bool) -> list[str]:
    fires: list[str] = []
    previous = None
    now = start
    for _ in range(CHAIN):
        fire = trigger.get_next_fire_time(previous, now)
        if strict and fire is not None and previous is not None and fire.timestamp() <= previous.timestamp():
            restart = datetime.fromtimestamp(previous.timestamp() + 1, trigger.timezone)
            fire = trigger.get_next_fire_time(None, restart)
        if fire is None:
            break
        fires.append(fire.isoformat())
        previous = now = fire
    return fires


def _next_fire_times() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for cron in CRONS:
        for zone in TIMEZONES:
            try:
                trigger = CronTrigger.from_crontab(cron, timezone=ZoneInfo(zone))
            except ValueError as exc:
                cases.append({'cron': cron, 'timezone': zone, 'error': type(exc).__name__, 'message': str(exc)})
                continue
            for start in STARTS:
                for now_zone, now in (('UTC', start), ('trigger', start.astimezone(trigger.timezone))):
                    fires = _chain(trigger, now, strict=False)
                    case: dict[str, Any] = {
                        'cron': cron,
                        'timezone': zone,
                        'trigger': str(trigger),
                        'now': now.isoformat(),
                        'now_zone': now_zone,
                        'fires': fires,
                    }
                    strict = _chain(trigger, now, strict=True)
                    if strict != fires:
                        case['strict'] = strict
                    cases.append(case)
    return cases


GOLDENS = {'scheduler/cron_next_fire.json': _next_fire_times}

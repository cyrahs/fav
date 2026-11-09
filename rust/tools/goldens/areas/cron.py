"""APScheduler 3 `CronTrigger.from_crontab` next-fire times (WP2's scheduler).

APScheduler is not standard cron: day-of-week 0 is Monday (so `* * * * 1` fires on
Tuesdays), `7` is rejected, and DST gaps and repeats are resolved its own way. The
worker builds its triggers in `run.py::_sync_schedule` and the API shows the next fire
time through `src/api/ops.py::next_fire_time`; both use `from_crontab(cron, timezone=TZ)`.

A chain is what the scheduler does after each run: the next fire time strictly after
the previous one, `get_next_fire_time(previous, previous)`.
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
]

TIMEZONES: list[str] = ['UTC', 'Asia/Shanghai', 'America/New_York']

STARTS: list[datetime] = [
    datetime(2026, 11, 9, 22, 56, 54, tzinfo=UTC),
    datetime(2026, 3, 7, 12, 0, tzinfo=UTC),
    datetime(2026, 10, 31, 12, 0, tzinfo=UTC),
    datetime(2026, 12, 31, 23, 59, 59, 999000, tzinfo=UTC),
]

CHAIN = 5


def _next_fire_times() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for cron in CRONS:
        for zone in TIMEZONES:
            try:
                trigger = CronTrigger.from_crontab(cron, timezone=ZoneInfo(zone))
            except ValueError as exc:
                cases.append({'cron': cron, 'timezone': zone, 'error': type(exc).__name__})
                continue
            for start in STARTS:
                fires: list[str] = []
                previous = None
                now = start
                for _ in range(CHAIN):
                    fire = trigger.get_next_fire_time(previous, now)
                    if fire is None:
                        break
                    fires.append(fire.isoformat())
                    previous = now = fire
                cases.append({'cron': cron, 'timezone': zone, 'now': start.isoformat(), 'fires': fires})
    return cases


GOLDENS = {'scheduler/cron_next_fire.json': _next_fire_times}

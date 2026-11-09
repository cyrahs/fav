# Intentional differences from the Python baseline

Every place where the Rust build deliberately behaves differently from Python at `rust/BASELINE`, and every
Python bug found while porting (copied or fixed). Add an entry in the PR that introduces the difference.
The independent review checks that nothing differs without an entry here.

Kinds:

- **dropped**: legacy behaviour the plan removes (SQLite-dialect translation, CookieCloud cleanup,
  old-field startup migrations, tqdm bars).
- **mechanism**: same observable behaviour, different machinery (for example grammers instead of Telethon).
- **observable**: a user, the front end, an API client or stored data can tell the difference.
- **python-bug**: a bug found in Python; say whether Rust copies it or fixes it, and why.

| ID | WP | Kind | Python behaviour | Rust behaviour | Why | Approved by |
| --- | --- | --- | --- | --- | --- | --- |
| D1 | WP0 | mechanism | Three processes (supervisor, API, worker) | One binary; `fav serve` runs API and worker in one process, `fav api` / `fav worker` run them apart | Agreed in the plan (target architecture) | Plan, 2026-11-09 |
| D2 | WP0 | mechanism | Chromium launched by Playwright | Chromium spawned by fav with Playwright's argument list, driven over CDP (`API-COVERAGE.md`) | Agreed in the plan; chromiumoxide's launcher cannot reproduce the argument list | Plan, 2026-11-09 |
| D3 | WP0 (for WP2) | python-bug | APScheduler 3.11 `CronTrigger` can return the previous fire time again on a DST fall-back (`15 1 * * *` in America/New_York repeats 2026-11-01 01:15-05:00, `get_next_fire_time(T, later)` returns `T`), so the scheduler's run-time loop would not advance | Rust fixes it: the next fire time is always strictly after the previous one; the affected cases in `rust/goldens/scheduler/cron_next_fire.json` are the ones whose `fires` repeat a value | A hang is not behaviour worth copying. It needs a DST zone in `TZ`; the fav manifest sets none (the SOPS env secret could), so production is probably unaffected. Max can overrule | Review of #133 |


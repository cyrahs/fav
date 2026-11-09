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

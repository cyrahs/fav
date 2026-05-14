# Project Status

Last updated: 2026-05-14
Maintainer: step-orchestrator coordinator

## Current Objective

- Implement the Azur Lane Live2D/Spine viewer described in `plan.md`.
- Process steps `1-13` with worker/reviewer rounds, keeping `plan.md` local-only and uncommitted.

## Active Plan

- Plan file: `plan.md`
- Requested steps: `1-13`
- Current step: `10. Share Link Serialization`
- Current status: `Done`

## Harness References

- Repo instructions:
  - `AGENTS.md`
- Plan:
  - `plan.md`
- Architecture:
  - `README.md`
- Testing:
  - `pyproject.toml`
  - `tests/`
- Decisions and records:
  - `plan.md`

## Constraints and Decisions

- Code, comments, docstrings, and documentation must be written in English.
- Use the repository `.venv` and run Python tooling through `uv`.
- Do not commit secrets or `config.toml`.
- `plan.md` is ignored through `.git/info/exclude` and must not be staged or committed.
- Viewer work should preserve existing Python automation behavior unless a step explicitly needs integration.

## Relevant Project State

- Key areas: `src/`, `tests/`, `script/`, `README.md`
- Recent approved commits: `step 1: Fetch Source Snapshots` - source snapshot fetchers and tests; `step 2: Normalize Catalog` - catalog normalization and tests; `step 3: Validate Resource URLs` - resource validation and tests; `step 4: Build Pixi Viewer Shell` - isolated Pixi shell and tests; `step 5: Fixed Logical Stage` - 1600x900 stage scaling and tests; `step 6: Live2D Loading And Auto-Fit` - Live2D loader and auto-fit tests; `step 7: Spine Loading And Auto-Fit` - Spine loader and runtime compatibility path; `step 8: Model Selection UI` - catalog picker, search/filtering, and one-model-at-a-time loading; `step 9: Interaction And Transform Persistence` - logical drag/zoom, persisted transforms, and reset-to-auto-fit; `step 10: Share Link Serialization` - compact share links and restore path.
- Pending work: steps `11-13` in `plan.md`.
- Known risks or blockers: the viewer uses a static CDN-based frontend shell; no package-managed frontend build system is present.

## Verification

- Required commands:
  - `uv run ruff format .`
  - `uv run ruff check .`
  - `uv run pytest`
- Last known results:
  - Step 1 approved: `uv run pytest tests/test_azurlane_l2d_sources.py` passed, `uv run pytest` passed, touched-file Ruff passed, repo-wide Ruff blocked by pre-existing unrelated lint.
  - Step 2 worker1: `uv run pytest tests/test_azurlane_l2d_sources.py` passed, `uv run pytest` passed, touched-file Ruff passed.
  - Step 2 reviewer1: rejected unsafe Nagami fallback matching for l2d.su variants such as `/adaerbote_3_fhx/adaerbote_3.model3.json`.
  - Step 2 worker2: `uv run pytest tests/test_azurlane_l2d_sources.py` passed, `uv run pytest` passed, touched-file Ruff passed.
  - Step 2 reviewer2 approved: `uv run pytest tests/test_azurlane_l2d_sources.py` passed, `uv run pytest` passed, touched-file Ruff passed.
  - Step 3 worker1: `uv run pytest tests/test_azurlane_l2d_sources.py` passed, `uv run pytest` passed, touched-file Ruff passed.
  - Step 3 reviewer1: rejected because `*-spine` catalog paths probed `*-spine.skel`/`.atlas` instead of files without the suffix.
  - Step 3 worker2: `uv run pytest tests/test_azurlane_l2d_sources.py` passed, `uv run pytest` passed, touched-file Ruff passed.
  - Step 3 reviewer2 approved: `uv run pytest tests/test_azurlane_l2d_sources.py` passed, `uv run pytest` passed, touched-file Ruff passed; repo-wide Ruff still has unrelated pre-existing failures.
  - Step 4 worker1: `node --test viewer/azurlane/tests/stage-layout.test.js` passed; browser smoke test passed.
  - Step 4 reviewer1 approved: unit/syntax/diff checks passed; coordinator browser smoke test passed through temporary Playwright install.
  - Step 5 worker1: stage-layout tests, syntax checks, browser smoke across desktop/wide/narrow/mobile-like viewports, and `git diff --check` passed.
  - Step 5 reviewer1 approved: stage-layout tests, syntax checks, browser smoke through temporary Playwright install, and `git diff --check` passed.
  - Step 6 worker1: Live2D loader tests, syntax checks, browser smoke with fake runtime, live runtime smoke, and `git diff --check` passed.
  - Step 6 reviewer1 approved: syntax checks, unit tests, CDN URL checks, and coordinator browser smoke with temporary Playwright install passed.
  - Step 7 worker1: Spine loader tests, syntax checks, fake-runtime browser smoke, and `git diff --check` passed; live probe exposed a Spine 3.8 asset/runtime compatibility risk.
  - Step 7 reviewer1: rejected because real `l2d.su` Spine assets identify as Spine 3.8.99 and fail under the pinned 4.2 Pixi v8 Spine runtime.
  - Step 7 worker2: switched to a Spine 3.8-compatible Pixi v8 runtime path; unit tests, fake-runtime browser smoke, and live smoke for three real Spine models passed.
  - Step 7 reviewer2 approved: syntax checks, unit tests, browser smoke, and live runtime smoke for three real Spine models passed.
  - Step 8 reviewer2 approved: syntax checks, focused node tests, browser smoke with temporary Playwright, combined viewer tests, and `git diff --check` passed.
  - Step 8 coordinator verification: `node --check` for touched JS files passed, `node --test viewer/azurlane/tests/model-catalog.test.js viewer/azurlane/tests/live2d-loader.test.js viewer/azurlane/tests/spine-loader.test.js viewer/azurlane/tests/stage-layout.test.js` passed, `NODE_PATH=/tmp/fav-playwright-GGKkpk/node_modules node --test viewer/azurlane/tests/viewer-shell.browser.test.mjs` passed, and `git diff --check` passed.
  - Step 9 reviewer1 approved: syntax checks, focused node tests, browser smoke with temporary Playwright, and `git diff --check` passed.
  - Step 9 coordinator verification: `node --check viewer/azurlane/viewer-shell.js`, `node --check viewer/azurlane/tests/viewer-shell.browser.test.mjs`, focused node tests, browser smoke through temporary Playwright, and `git diff --check` passed.
  - Step 10 reviewer1 approved: syntax checks, share-link tests, focused viewer tests, and `git diff --check` passed.
  - Step 10 coordinator verification: `node --check` for touched JS files passed, focused viewer tests passed, browser smoke through temporary Playwright passed, and `git diff --check` passed.

## Subagent Notes

- Read this file, `plan.md`, and applicable `AGENTS.md` files before editing or reviewing.
- Work only on the active step named in the subagent prompt.
- Do not infer or start future steps.
- Workers must not commit or update `plan.md`.
- Reviewers are read-only and must report findings and a verdict only.

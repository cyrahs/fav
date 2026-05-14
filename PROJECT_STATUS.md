# Project Status

Last updated: 2026-05-14
Maintainer: step-orchestrator coordinator

## Current Objective

- Implement the Azur Lane Live2D/Spine viewer described in `plan.md`.
- Process steps `1-13` with worker/reviewer rounds, keeping `plan.md` local-only and uncommitted.

## Active Plan

- Plan file: `plan.md`
- Requested steps: `1-13`
- Current step: `5. Fixed Logical Stage`
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
- Recent approved commits: `step 1: Fetch Source Snapshots` - source snapshot fetchers and tests; `step 2: Normalize Catalog` - catalog normalization and tests; `step 3: Validate Resource URLs` - resource validation and tests; `step 4: Build Pixi Viewer Shell` - isolated Pixi shell and tests; `step 5: Fixed Logical Stage` - 1600x900 stage scaling and tests.
- Pending work: steps `6-13` in `plan.md`.
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

## Subagent Notes

- Read this file, `plan.md`, and applicable `AGENTS.md` files before editing or reviewing.
- Work only on the active step named in the subagent prompt.
- Do not infer or start future steps.
- Workers must not commit or update `plan.md`.
- Reviewers are read-only and must report findings and a verdict only.

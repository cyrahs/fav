# Project Status

Last updated: 2026-05-19
Maintainer: step-orchestrator coordinator

## Current Objective

- Correct the Azur Lane work so `fav` owns only resource crawling, local archiving, manifests, scheduler integration, and API backend support.
- Process the corrected `plan.md` steps `1-11` with worker/reviewer rounds.

## Active Plan

- Plan file: `plan.md`
- Requested steps: `1-11`
- Current step: `3. Add Azur Lane Config And Scheduler Wiring`
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
  - `PROJECT_STATUS.md`

## Constraints and Decisions

- Code, comments, docstrings, and documentation must be written in English.
- Use the repository `.venv` and run Python tooling through `uv`.
- Do not commit secrets, `config.toml`, or `plan.md`.
- `plan.md` is ignored through `.git/info/exclude`; it remains the local step tracker and must not be staged.
- Remove the mistaken Azur Lane client-side work from this repository.
- Implement Azur Lane in the same backend shape as NIKKE and BD2: crawler/storage under `src/web`, resource/source helpers under `src/tool`, manifest reader under `src/api`, scheduler wiring, and protected API endpoints.
- Static asset URLs in API responses should use `/static/azurlane/...`; FastAPI should not render Live2D or Spine.
- Do not add Azur Lane Live2D view override endpoints unless explicitly requested later.

## Relevant Project State

- Key existing backend references: `src/web/nikke.py`, `src/web/bd2.py`, `src/api/nikke.py`, `src/api/bd2.py`, `src/service/jobs.py`, `src/core/config.py`, `src/api/routes.py`, `src/api/schemas.py`, `src/api/service.py`.
- Existing Azur Lane helper: `src/tool/azurlane_l2d_sources.py`.
- Existing Azur Lane helper tests: `tests/test_azurlane_l2d_sources.py`.
- Recent approved commits before correction: steps 1-13 of the previous out-of-scope plan; those commits introduced both backend helper code and client-side artifacts.
- Recent approved correction steps: `step 1: Remove Frontend Viewer Artifacts` removed `viewer/azurlane/` and client-side Azur Lane claims from committed status docs; `step 2: Re-scope Source Helpers` removed renderer/viewer terminology from Azur Lane source helpers while preserving source/catalog/resource/drift behavior; `step 3: Add Azur Lane Config And Scheduler Wiring` added config defaults, scheduler job wiring, API job target support, and a safe no-op crawler placeholder.
- Pending work: corrected steps `4-11`.
- Known risks or blockers: repo-wide Ruff had pre-existing unrelated lint failures in earlier runs; use touched-file Ruff for step verification unless the full repo is intentionally cleaned.

## Verification

- Required commands by area:
  - Source helper: `uv run pytest tests/test_azurlane_l2d_sources.py`
  - Python lint for touched files: `uv run ruff check <paths>`
  - API/crawler focused tests as added by later steps
  - Whitespace check: `git diff --check`
- Last known results:
  - Step 1 approved: `rg -n "viewer/azurlane|Pixi|share link|visual regression|viewer shell" . --glob "!plan.md"` had no matches, `git status --short --ignored=matching plan.md` showed `!! plan.md`, and `git diff --check` passed.
  - Step 2 approved: `uv run pytest tests/test_azurlane_l2d_sources.py` passed with 22 tests, touched-file Ruff passed, viewer/renderer terminology grep had no matches in the helper/test files, and `git diff --check` passed.
  - Step 3 approved: focused Azur Lane/config/API/scheduler tests passed with 74 tests, touched-file Ruff passed, new `src/web/azurlane.py` and `tests/test_azurlane.py` were confirmed addable/not ignored, and `git diff --check` passed.

## Subagent Notes

- Read this file, `plan.md`, and applicable `AGENTS.md` files before editing or reviewing.
- Work only on the active step named in the subagent prompt.
- Do not infer or start future steps.
- Workers must not commit or update `plan.md`.
- Reviewers are read-only and must report findings and a verdict only.

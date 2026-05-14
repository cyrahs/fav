# Project Status

Last updated: 2026-05-14
Maintainer: step-orchestrator coordinator

## Current Objective

- Implement the Azur Lane Live2D/Spine viewer described in `plan.md`.
- Process steps `1-13` with worker/reviewer rounds, keeping `plan.md` local-only and uncommitted.

## Active Plan

- Plan file: `plan.md`
- Requested steps: `1-13`
- Current step: `1. Fetch Source Snapshots`
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
- Recent approved commits: `step 1: Fetch Source Snapshots` - source snapshot fetchers and tests.
- Pending work: steps `2-13` in `plan.md`.
- Known risks or blockers: frontend/runtime dependencies are not yet present in the repo.

## Verification

- Required commands:
  - `uv run ruff format .`
  - `uv run ruff check .`
  - `uv run pytest`
- Last known results:
  - Step 1 approved: `uv run pytest tests/test_azurlane_l2d_sources.py` passed, `uv run pytest` passed, touched-file Ruff passed, repo-wide Ruff blocked by pre-existing unrelated lint.

## Subagent Notes

- Read this file, `plan.md`, and applicable `AGENTS.md` files before editing or reviewing.
- Work only on the active step named in the subagent prompt.
- Do not infer or start future steps.
- Workers must not commit or update `plan.md`.
- Reviewers are read-only and must report findings and a verdict only.

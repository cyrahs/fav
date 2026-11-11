# Project Status

Last updated: 2026-11-11

## State

- Production (`media/fav` in charys117/nas-gitops) runs the Rust build: the `fav` binary from this
  workspace, image `ghcr.io/cyrahs/fav:latest`, which CI publishes from `main` and Flux deploys.
- The Python app was replaced on 2026-11-11 and removed from this branch. Its last commit is tag
  `python-final` (67609f6), and its last image is `ghcr.io/cyrahs/fav:python-final`; it runs on the same
  database schema, so it is the rollback.
- The rewrite's plan, parity record, final report and API checks are in `docs/rewrite/`. Intentional
  behaviour differences from Python are in `docs/DIVERGENCES.md`.

## Known open items

- RedNote: an intermittent 30 s stall loading the home document through the home proxy (seen on
  2026-11-11); not investigated.
- Telegram: grammers logs "cannot getChannelDifference as we're missing its hash" warnings; unchecked
  noise.

## Where to start

- `AGENTS.md`: layout, build/test/run, architecture, adding a source.
- `docs/CONVENTIONS.md`: code and test rules.
- `README.md`: sources, settings, API and MCP.

#!/usr/bin/env bash
# Repository rules for the `rust` branch that no compiler checks (docs/CONVENTIONS.md):
#   1. The Python tree is the reference: everything outside rust/ and the Rust CI
#      workflow must be byte-identical to the baseline commit in rust/BASELINE.
#   2. No ignored Rust tests: a test that cannot run is ported differently or waived.
# Needs the baseline commit in the local clone (CI checks out with full history).
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
baseline="$(tr -d '[:space:]' < rust/BASELINE)"
status=0

if ! git cat-file -e "${baseline}^{commit}" 2>/dev/null; then
  echo "error: baseline ${baseline} is not in this clone; fetch it first (git fetch origin ${baseline})"
  exit 1
fi

changed="$(git diff --name-only "${baseline}" -- . ':(exclude)rust' ':(exclude).github/workflows/rust.yml')"
if [[ -n "${changed}" ]]; then
  echo "error: files outside rust/ differ from the Python baseline ${baseline}:"
  sed 's/^/  /' <<< "${changed}"
  status=1
fi

ignored="$(grep -rnE '#\[ignore' --include='*.rs' rust/ --exclude-dir=target || true)"
if [[ -n "${ignored}" ]]; then
  echo "error: ignored tests are not allowed on the rust branch:"
  sed 's/^/  /' <<< "${ignored}"
  status=1
fi

if [[ ${status} -eq 0 ]]; then
  echo "tree check passed: Python tree matches ${baseline:0:7}, no ignored Rust tests"
fi
exit ${status}

#!/usr/bin/env bash
# Fails when production code gains a `let _ =` discard that is not classified
# in the allowlist, or when the allowlist still lists a site that is gone.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

if ! python3 tests/bootstrap/discarded-results-inventory.py \
    --check tests/bootstrap/discarded-results.allowlist; then
    printf '%s\n' \
        'Unclassified production `let _ =` must be added to tests/bootstrap/discarded-results.allowlist with a class, or the discard must be removed or replaced with best_effort / a surfaced error.' \
        'Stale allowlist rows must be deleted.' >&2
    exit 1
fi

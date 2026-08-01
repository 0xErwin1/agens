#!/usr/bin/env bash
set -euo pipefail

# Regenerates the committed baseline trace.
#
# Separate from perf-audit.sh on purpose: an ordinary audit is something you
# run constantly, and replacing the comparison point should be a deliberate
# act. A baseline captured from a dirty worktree records a commit that does
# not describe the code it measured, so this refuses to run on one.

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

if [[ -n "$(git status --porcelain)" ]]; then
    echo "refusing to regenerate the baseline from a dirty worktree" >&2
    echo "the trace would claim a commit that does not describe what ran" >&2
    exit 1
fi

baseline_dir="$repo_root/tests/perf/baseline"
run_id="baseline"

AGENS_PERF_COMMIT=$(git rev-parse HEAD)
AGENS_PERF_DIRTY=0
AGENS_PERF_HOST=$(uname -n)
export AGENS_PERF_COMMIT AGENS_PERF_DIRTY AGENS_PERF_HOST

mkdir -p "$baseline_dir"
cargo run -p agens-tui --features perf-audit --bin agens-perf-audit --locked -- \
    run "$baseline_dir" "$run_id"

echo "baseline regenerated at $baseline_dir/run.jsonl"
echo "review the diff before committing it: the shape is the contract, the timings are not"

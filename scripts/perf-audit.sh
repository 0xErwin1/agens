#!/usr/bin/env bash
set -euo pipefail

# Runs the performance audit and leaves a trace under target/perf/<run-id>/.
#
# The run's provenance is resolved here rather than inside the library: the
# library must never spawn a process, so commit, dirty flag and host arrive as
# environment variables it treats as opaque.
#
# The audit builds with a feature that `just verify` never enables, so this
# script lints and tests that code itself. Nothing else does.

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"

run_id=${1:-$(date -u +%Y%m%dT%H%M%SZ)}
target_dir=${CARGO_TARGET_DIR:-"$repo_root/target"}
trace_dir="$target_dir/perf/$run_id"

AGENS_PERF_COMMIT=$(git rev-parse HEAD 2>/dev/null || echo "")
if [[ -n "$(git status --porcelain 2>/dev/null)" ]]; then
    AGENS_PERF_DIRTY=1
else
    AGENS_PERF_DIRTY=0
fi
AGENS_PERF_HOST=$(uname -n)
export AGENS_PERF_COMMIT AGENS_PERF_DIRTY AGENS_PERF_HOST

cargo clippy -p agens-tui --features perf-audit --all-targets --locked -- -D warnings
cargo test -p agens-tui --features perf-audit --locked

mkdir -p "$trace_dir"
cargo run -p agens-tui --features perf-audit --bin agens-perf-audit --locked -- \
    run "$trace_dir" "$run_id"

printf '\nCompare against the committed baseline with:\n'
printf '  just perf-diff tests/perf/baseline/run.jsonl %s/run.jsonl\n' "$trace_dir"

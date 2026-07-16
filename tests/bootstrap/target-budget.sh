#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
check="$repo_root/scripts/check-target-budget.sh"
limit=21474836480
sandbox=$(mktemp -d)
trap 'rm -rf "$sandbox"' EXIT

caller_dir="$sandbox/caller"
mkdir -p "$caller_dir/target"
truncate -s 12345 "$caller_dir/target/caller-only"
repo_target="$repo_root/target"
if [[ -d "$repo_target" ]]; then
    repo_actual=$(du --apparent-size --bytes --summarize "$repo_target" | cut -f1)
else
    repo_actual=0
fi
caller_actual=$(du --apparent-size --bytes --summarize "$caller_dir/target" | cut -f1)
if [[ $caller_actual == "$repo_actual" ]]; then
    truncate -s 54321 "$caller_dir/target/caller-only"
    caller_actual=$(du --apparent-size --bytes --summarize "$caller_dir/target" | cut -f1)
fi

cd "$caller_dir"
output=$(env -u CARGO_TARGET_DIR "$check")
grep -F "actual size: $repo_actual bytes" <<<"$output" >/dev/null
if grep -F "actual size: $caller_actual bytes" <<<"$output" >/dev/null; then
    echo "checker measured the caller-relative target directory" >&2
    exit 1
fi

fixture="$sandbox/cargo-target"
mkdir "$fixture"
truncate -s "$limit" "$fixture/budget-fixture"
output=$(CARGO_TARGET_DIR="$fixture" "$check")
grep -F "actual size: $limit bytes" <<<"$output" >/dev/null
test -f "$fixture/budget-fixture"

truncate -s "$((limit + 1))" "$fixture/budget-fixture"
set +e
output=$(CARGO_TARGET_DIR="$fixture" "$check" 2>&1)
status=$?
set -e
if [[ $status -eq 0 ]]; then
    echo "one byte over budget unexpectedly passed" >&2
    exit 1
fi
grep -F "actual size: $((limit + 1)) bytes" <<<"$output" >/dev/null
grep -F "limit: $limit bytes" <<<"$output" >/dev/null
grep -F "just target-clean" <<<"$output" >/dev/null
test -f "$fixture/budget-fixture"

rm -rf "$sandbox"
test ! -e "$sandbox"
trap - EXIT

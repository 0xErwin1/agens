#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

recipes=$(just --dump --dump-format json)

dependencies() {
    jq -r --arg recipe "$1" '.recipes[$recipe].dependencies[].recipe' <<<"$recipes"
}

body() {
    jq -r --arg recipe "$1" '.recipes[$recipe].body[][]' <<<"$recipes"
}

assert_lines() {
    local expected=$1
    local actual=$2
    local label=$3
    if [[ "$actual" != "$expected" ]]; then
        printf '%s mismatch\nexpected:\n%s\nactual:\n%s\n' "$label" "$expected" "$actual" >&2
        exit 1
    fi
}

assert_lines '' "$(dependencies verify)" "verify dependency graph"
assert_lines $'just target-budget\njust contracts\njust fmt-check\njust lint\njust test\njust build\njust deny\njust target-budget' "$(body verify)" "verify execution order"

assert_lines 'cargo fmt --all -- --check' "$(body fmt-check)" "Rust format check"
assert_lines 'cargo clippy --workspace --all-targets --locked -- -D warnings' "$(body lint)" "Rust lint"
assert_lines 'cargo test --workspace --all-targets --locked' "$(body test)" "Rust tests"
assert_lines 'cargo build --workspace --locked' "$(body build)" "Rust build"
assert_lines 'cargo deny check' "$(body deny)" "supply-chain check"

assert_lines $'tests/bootstrap/assert-workspace.sh\ntests/bootstrap/docs-contract.sh\ntests/bootstrap/target-budget.sh\ntests/bootstrap/verify-contracts.sh\ntests/bootstrap/standards-contract.sh' "$(body contracts)" "bootstrap contracts"

for gate in verify build; do
    if just --dry-run "$gate" | grep -Eq '(^|[[:space:]])(cargo clean|just target-clean|rm -rf target)($|[[:space:]])'; then
        echo "$gate must not clean build output" >&2
        exit 1
    fi
done

if git ls-files -- 'cmd/**' 'internal/**' '*.go' go.mod go.sum .golangci.yml sqlc.yaml | grep -q .; then
    echo "tracked Go production, test, module, or tooling files remain" >&2
    exit 1
fi

if just --list --unsorted | grep -Eq '^(sqlc|verify-go|verify-rust|verify-dual|rust-fmt|rust-fmt-check|rust-lint|rust-test|rust-build)[[:space:]]'; then
    echo "Go or dual-runtime recipes remain" >&2
    exit 1
fi

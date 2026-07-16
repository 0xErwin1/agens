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

assert_lines $'sqlc\nfmt-check\nlint\ntest\nbuild' "$(dependencies verify-go)" "verify-go dependency order"
assert_lines 'verify-go' "$(dependencies verify)" "verify dependency graph"
assert_lines '' "$(dependencies verify-rust)" "verify-rust dependency graph"
assert_lines $'just target-budget\njust rust-fmt-check\njust rust-lint\njust rust-test\njust rust-build\njust target-budget' "$(body verify-rust)" "verify-rust execution order"
assert_lines $'verify-go\nverify-rust' "$(dependencies verify-dual)" "verify-dual dependency order"

assert_lines 'cargo fmt --all -- --check' "$(body rust-fmt-check)" "Rust format check"
assert_lines 'cargo clippy --workspace --all-targets --locked -- -D warnings' "$(body rust-lint)" "Rust lint"
assert_lines 'cargo test --workspace --all-targets --locked' "$(body rust-test)" "Rust tests"
assert_lines 'cargo build --workspace --locked' "$(body rust-build)" "Rust build"

for gate in verify verify-go verify-rust verify-dual build rust-build; do
    if just --dry-run "$gate" | grep -Eq '(^|[[:space:]])(cargo clean|just target-clean|rm -rf target)($|[[:space:]])'; then
        echo "$gate must not clean build output" >&2
        exit 1
    fi
done

#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

# devenv renders each script as a wrapper that execs a store path holding the
# body verbatim. Reading that body pins what the gate actually runs rather than
# how flake.nix happens to be formatted.
# The profile is consulted before PATH so that a task whose name collides with
# a shell builtin is still discoverable, which is what makes the shadow check
# below meaningful.
script_path() {
    local wrapper path

    wrapper=""
    if [[ -n "${DEVENV_PROFILE:-}" && -f "$DEVENV_PROFILE/bin/$1" ]]; then
        wrapper="$DEVENV_PROFILE/bin/$1"
    else
        wrapper=$(command -v "$1" 2>/dev/null) || return 1
        [[ -f "$wrapper" ]] || return 1
    fi

    path=$(grep -oE '/nix/store/[^ ]+-script' "$wrapper" 2>/dev/null | head -1)
    [[ -n "$path" ]] || return 1

    printf '%s\n' "$path"
}

is_task() {
    script_path "$1" >/dev/null 2>&1
}

# The prelude is boilerplate every multi-step script repeats; the steps are the
# contract.
steps() {
    local path
    if ! path=$(script_path "$1"); then
        printf 'task %s is not available; run the contracts inside the development shell\n' "$1" >&2
        exit 1
    fi
    grep -vxE 'set -euo pipefail|cd "\$DEVENV_ROOT"' "$path"
}

expand() {
    local line
    while IFS= read -r line; do
        if [[ "$line" =~ ^[a-z][a-z-]*$ ]] && is_task "$line"; then
            expand "$line"
        else
            printf '%s\n' "$line"
        fi
    done < <(steps "$1")
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

assert_lines $'target-budget\ncontracts\nfmt-check\nlint\ncheck\nbuild\ndeny\ntarget-budget' "$(steps verify)" "verify execution order"

assert_lines 'cargo fmt --all -- --check' "$(steps fmt-check)" "Rust format check"
assert_lines 'cargo clippy --workspace --all-targets --locked -- -D warnings' "$(steps lint)" "Rust lint"
assert_lines 'cargo test --workspace --all-targets --locked --no-fail-fast' "$(steps check)" "Rust tests"
assert_lines 'cargo build --workspace --locked' "$(steps build)" "Rust build"
assert_lines 'cargo deny check' "$(steps deny)" "supply-chain check"

assert_lines $'tests/bootstrap/assert-workspace.sh\ntests/bootstrap/assert-surface-boundary.sh\ntests/bootstrap/docs-contract.sh\ntests/bootstrap/target-budget.sh\ntests/bootstrap/verify-contracts.sh\ntests/bootstrap/standards-contract.sh\ntests/bootstrap/perf-offpath.sh' "$(steps contracts)" "bootstrap contracts"

# The expansion is captured before matching: piping it into `grep -q` lets grep
# exit on the first hit, killing the producer with SIGPIPE, and under pipefail
# that failing status masks the match the guard exists to catch.
for gate in verify build; do
    expanded=$(expand "$gate")

    if grep -Eq '(^|[[:space:]])(cargo clean|rm -rf target)($|[[:space:]])' <<<"$expanded"; then
        echo "$gate must not clean build output" >&2
        exit 1
    fi
done

# A script named `test` is shadowed by the bash builtin and could never run.
for shadowed in test '['; do
    if is_task "$shadowed"; then
        echo "task $shadowed is shadowed by a shell builtin and can never be invoked" >&2
        exit 1
    fi
done

if git ls-files -- 'cmd/**' 'internal/**' '*.go' go.mod go.sum .golangci.yml sqlc.yaml | grep -q .; then
    echo "tracked Go production, test, module, or tooling files remain" >&2
    exit 1
fi

for legacy in sqlc verify-go verify-rust verify-dual rust-fmt rust-fmt-check rust-lint rust-test rust-build; do
    if is_task "$legacy"; then
        echo "Go or dual-runtime task $legacy remains" >&2
        exit 1
    fi
done

if git ls-files -- justfile '*.just' scripts/dev | grep -q .; then
    echo "tracked files from a removed task runner remain" >&2
    exit 1
fi

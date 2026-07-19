default:
    @just --list

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets --locked -- -D warnings

test:
    cargo test --workspace --all-targets --locked

build:
    cargo build --workspace --locked

deny:
    cargo deny check

contracts:
    tests/bootstrap/assert-workspace.sh
    tests/bootstrap/docs-contract.sh
    tests/bootstrap/target-budget.sh
    tests/bootstrap/verify-contracts.sh
    tests/bootstrap/standards-contract.sh

target-size:
    #!/usr/bin/env bash
    set -euo pipefail
    target_dir="${CARGO_TARGET_DIR:-target}"
    if [[ -d "$target_dir" ]]; then
        du --apparent-size --bytes --summarize "$target_dir"
    else
        printf '0\t%s\n' "$target_dir"
    fi

target-budget:
    scripts/check-target-budget.sh

target-clean:
    cargo clean

verify:
    just target-budget
    just contracts
    just fmt-check
    just lint
    just test
    just build
    just deny
    just target-budget

clean:
    cargo clean

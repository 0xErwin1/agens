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
    just fmt-check
    just lint
    just test
    just build
    just target-budget

clean:
    cargo clean

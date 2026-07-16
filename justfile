default:
    @just --list

fmt:
    gofmt -w ./cmd ./internal
    goimports -w ./cmd ./internal

fmt-check:
    #!/usr/bin/env bash
    set -euo pipefail
    test -z "$(gofmt -l ./cmd ./internal)"
    test -z "$(goimports -l ./cmd ./internal)"

lint:
    golangci-lint run

sqlc:
    sqlc generate

test:
    go test ./...

build:
    go build -o agens ./cmd/agens

rust-fmt:
    cargo fmt --all

rust-fmt-check:
    cargo fmt --all -- --check

rust-lint:
    cargo clippy --workspace --all-targets --locked -- -D warnings

rust-test:
    cargo test --workspace --all-targets --locked

rust-build:
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

verify-go: sqlc fmt-check lint test build

verify: verify-go

verify-rust:
    just target-budget
    just rust-fmt-check
    just rust-lint
    just rust-test
    just rust-build
    just target-budget

verify-dual: verify-go verify-rust

clean:
    rm -f agens

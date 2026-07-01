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

test:
    go test ./...

build:
    go build -o agens ./cmd/agens

verify: fmt-check lint test build

clean:
    rm -f agens

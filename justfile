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

verify: sqlc fmt-check lint test build

clean:
    rm -f agens

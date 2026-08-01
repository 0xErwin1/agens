#!/usr/bin/env bash
# Pins the default build's promise that `agens-tui` carries no `tracing`
# dependency, and that `--features perf-audit` is the only thing that adds
# one. A regression here would mean the "zero cost when off" claim in
# AGENTS.md/CODE_STYLE.md is no longer true and nobody would notice until a
# shipped binary grew a tracing subscriber it never asked for.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

default_tree="$(cargo tree -p agens-tui -e normal)"
if grep -q '^tracing ' <<<"$default_tree" || grep -q ' tracing ' <<<"$default_tree"; then
    echo "agens-tui pulls in tracing under default features" >&2
    exit 1
fi

audit_tree="$(cargo tree -p agens-tui -e normal --features perf-audit)"
if ! grep -q 'tracing ' <<<"$audit_tree"; then
    echo "agens-tui with --features perf-audit does not pull in tracing" >&2
    exit 1
fi

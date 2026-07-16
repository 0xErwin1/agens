#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-"$repo_root/target"}
limit=21474836480

if [[ -d "$target_dir" ]]; then
    actual=$(du --apparent-size --bytes --summarize "$target_dir" | cut -f1)
else
    actual=0
fi

if ((actual > limit)); then
    printf 'Rust target budget exceeded\nactual size: %s bytes\nlimit: %s bytes\nCleanup is manual only; run just target-clean when appropriate.\n' "$actual" "$limit" >&2
    exit 1
fi

printf 'Rust target budget OK (actual size: %s bytes; limit: %s bytes)\n' "$actual" "$limit"

#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

grep -Fx '/target/' .gitignore >/dev/null

for doc in README.md AGENTS.md CONTRIBUTING.md CODE_STYLE.md; do
    grep -F 'nix develop --no-pure-eval' "$doc" >/dev/null
    if ! awk '
        function is_stale(command) {
            return command ~ /^nix[[:space:]]+develop([[:space:]]|$)/ &&
                command !~ /(^|[[:space:]])--no-pure-eval([[:space:]]|$)/
        }
        {
            command = $0
            sub(/^[[:space:]]*\$?[[:space:]]*/, "", command)
            if (is_stale(command)) {
                print FILENAME ":" FNR ": stale nix develop command: " command > "/dev/stderr"
                stale = 1
            }

            rest = $0
            while (match(rest, /`nix[[:space:]]+develop[^`]*`/)) {
                command = substr(rest, RSTART + 1, RLENGTH - 2)
                if (is_stale(command)) {
                    print FILENAME ":" FNR ": stale nix develop command: " command > "/dev/stderr"
                    stale = 1
                }
                rest = substr(rest, RSTART + RLENGTH)
            }
        }
        END { exit stale }
    ' "$doc"; then
        exit 1
    fi
    grep -F './agens' "$doc" >/dev/null
    grep -F 'target/{debug,release}/agens' "$doc" >/dev/null
    grep -F 'verify-go' "$doc" >/dev/null
    grep -F 'verify-rust' "$doc" >/dev/null
    grep -F 'verify-dual' "$doc" >/dev/null
    grep -F '20 GiB' "$doc" >/dev/null
    grep -Fi 'manual' "$doc" >/dev/null
    grep -F 'just target-clean' "$doc" >/dev/null
done

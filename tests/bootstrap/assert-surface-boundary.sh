#!/usr/bin/env bash
# Keeps logic from naming a user interface.
#
# `assert-workspace.sh` pins the crate graph, but it cannot see inside a crate,
# and today the engine still shares `agens-cli` with the surfaces. This check
# covers that gap for the modules already separated: each listed file must stay
# free of terminal rendering, prompting and TUI types. When a module moves to its
# own crate the crate graph takes over and its entry here can go.
#
# Adding a file here is how a decoupled module stays decoupled. Removing one is a
# decision to re-couple, and should be argued for rather than done quietly.
set -euo pipefail

declare -a LOGIC_MODULES=(
  "crates/agens-cli/src/permissions.rs"
  "crates/agens-cli/src/session/provider.rs"
  "crates/agens-cli/src/session/context.rs"
  "crates/agens-cli/src/turns.rs"
)

# Names that only exist because a human is watching.
SURFACE_PATTERN='agens_tui|[^A-Za-z_]Tui[A-Z]|IsTerminal|stdin\(\)|stderr\(\)|eprint!|println!'

status=0

for module in "${LOGIC_MODULES[@]}"; do
  if [[ ! -f "$module" ]]; then
    echo "surface boundary: $module is listed but missing; update the list" >&2
    status=1
    continue
  fi

  if matches="$(grep -nE "$SURFACE_PATTERN" "$module")"; then
    echo "surface boundary: $module names a user interface" >&2
    echo "$matches" >&2
    status=1
  fi
done

if [[ $status -ne 0 ]]; then
  echo >&2
  echo "Logic must reach a surface through a port it owns, never by naming one." >&2
  echo "See ARCHITECTURE.md, 'Surfaces and logic'." >&2
fi

exit "$status"

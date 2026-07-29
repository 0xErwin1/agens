#!/usr/bin/env bash
# Pins which workspace crates each crate may reach.
#
# The list is deduplicated because a crate legitimately appears in both
# `[dependencies]` and `[dev-dependencies]` when the dev entry exists only to
# enable a test-seam feature. What this contract pins is which crates are
# reachable, not how many manifest entries name them.
set -euo pipefail

metadata="$(cargo metadata --format-version 1 --no-deps)"

jq -e '
  {
    "agens-core": [],
    "agens-bus": [],
    "agens-error": ["agens-core"],
    "agens-bootstrap": ["agens-config", "agens-core", "agens-error", "agens-tools"],
    "agens-callcount": [],
    "agens-models": ["agens-core"],
    "agens-agents": [
      "agens-bootstrap",
      "agens-core",
      "agens-diagnostics",
      "agens-error",
      "agens-models",
      "agens-permissions",
      "agens-providers",
      "agens-session",
      "agens-store",
      "agens-tools"
    ],
    "agens-fixtures": ["agens-bootstrap", "agens-error"],
    "agens-diagnostics": [
      "agens-bootstrap",
      "agens-core",
      "agens-error",
      "agens-providers"
    ],
    "agens-dispatch": [
      "agens-core",
      "agens-permissions",
      "agens-tools"
    ],
    "agens-permissions": [
      "agens-config",
      "agens-core",
      "agens-error",
      "agens-store",
      "agens-tools"
    ],
    "agens-session": [
      "agens-bootstrap",
      "agens-core",
      "agens-error",
      "agens-models",
      "agens-providers",
      "agens-store",
      "agens-tools"
    ],
    "agens-config": [],
    "agens-providers": ["agens-config", "agens-core"],
    "agens-tools": ["agens-config", "agens-core"],
    "agens-store": ["agens-core"],
    "agens-tool-runtime": [
      "agens-agents",
      "agens-bootstrap",
      "agens-bus",
      "agens-callcount",
      "agens-config",
      "agens-core",
      "agens-diagnostics",
      "agens-dispatch",
      "agens-error",
      "agens-fixtures",
      "agens-models",
      "agens-permissions",
      "agens-providers",
      "agens-session",
      "agens-store",
      "agens-tools"
    ],
    "agens-tui": ["agens-bus", "agens-core"],
    "agens-server": ["agens-core"],
    "agens": [
      "agens-agents",
      "agens-bootstrap",
      "agens-bus",
      "agens-callcount",
      "agens-config",
      "agens-core",
      "agens-diagnostics",
      "agens-dispatch",
      "agens-error",
      "agens-fixtures",
      "agens-models",
      "agens-permissions",
      "agens-providers",
      "agens-server",
      "agens-session",
      "agens-store",
      "agens-tool-runtime",
      "agens-tools",
      "agens-tui"
    ]
  } as $expected
  | (.packages | map(.name)) as $workspace_names
  | (.packages
      | map({
          key: .name,
          value: (
            .dependencies
            | map(.name as $name | select($workspace_names | index($name)) | $name)
            | sort
            | unique
          )
        })
      | from_entries) as $actual
  | ($actual == $expected)
' <<<"$metadata" >/dev/null

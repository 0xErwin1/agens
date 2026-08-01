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
    "agens": [
      "agens-agents",
      "agens-auth",
      "agens-bootstrap",
      "agens-bus",
      "agens-callcount",
      "agens-config",
      "agens-core",
      "agens-diagnostics",
      "agens-dispatch",
      "agens-error",
      "agens-fixtures",
      "agens-headless",
      "agens-models",
      "agens-permissions",
      "agens-providers",
      "agens-server",
      "agens-session",
      "agens-store",
      "agens-tool-runtime",
      "agens-tools",
      "agens-tui",
      "agens-tui-app"
    ],
    "agens-agents": [
      "agens-bootstrap",
      "agens-config",
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
    "agens-auth": [
      "agens-error",
      "agens-providers"
    ],
    "agens-bootstrap": [
      "agens-config",
      "agens-core",
      "agens-error",
      "agens-models",
      "agens-tools"
    ],
    "agens-bus": [],
    "agens-callcount": [],
    "agens-config": [],
    "agens-core": [],
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
    "agens-error": [
      "agens-core"
    ],
    "agens-fixtures": [
      "agens-bootstrap",
      "agens-error",
      "agens-models",
      "agens-tools"
    ],
    "agens-headless": [
      "agens-agents",
      "agens-bootstrap",
      "agens-callcount",
      "agens-core",
      "agens-diagnostics",
      "agens-dispatch",
      "agens-error",
      "agens-models",
      "agens-permissions",
      "agens-providers",
      "agens-session",
      "agens-store",
      "agens-tool-runtime",
      "agens-tools"
    ],
    "agens-models": [
      "agens-core"
    ],
    "agens-perf": [],
    "agens-permissions": [
      "agens-config",
      "agens-core",
      "agens-error",
      "agens-store",
      "agens-tools"
    ],
    "agens-providers": [
      "agens-config",
      "agens-core"
    ],
    "agens-server": [
      "agens-core"
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
    "agens-store": [
      "agens-core"
    ],
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
    "agens-tools": [
      "agens-config",
      "agens-core"
    ],
    "agens-tui": [
      "agens-bus",
      "agens-core"
    ],
    "agens-tui-app": [
      "agens-agents",
      "agens-auth",
      "agens-bootstrap",
      "agens-bus",
      "agens-callcount",
      "agens-config",
      "agens-core",
      "agens-diagnostics",
      "agens-dispatch",
      "agens-error",
      "agens-fixtures",
      "agens-headless",
      "agens-models",
      "agens-permissions",
      "agens-providers",
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

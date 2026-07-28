#!/usr/bin/env bash
set -euo pipefail

metadata="$(cargo metadata --format-version 1 --no-deps)"

jq -e '
  {
    "agens-core": [],
    "agens-bus": [],
    "agens-error": ["agens-core"],
    "agens-bootstrap": ["agens-config", "agens-core", "agens-error", "agens-tools"],
    "agens-models": ["agens-core"],
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
    "agens-tui": ["agens-bus", "agens-core"],
    "agens-server": ["agens-core"],
    "agens": [
      "agens-bootstrap",
      "agens-bus",
      "agens-config",
      "agens-core",
      "agens-error",
      "agens-models",
      "agens-permissions",
      "agens-providers",
      "agens-server",
      "agens-session",
      "agens-store",
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
          )
        })
      | from_entries) as $actual
  | ($actual == $expected)
' <<<"$metadata" >/dev/null

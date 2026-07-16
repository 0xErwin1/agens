#!/usr/bin/env bash
set -euo pipefail

metadata="$(cargo metadata --format-version 1 --no-deps)"

jq -e '
  {
    "agens-core": [],
    "agens-config": [],
    "agens-providers": ["agens-config", "agens-core"],
    "agens-tools": ["agens-config", "agens-core"],
    "agens-store": ["agens-core"],
    "agens-tui": ["agens-core"],
    "agens": [
      "agens-config",
      "agens-core",
      "agens-providers",
      "agens-store",
      "agens-tools",
      "agens-tui"
    ]
  } as $expected
  | (.packages
      | map({key: .name, value: (.dependencies | map(.name) | sort)})
      | from_entries) as $actual
  | ($actual == $expected)
' <<<"$metadata" >/dev/null

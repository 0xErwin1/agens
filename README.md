# Agens

Agens is a Go CLI coding agent. The first milestone is a small, testable headless core that can grow into a polished TUI, provider integrations, tools, MCP, skills, and sub-agents.

## Status

This repository is at `AGN-1`: foundation scaffold only.

In scope now:

- Go module and single `agens` binary.
- Cobra-based CLI shell.
- Nix development shell.
- Canonical `just` workflows.
- Mandatory formatting, linting, tests, and local build.

Out of scope for `AGN-1`:

- Providers and ChatGPT/Codex subscription integration.
- TUI.
- MCP/tools implementation.
- Cross-compilation, packaging, and release automation.

## Development

This project is Nix-first:

```sh
nix develop
```

Canonical commands:

```sh
just fmt      # format Go code
just lint     # golangci-lint
just test     # go test ./...
just build    # build local ./agens binary
just verify   # fmt-check + lint + test + build
just clean    # remove local build output
```

Run the scaffolded CLI:

```sh
just build
./agens --help
./agens config doctor
```

## Configuration

Agens uses TOML files for hand-authored bootstrap configuration:

- Global: `~/.config/agens/config.toml` or `$AGENS_CONFIG_HOME/config.toml`.
- Project: `<project-root>/.agens/config.toml`, where project root is the nearest `.git` ancestor or the current directory outside git.

Project config overrides global config. Missing files are valid and load defaults.

Mutable runtime state does not belong in TOML config. Sessions, remembered permissions, model caches, discovered MCP state, and last-used values should be stored in SQLite by later tasks.

## TDD

Use strict TDD for non-trivial changes:

1. Write or update the failing test first.
2. Record the red output in the apply artifact.
3. Implement the smallest change.
4. Record green test output.
5. Finish with `nix develop -c just verify`.

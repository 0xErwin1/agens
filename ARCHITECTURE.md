# Architecture — Agens

Agens is a Go CLI coding agent. The architecture starts intentionally small: one binary, shallow internal packages, and clear seams for future providers, tools, persistence, and TUI work.

## Current package map

```text
cmd/agens      process entrypoint only
internal/app   application bootstrap and execution seam
internal/cli   Cobra command tree and CLI adapter behavior
internal/config minimal configuration path contract
internal/message typed, provider-neutral conversation history model (leaf, no internal deps)
internal/provider provider-neutral contracts for auth, chat streaming, and factory wiring (leaf, Cobra-free, depends only on internal/message)
internal/version build/version metadata
```

## Dependency direction

```text
cmd/agens -> internal/app -> internal/cli
                         -> internal/config
                         -> internal/version

internal/provider -> internal/message

(future) internal/agentloop, internal/tui, internal/persistence -> internal/message
```

Rules:

- `cmd/agens` contains no business logic.
- `internal/cli` owns Cobra commands and help behavior.
- Future domain packages must not import Cobra.
- `internal/config` owns TOML bootstrap config loading, project/global merge, environment expansion, and read-only diagnostics.
- `internal/message` is a leaf package: it depends on nothing internal (stdlib + `google/uuid` only) and defines `Message`, `Role`, and the closed `Part` union (`TextPart`, `ToolUsePart`, `ToolResultPart`) with their JSON codec. Future providers (mapping wire formats to/from this model), the agent loop, the TUI, and persistence will all depend on `internal/message`; it must never depend on any of them.
- `internal/version` exposes build metadata and can later receive linker-provided values.

## Boundaries for future work

AGN-1 creates the foundation only. Later tasks may add:

- provider interfaces and ChatGPT/Codex auth;
- agent loop and message model;
- tools and permission engine;
- persistence;
- TUI.

Those features must enter through new SDD artifacts and should preserve the dependency direction above. Avoid adding broad abstractions before a concrete task needs them.

## Config and state boundary

TOML config files are for hand-authored bootstrap inputs only. Runtime state that Agens mutates — sessions, remembered permissions, model caches, discovered MCP state, and last-used values — belongs in a future SQLite-backed store, not in config files.

## Repository contracts

- `justfile` is the canonical developer command surface.
- `flake.nix` owns the reproducible development environment.
- `CODE_STYLE.md` owns formatting, linting, and testing expectations.
- `AGENTS.md` owns agent-specific workflow rules.

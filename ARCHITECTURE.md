# Architecture — Agens

Agens is a Go CLI coding agent. The architecture starts intentionally small: one binary, shallow internal packages, and clear seams for future providers, tools, persistence, and TUI work.

## Current package map

```text
cmd/agens      process entrypoint only
internal/app   application bootstrap and execution seam
internal/cli   Cobra command tree and CLI adapter behavior
internal/config minimal configuration path contract
internal/version build/version metadata
```

## Dependency direction

```text
cmd/agens -> internal/app -> internal/cli
                         -> internal/config
                         -> internal/version
```

Rules:

- `cmd/agens` contains no business logic.
- `internal/cli` owns Cobra commands and help behavior.
- Future domain packages must not import Cobra.
- `internal/config` currently defines only the config-home contract. Deep config loading belongs to `AGN-2`.
- `internal/version` exposes build metadata and can later receive linker-provided values.

## Boundaries for future work

AGN-1 creates the foundation only. Later tasks may add:

- provider interfaces and ChatGPT/Codex auth;
- agent loop and message model;
- tools and permission engine;
- persistence;
- TUI.

Those features must enter through new SDD artifacts and should preserve the dependency direction above. Avoid adding broad abstractions before a concrete task needs them.

## Repository contracts

- `justfile` is the canonical developer command surface.
- `flake.nix` owns the reproducible development environment.
- `CODE_STYLE.md` owns formatting, linting, and testing expectations.
- `AGENTS.md` owns agent-specific workflow rules.

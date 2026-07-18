# Architecture — Agens

Agens is a Rust coding-agent CLI. The Rust workspace is the only buildable, testable, and executable implementation.

## Workspace map

```text
crates/agens-cli        command parsing and composition root
  -> agens-core         messages, turns, cancellation, and domain errors
  -> agens-config       TOML configuration and credential-path compatibility
  -> agens-providers    OpenAI and ChatGPT authentication and streaming adapters
  -> agens-tools        native tools, permissions, MCP, skills, and subagents
  -> agens-store        SQLite sessions and persisted grants
  -> agens-tui          terminal surface over the shared runtime
```

## Dependency direction

- `agens-core` owns domain contracts and does not depend on adapter crates.
- `agens-config` is a leaf crate for configuration and credential compatibility.
- Provider, tool, and store crates may depend on `agens-core` and `agens-config` where required.
- `agens-tui` depends on `agens-core` only and remains a surface adapter.
- `agens-cli` is the composition root and the sole binary crate.

## Runtime boundary

The CLI and TUI submit work through one cancellation-aware engine. Providers emit ordered turn events; tool dispatch evaluates permissions before execution; completed turns and grants are persisted by `agens-store` in clean Rust SQLite databases. Adapters add actionable context while typed errors remain distinct from cancellation.

## Repository contracts

- `justfile` is the canonical Rust developer command surface.
- `flake.nix` owns the reproducible Rust development environment.
- `CODE_STYLE.md` owns formatting, linting, and testing expectations.
- `AGENTS.md` owns agent-specific workflow rules.
- `target/{debug,release}/agens` contains build outputs; `target/` is never cleaned by verification.

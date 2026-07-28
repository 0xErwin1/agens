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
  -> agens-server       the machine's daemon: single-instance runtime and, later, the coordinator
```

## Dependency direction

- `agens-core` owns domain contracts and does not depend on adapter crates.
- `agens-config` is a leaf crate for configuration and credential compatibility.
- Provider, tool, and store crates may depend on `agens-core` and `agens-config` where required.
- `agens-tui` depends on `agens-core` only and remains a surface adapter.
- `agens-server` depends on `agens-core` only. It owns the daemon: the single-instance runtime today, and the coordinator, its state machines, the scheduler and the timers as they land. None of that belongs to a command surface, so `serve` stays a thin adapter over this crate.
- `agens-cli` is the composition root and the sole binary crate.

## Surfaces and logic

A **surface** is anything a user interacts with: argument parsing, terminal rendering, prompting, formatting for a human. Everything else is **logic**.

- **Logic must never depend on a surface.** No logic crate may depend on `agens-tui`, on the CLI crate, or on any surface crate added later. The dependency goes one way: a surface depends on logic and adapts it.
- **Logic must not be written against surface types.** A function that takes a rendering type, a prompt reply, or a "which surface submitted this" enum is coupled even when the crate graph looks clean. When logic needs something from a surface, it declares a trait it owns and the surface implements it.
- **A surface holds only what disappears with it.** If deleting the TUI would delete the ability to run a turn, evaluate a permission, or dispatch a tool, that logic is in the wrong place.

`tests/bootstrap/assert-workspace.sh` pins every crate's exact dependency list, so a logic crate that grows a surface dependency fails `just contracts`. **That check cannot see inside a crate.** Logic that lives in the binary crate alongside the surfaces is therefore unenforced by construction, which is the reason engine code moves out of `agens-cli` rather than staying there behind naming discipline.

## Runtime boundary

The CLI and TUI submit work through one cancellation-aware engine. Providers emit ordered turn events; tool dispatch evaluates permissions before execution; completed turns and grants are persisted by `agens-store` in clean Rust SQLite databases. Adapters add actionable context while typed errors remain distinct from cancellation.

## Repository contracts

- `justfile` is the canonical Rust developer command surface.
- `flake.nix` owns the reproducible Rust development environment.
- `CODE_STYLE.md` owns formatting, linting, and testing expectations.
- `AGENTS.md` owns agent-specific workflow rules.
- `target/{debug,release}/agens` contains build outputs; `target/` is never cleaned by verification.

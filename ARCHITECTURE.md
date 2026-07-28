# Architecture — Agens

Agens is a Rust coding-agent CLI. The Rust workspace is the only buildable, testable, and executable implementation.

## Workspace map

```text
crates/agens-cli        argument parsing and composition; calls into the crates below
  -> agens-core         messages, turns, cancellation, and domain contracts
  -> agens-error        the shared error and exit-status contract
  -> agens-config       TOML configuration and credential-path compatibility
  -> agens-models       the bundled model catalog and selection
  -> agens-bootstrap    resolving a run's configuration from a host environment
  -> agens-providers    OpenAI and ChatGPT authentication and streaming adapters
  -> agens-store        SQLite sessions, grants, and the evidence ledger
  -> agens-tools        native tools, permissions, MCP, skills, and subagents
  -> agens-session      what a session is: identity, context, provider, attempts
  -> agens-server       the machine's daemon and its sync/async boundary
  -> agens-tui          terminal rendering surface
```

## What each crate is

Read this before adding a module: the question "where does this go" is answered
by which of these sentences it fits, not by which directory is convenient.

| Crate | Owns | Does not own |
|---|---|---|
| `agens-core` | The domain vocabulary every other crate speaks: messages, turns and their state machine, cancellation, session metadata, tool-result facts, the subagent outcome taxonomy. | Anything that performs I/O, and any dependency on another workspace crate. |
| `agens-error` | The error and exit-status contract shared by every layer that can fail. | Deciding how an error is displayed. |
| `agens-config` | Reading hand-authored TOML and resolving credential paths. | Deciding what the resolved values mean for a run. |
| `agens-models` | The bundled model catalog, its checksum, and a validated model selection. | Talking to a provider. |
| `agens-bootstrap` | Turning a host environment into a resolved `Bootstrap`: paths, settings, credentials, MCP servers, and the session-scoped re-resolution that keeps a resumed session on its own root. | Knowing which command is running or how it reports. |
| `agens-providers` | Provider adapters: authentication, streaming, and their error taxonomy. | Choosing a provider or a model for a session. |
| `agens-store` | SQLite: the unified database, migrations, sessions, attempts, grants, the evidence ledger, and paginated transcript reads. | Interpreting what it stores. |
| `agens-tools` | Native tools and their confinement, permission capabilities, MCP transports, skills, and subagent plumbing. | Deciding whether a call is allowed for a given session. |
| `agens-session` | What a session is: its context, the provider and credentials it speaks through, and its attempt lifecycle. | Rendering any of it, or composing text for a person. |
| `agens-server` | The machine's daemon: its single-instance guard, its runtime, and the one named crossing into synchronous code. Grows to hold the coordinator. | A project. One daemon serves many. |
| `agens-tui` | Terminal rendering: widgets, layout, the conversation projection, and the bridges a surface needs. | Any decision the runtime would still have to make with no terminal attached. |
| `agens-cli` | Argument parsing, the command table, and wiring production implementations together. | Logic. If deleting the CLI would delete a capability, that capability is in the wrong crate. |

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

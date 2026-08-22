# Architecture — Agens

Agens is a Rust coding-agent CLI. The Rust workspace is the only buildable, testable, and executable implementation.

## Workspace map

```text
crates/agens-cli        argument parsing and composition; calls into the crates below
  -> agens-core         messages, turns, cancellation, and domain contracts
  -> agens-bus          a bounded, ordered, cancellable publish channel
  -> agens-error        the shared error and exit-status contract
  -> agens-callcount    how often a cold path ran, for tests to assert on
  -> agens-perf         hierarchical timing traces, and the diff between two runs
  -> agens-config       TOML configuration and credential-path compatibility
  -> agens-models       the bundled model catalog and selection
  -> agens-bootstrap    resolving a run's configuration from a host environment
  -> agens-providers    OpenAI, ChatGPT, and Moonshot authentication and streaming adapters
  -> agens-auth         signing in to a ChatGPT subscription
  -> agens-store        SQLite sessions, grants, and the evidence ledger
  -> agens-tools        native tools, MCP, skills, and subagents
  -> agens-tool-runtime assembling and running the tools a turn can call
  -> agens-headless     running one turn with no interface attached
  -> agens-permissions  whether a tool call is allowed
  -> agens-dispatch     binding a tool call to the thing that runs it
  -> agens-diagnostics  the sanitized, capacity-bounded record of what went wrong
  -> agens-agents       which agents exist and which models each may run
  -> agens-session      what a session is: identity, context, provider, attempts
  -> agens-snapshot     point-in-time snapshots of the working tree, stored outside the project
  -> agens-server       the machine's daemon and its sync/async boundary
  -> agens-tui          terminal rendering surface
  -> agens-tui-app      the terminal application that drives it
```

## What each crate is

Read this before adding a module: the question "where does this go" is answered
by which of these sentences it fits, not by which directory is convenient.

| Crate | Owns | Does not own |
|---|---|---|
| `agens-core` | The domain vocabulary every other crate speaks: messages, turns and their state machine, cancellation, session metadata, tool-result facts, the subagent outcome taxonomy. | Anything that performs I/O, and any dependency on another workspace crate. |
| `agens-bus` | A bounded, ordered, cancellable publish channel, generic over what travels on it. Communication belongs to no layer: the runtime publishes, and a terminal, a daemon or a test consumes. | Knowing what an event means, or who is at either end. |
| `agens-callcount` | Thread-local counters for cold paths a test needs to count: runtime construction, session resume. Not behind `cfg(test)`, because that flag never reaches a dependency and the instrumentation would silently become a no-op across a crate boundary. | Anything on a hot path. |
| `agens-perf` | The hot-path counterpart: hierarchical spans with timings, written to a trace, and the diff that says what changed between two of them. Compiles to nothing unless its feature is on, for the same reason `agens-callcount` is not behind `cfg(test)`. Knows nothing about any crate that instruments it. | Deciding what is too slow. It reports; a person judges. |
| `agens-error` | The error and exit-status contract shared by every layer that can fail. | Deciding how an error is displayed. |
| `agens-config` | Reading hand-authored TOML and resolving credential paths. | Deciding what the resolved values mean for a run. |
| `agens-models` | The bundled model catalog, its checksum, and a validated model selection. | Talking to a provider. |
| `agens-bootstrap` | Turning a host environment into a resolved `Bootstrap`: paths, settings, per-provider credentials, MCP servers, and the session-scoped re-resolution that keeps a resumed session on its own root. | Knowing which command is running or how it reports. |
| `agens-providers` | Provider adapters for both wire dialects — the OpenAI responses API and chat completions — with authentication, streaming, and their shared error taxonomy. | Choosing a provider or a model for a session. |
| `agens-auth` | The two ChatGPT sign-in flows, browser and device code, and the credentials they produce. Both report progress through a callback. | Showing progress. It never learns which surface is watching. |
| `agens-store` | SQLite: the unified database, migrations, sessions, attempts, grants, the evidence ledger, and paginated transcript reads. | Interpreting what it stores. |
| `agens-tools` | Native tools and their confinement, permission capabilities, MCP transports, skills, and subagent plumbing. | Deciding whether a call is allowed for a given session. |
| `agens-tool-runtime` | Assembly and execution: building the native catalog and the MCP registry for a project root, registering the `task` tool, running a delegated task in a confined child process, and launching a subagent a caller armed for the next prompt. | Who asked. A headless run, a terminal session and a daemon worker assemble the same runtime. |
| `agens-headless` | One turn driven to completion under the session attempt lifecycle: building the provider for the configured backend, resolving the policy and prompt it runs under, and recording the outcome. | Asking a person. Permission questions leave through a `PermissionPrompter` the caller supplies, so the same turn runs behind a terminal, a `--print` invocation or a daemon worker. |
| `agens-permissions` | Deciding whether a tool call is allowed: rules, grants, session authorization, and which tools a delegated subagent may reach. | Asking a person. That is a surface, and it reaches policy through the `PermissionPrompter` port. |
| `agens-agents` | Agents as configuration resolved at run time: catalog discovery against the session's own root, the two model validators, and which agent a session is actually running once a persisted choice may have gone away. | Switching agent. Rotation needs a tool runtime, so it sits above this crate. |
| `agens-diagnostics` | The local failure record: a rotating, size-capped JSONL log written with a closed set of fields, and the reference-scoped handles a provider writes through. | Showing any of it. A surface reads the files and decides what a person sees. |
| `agens-dispatch` | The dispatch table: the adapters that bind a native or MCP tool to its executor, the dispatcher that runs only what permissions already authorized, the redaction a failure passes through before a model sees it, and the authorized subagent-task launch. | Who asked. The same table serves a headless run, a terminal session and a daemon worker. |
| `agens-session` | What a session is: its context, the provider and credentials it speaks through, its attempt lifecycle, how a completed turn is recorded, and how one is taken back — which snapshot a direction restores, which paths it may write, and which belong to somebody else. | Rendering any of it, or composing text for a person. |
| `agens-snapshot` | Point-in-time snapshots of a project's working tree, and putting one back a path at a time. The snapshots live in a git repository kept beside the session data, so the project's own history, index, branches and stash are never touched. | Deciding when a snapshot is worth taking, or which paths a caller wants back. Both are the caller's judgment, and a restore is never wholesale. |
| `agens-server` | The machine's daemon: its single-instance guard, its runtime, and the one named crossing into synchronous code. Grows to hold the coordinator. | A project. One daemon serves many. |
| `agens-tui` | Terminal rendering: widgets, layout, the conversation projection, and the bridges a surface needs. | Any decision the runtime would still have to make with no terminal attached. |
| `agens-fixtures` | Test fixtures more than one crate needs: an isolated project directory, a `Bootstrap` resolved from a fixed host, a deadline-based wait. A dev-dependency. | Anything a surface would render. Keeping it surface-free is what lets a logic crate use it without pulling a terminal into its test build. |
| `agens-tui-app` | The terminal application: routing a submission or a slash command, resuming a session, choosing a model, answering a permission question, reporting what the runtime is doing. A surface, so it may depend on logic and logic may never depend on it. | Any decision the runtime would still have to make with no terminal attached. |
| `agens-cli` | Argument parsing, the command table, and wiring production implementations together. | Logic. If deleting the CLI would delete a capability, that capability is in the wrong crate. |

## Dependency direction

- `agens-core` owns domain contracts and does not depend on adapter crates.
- `agens-config` is a leaf crate for configuration and credential compatibility.
- Provider, tool, and store crates may depend on `agens-core` and `agens-config` where required.
- `agens-tui` depends on `agens-bus`, `agens-core` and `agens-perf`, and remains a surface adapter.
- `agens-server` depends on `agens-core` and `agens-store`. It owns the daemon: the single-instance runtime, the coordinator's state machines, and the scheduler and timers as they land. None of that belongs to a command surface, so `serve` stays a thin adapter over this crate. The store dependency is the coordinator's: its state machines are the sole writer of the control-plane tables, and every transition they apply is a conditional state change plus its journal entries in one transaction. Reaching that through a port would put the transaction boundary on the wrong side of the seam, and copying the SQL into the daemon would give the control plane a second writer.
- `agens-cli` is the composition root and the sole shipped binary crate. `agens-tui` also carries `agens-perf-audit`, a development binary that only exists behind a non-default feature.

## Surfaces and logic

A **surface** is anything a user interacts with: argument parsing, terminal rendering, prompting, formatting for a human. Everything else is **logic**.

- **Logic must never depend on a surface.** No logic crate may depend on `agens-tui`, on the CLI crate, or on any surface crate added later. The dependency goes one way: a surface depends on logic and adapts it.
- **Logic must not be written against surface types.** A function that takes a rendering type, a prompt reply, or a "which surface submitted this" enum is coupled even when the crate graph looks clean. When logic needs something from a surface, it declares a trait it owns and the surface implements it.
- **A surface holds only what disappears with it.** If deleting the TUI would delete the ability to run a turn, evaluate a permission, or dispatch a tool, that logic is in the wrong place.

`tests/bootstrap/assert-workspace.sh` pins every crate's exact dependency list, so a logic crate that grows a surface dependency fails `contracts`. **That check cannot see inside a crate.** Logic that lives in the binary crate alongside the surfaces is therefore unenforced by construction, which is the reason engine code moves out of `agens-cli` rather than staying there behind naming discipline.

## Runtime boundary

The CLI and TUI submit work through one cancellation-aware engine. Providers emit ordered turn events; tool dispatch evaluates permissions before execution; completed turns and grants are persisted by `agens-store` in clean Rust SQLite databases. Adapters add actionable context while typed errors remain distinct from cancellation.

## Agent instructions (AGENTS.md)

Every agent's system prompt is assembled in a fixed layer order, each layer separated by a blank line:

1. **Built-in base** — `agens_core::prompt::BASE_SYSTEM_PROMPT`, the one place the base identity text lives (`agens-core/src/prompt.rs`).
2. **Agent prompt** — the agent's own prompt, TOML-configured (`[agent].system_prompt`) or markdown-defined.
3. **AGENTS.md instructions** — two optional instruction files, appended global-then-project (see table below).
4. **Delegation block** — the subagent routing-discipline text, appended only when the catalog reports a subagent.

**Breaking change**: a configured `[agent].system_prompt` now composes after the built-in base (`agens_core::prompt::base_system_prompt`) instead of replacing it. An unconfigured agent is unaffected — it still resolves to the base alone. A configured agent's effective prompt gained a new leading paragraph it did not have before.

The headless `--system` flag is the one exception: it fully **replaces** the base for that invocation — the resolver in `agens_core::prompt` is not consulted at all when `--system` is supplied. Layers 3 and 4 still apply on top of the replaced text. This means "the user supplied a prompt" carries two different semantics depending on the source — `[agent].system_prompt` composes, `--system` replaces — which is a discoverability wart worth knowing before reaching for either.

The built-in `explore` and `general` subagents never receive the base layer: their one-line role prompts already state an identity, and they are unreachable by `[agent].system_prompt`, so composing there would widen the breaking surface with no benefit.

| Source | Location |
|---|---|
| Global | `AGENTS.md` beside the resolved global config (honors `$AGENS_CONFIG_HOME` / `$XDG_CONFIG_HOME`) |
| Project | `<session-root>/AGENTS.md` |

There is no ancestor-directory search; only these two exact paths are considered.

`SessionInstructions` (`agens-bootstrap`, `session_config.rs`) owns discovery and composition for every caller. It reads each candidate through `agens-tools`' `markdown::load_instruction_file`, which rejects symlinks, non-regular files, oversized files, and non-UTF-8 content. A missing, empty, oversized (over 256 KiB), symlinked, unreadable, or non-UTF-8 file is skipped silently, and skipping one file never affects the other. Each accepted file is labelled `## Instructions from <path>` and appended global-then-project; identical canonical paths are deduplicated; the combined appended text is capped at 256 KiB, dropping the whole offending file rather than truncating it mid-content.

`SessionInstructions` is read at two points, each appending into a different prompt surface:

- `discover_agent_catalog` (`agens-agents`, `catalog.rs`) resolves it once per session and passes the composed text to `AgentCatalog::with_appended_instructions` (`agens-tools`, `agents.rs`), which appends it to every catalog agent's own prompt: the primary agent, the built-in `explore` and `general` agents, and every markdown-defined custom agent.
- `headless_turn_own_system_prompt` (`agens-headless`, `turn.rs`) appends it to the headless `agens chat` parent turn's own prompt, including when `--system` supplies an explicit prompt. This is the only path that would otherwise miss instructions entirely, since a headless parent turn with no live TUI task runtime never goes through `discover_agent_catalog`.

Every `task` subagent inherits its prompt from the catalog agent it was dispatched from, so the catalog append point already covers it; no separate subagent call site exists.

## Repository contracts

- The devenv scripts declared in `devenv.nix` are the canonical Rust developer command surface.
- `flake.nix` owns the reproducible Rust development environment.
- `CODE_STYLE.md` owns formatting, linting, and testing expectations.
- `AGENTS.md` owns agent-specific workflow rules.
- `target/{debug,release}/agens` contains build outputs; `target/` is never cleaned by verification.

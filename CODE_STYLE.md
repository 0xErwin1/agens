# Code Style

Agens favors explicit, maintainable Rust and small behavior-focused changes. `ARCHITECTURE.md` owns system boundaries, `AGENTS.md` owns agent execution rules, and this document owns engineering and verification standards.

## Toolchain and formatting

Every crate inherits Rust edition 2024 and Rust 1.97.1 from the workspace manifest. Keep `Cargo.toml`, `rust-toolchain.toml`, Nix, and CI aligned when changing the toolchain.

`rustfmt.toml` is the formatting contract: 100-column width, four-space indentation, and Unix newlines. Run formatting checks with:

```sh
nix develop --no-pure-eval -c just fmt-check
```

`just fmt` is intentionally mutating. Do not run it when unrelated or excluded Rust files are dirty. Use `just fmt-check` for verification.

Build outputs remain at `target/{debug,release}/agens`. The target budget is 20 GiB. Verification never cleans automatically; cleanup is manual only through `just clean`.

## Naming, imports, and modules

| Element | Convention | Example |
|---------|------------|---------|
| Crates | lowercase kebab-case | `agens-providers` |
| Types and traits | `PascalCase` | `TurnProvider` |
| Functions and modules | `snake_case` | `run_headless_turn` |
| Fields and locals | `snake_case` | `project_root` |
| Constants and statics | `UPPER_SNAKE_CASE` | `DEFAULT_MCP_TIMEOUT_MS` |
| Tests | descriptive `snake_case` | `expired_deadline_never_persists_a_partial_turn` |

- Prefer complete words over private abbreviations when the longer name improves clarity.
- Keep imports at the top of the module. Group standard-library, external-crate, and local imports in the form already used by the file.
- Prefer `crate::` and `super::` for local module paths.
- Add code to the crate and module that already own the behavior. Do not create a parallel abstraction or many tiny modules without a distinct responsibility.
- Keep public APIs narrow. Use private visibility by default and expose only contracts needed across crate boundaries.
- Treat unreachable public items as defects; the workspace denies `unreachable_pub`.

## API and architecture design

- `agens-core` defines domain types, turn state, cancellation, permission contracts, and adapter ports. It must remain independent of provider, storage, tool, TUI, and CLI implementations.
- `agens-cli` wires concrete adapters and owns command parsing. Do not move domain behavior into the composition root.
- `agens-tui` renders and submits work through the shared runtime. Do not duplicate provider, permission, or persistence logic in the terminal layer.
- TOML configuration belongs in `agens-config`; mutable sessions and persisted grants belong in `agens-store` SQLite databases.
- Preserve existing public behavior during refactors. Public API, side effects, output format, and error categories change only when required by the task.

## Errors and context

- Return typed errors for expected failure modes and preserve the distinction between cancellation, timeout, configuration, authentication, provider, tool, and storage failures.
- Use `Result` and `?` for propagation. Add actionable context at adapter boundaries without embedding secrets or raw sensitive payloads.
- Never silently discard a fallible result. Propagate it, branch explicitly, or record a deliberately sanitized failure.
- Do not use panic as normal control flow. Existing panic-like calls are not a precedent for new production use.
- Avoid indexing when bounds are not already proven by a local invariant. Prefer `get`, iterators, pattern matching, or checked conversion.
- Preserve the primary error when best-effort cleanup also fails; cleanup must not hide cancellation or timeout.

## Async work, cancellation, and deadlines

- Every provider, tool, MCP, process, and turn operation that accepts cancellation or a deadline must check it before and after blocking work.
- Pass the remaining deadline into child operations instead of starting an independent unbounded timeout.
- Cancellation and timeout are distinct outcomes. Do not rewrite either as a generic infrastructure failure.
- Do not detach work that can outlive its owner unless shutdown, error reporting, and resource cleanup are explicit.
- Choose sequential or concurrent execution from the contract, not convenience. Preserve ordering where turn events or tool results are ordered.
- Never hold a mutex guard across blocking I/O or an await boundary.

## Unsafe code

Agens currently uses small unsafe blocks for Unix descriptor, permission, and process-group operations in provider and tool adapters. New unsafe code is allowed only when all of the following are true:

- A safe standard-library or existing dependency API cannot express the required operating-system guarantee.
- The block is confined to the adapter boundary rather than exposed through domain APIs.
- Inputs, ownership, descriptor lifetime, and return-code handling are validated around the block.
- The safety invariant is documented where it cannot be made obvious by structure.
- Focused tests cover the safe wrapper and failure behavior.

The workspace lint remains explicit but non-enforcing for `unsafe_code` until the existing audited blocks can be annotated or isolated without broad exceptions.

## Comments and documentation

- Default to no inline comments. Explain only a hidden constraint, safety invariant, protocol requirement, or surprising tradeoff.
- Prefer function-level documentation for public behavior, invariants, side effects, cancellation, and security properties.
- Never restate syntax or narrate the code line by line.
- Do not reference a task, pull request, or temporary implementation phase in code comments.
- Keep root documents factual. Remove claims when code is not wired rather than presenting intended work as implemented.

## Function size and refactoring

Treat a function growing beyond roughly 100 lines as a design smell. Before adding more logic, consider extracting a named step with one responsibility. Keep refactors local, preserve behavior exactly, and avoid changing public APIs or module ownership without a concrete need.

## Security and boundary rules

- Redact API keys, OAuth tokens, refresh tokens, authorization headers, MCP environment values, and secret-bearing provider responses from errors and diagnostics.
- Keep filesystem tools confined to the discovered project root. Reject traversal, symlink escapes, invalid encodings, and oversized input or output according to existing limits.
- Treat process execution as mutating and permission-gated. Preserve bounded execution, cancellation, process-group termination, and sanitized output.
- Validate network URLs, redirects, headers, response size, retries, and timeouts at the transport boundary. Do not expose raw remote errors that may contain secrets.
- Validate configuration before expansion or execution. Project configuration must not acquire global-only capabilities such as MCP server definitions.
- Use transactions for multi-step persistence changes. Never persist a partial or cancelled turn.
- Preserve restrictive permissions on credential directories, credential files, and SQLite runtime state.

## Workspace lint policy

Every crate must contain `[lints] workspace = true`. `cargo clippy --workspace --all-targets --locked -- -D warnings` makes enabled warnings blocking.

| Lint | Level | Current rationale |
|------|-------|-------------------|
| `rust::unused_must_use` | deny | Fallible return values must be handled. |
| `rust::unreachable_pub` | deny | Public API must be reachable and intentional. |
| `clippy::dbg_macro` | deny | Debug macros must not ship. |
| `clippy::todo` | deny | Incomplete production paths must not ship. |
| `clippy::unimplemented` | deny | Incomplete production paths must not ship. |
| `rust::unsafe_code` | allow | Required by existing audited Unix adapter boundaries. |
| `clippy::unwrap_used` | allow | Existing production and test usage prevents workspace enforcement. |
| `clippy::expect_used` | allow | Existing production and test usage prevents workspace enforcement. |
| `clippy::panic` | allow | Existing tests use explicit panic branches. |
| `clippy::unwrap_in_result` | allow | Existing production usage prevents workspace enforcement. |
| `clippy::indexing_slicing` | allow | Existing parser and state-machine indexing prevents workspace enforcement. |

Do not add broad per-module allows or weaken `-D warnings`. Tighten an allowed lint only after focused cleanup proves the complete all-target workspace remains green.

## Tests and strict TDD

For non-trivial behavior or repository contracts:

1. Write the focused failing test first and observe the expected red result.
2. Implement the smallest complete behavior.
3. Re-run the focused test to green.
4. Run the affected crate or contract checks.
5. Finish with `nix develop --no-pure-eval -c just verify`.

- Keep unit tests near implementation and integration tests under the owning crate's `tests/` directory.
- Prefer table-driven cases for a stable set of input/output branches.
- Test boundary failures, cancellation, deadlines, redaction, traversal, partial persistence, and permission decisions where relevant.
- Use local deterministic protocol doubles unless a live external boundary is explicitly required.
- Tests may use panic-like assertions where they make failures clearer; production code should return typed failures.

## Focused verification matrix

Run commands inside `nix develop --no-pure-eval` unless the full invocation is shown.

| Change area | Focused verification before the full gate |
|-------------|-------------------------------------------|
| Core turn, error, cancellation, or permission contracts | `cargo test -p agens-core --all-targets --locked` |
| TOML, path, expansion, MCP, or permission configuration | `cargo test -p agens-config --all-targets --locked` |
| OpenAI or ChatGPT provider/auth behavior | `cargo test -p agens-providers --all-targets --locked` |
| Native tools, MCP transports, skills, or sub-agent library behavior | `cargo test -p agens-tools --all-targets --locked` |
| Sessions or persisted permission grants | `cargo test -p agens-store --all-targets --locked` |
| TUI rendering or event behavior | `cargo test -p agens-tui --all-targets --locked` |
| CLI parsing, composition, or runtime wiring | `cargo test -p agens --all-targets --locked` |
| Root docs, manifests, scripts, CI, or tooling | `just contracts && just fmt-check && just lint && just deny` |
| Any completed change | `nix develop --no-pure-eval -c just verify` |

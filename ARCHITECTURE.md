# Architecture — Agens

Agens is a Go CLI coding agent. The architecture starts intentionally small: one binary, shallow internal packages, and clear seams for future providers, tools, persistence, and TUI work.

## Current package map

```text
cmd/agens      process entrypoint only
internal/app   application bootstrap and execution seam
internal/cli   Cobra command tree, CLI adapter behavior, and the interactive ttyPrompter (permission UI adapter)
internal/config minimal configuration path contract
internal/auth  on-disk provider credential storage (api_key/access_token entries), no network calls
internal/auth/chatgpt ChatGPT OAuth login flow and token authenticator: PKCE browser login, JWT account-id parsing, and refresh_token-based renewal that persists back through internal/auth; no Cobra
internal/message typed, provider-neutral conversation history model (leaf, no internal deps)
internal/provider provider-neutral contracts for auth, chat streaming, and factory wiring (leaf, Cobra-free, depends only on internal/message)
internal/provider/openai provider.Provider implementation for OpenAI's chat-completions API, API-key authenticated
internal/provider/chatgpt provider.Provider implementation for OpenAI's Responses API ("/responses"), ChatGPT-OAuth authenticated, SSE streaming; no Cobra, no internal/auth
internal/permission rule engine (Allow|Ask|Deny) and Gate decorator gating tool calls before dispatch; depends on internal/message, internal/provider, and github.com/bmatcuk/doublestar/v4; must not import internal/agentloop — the Gate satisfies agentloop.ToolRunner structurally, asserted only from a test file
internal/permission/permissiondb SQLite-backed permission.Store: persists per-project, argument-scoped "allow always"/"deny always" grants so they survive a restart without leaking across projects; depends only on internal/permission
internal/tool    uniform Tool contract and Registry the agent loop dispatches against; Cobra-free, depends on internal/message, internal/provider, jsonschema-go
internal/tool/fs read/write/edit Tool implementations confined to a worktree via os.Root; Cobra-free, depends only on internal/tool, jsonschema-go, and the standard library
internal/tool/bash bash Tool implementation (bash -c from the project root); turn-ctx-aware per-command timeout with turn-cancellation differentiation, process-group kill (no orphaned grandchildren), capped combined stdout/stderr output; Cobra-free, not sandboxed
internal/tool/search grep and glob Tool implementations, worktree-confined via the same os.Root FS as internal/tool/fs (consumed through its FS() accessor); Cobra-free, depends only on internal/tool, doublestar, jsonschema-go, and the standard library
internal/tool/webfetch webfetch Tool implementation: single HTTP GET per call, HTML responses converted to readable text (raw passthrough otherwise), response capped at 100 KiB, 30s default timeout; dial-time SSRF guard blocks link-local and cloud-metadata addresses on every connection attempt, including redirect hops; Ask-default (no seeded permission rule); Cobra-free
internal/prompt assembles the model-family base prompt, runtime environment block, and discovered AGENTS.md/CLAUDE.md instructions into the system prompt; stdlib-only, embeds prompt text via go:embed
internal/agentloop drives one synchronous agent turn loop: streams a provider response, assembles it into a message.Message, and dispatches requested tool calls
internal/agent composition root that wires a config.Config and an auth.File into a ready-to-run *agentloop.Loop (provider, tool registry, permission Gate); no network calls of its own
internal/version build/version metadata
```

## Dependency direction

```text
cmd/agens -> internal/app -> internal/cli
                         -> internal/config
                         -> internal/version

internal/provider -> internal/message
internal/provider/openai -> internal/provider, internal/message
internal/provider/chatgpt -> internal/provider, internal/message, google/uuid, stdlib (Responses-API SSE provider; no Cobra, no internal/auth)

internal/auth -> internal/config
internal/auth/chatgpt -> internal/auth, internal/provider, stdlib (OAuth login + token authenticator; no Cobra)

internal/tool -> internal/message, internal/provider, jsonschema-go
internal/tool/fs -> internal/tool, jsonschema-go, stdlib (no Cobra, no internal/agentloop, no internal/agent, no internal/cli, no internal/message)
internal/tool/bash -> internal/tool, jsonschema-go, stdlib os/exec + syscall (no Cobra, no internal/agentloop, no internal/agent, no internal/cli, no internal/message)
internal/tool/search -> internal/tool, doublestar, jsonschema-go, stdlib io/fs + regexp (no Cobra, no internal/agentloop, no internal/agent, no internal/cli, no internal/tool/fs)
internal/tool/webfetch -> internal/tool, golang.org/x/net/html, jsonschema-go, stdlib net/http + net (no Cobra, no internal/agentloop, no internal/agent, no internal/cli)

internal/permission -> internal/message, internal/provider
internal/permission/permissiondb -> internal/permission, modernc.org/sqlite (leaf-ward: no internal/session import, no cycle)

internal/agentloop -> internal/message, internal/provider

internal/agent -> internal/agentloop, internal/auth, internal/auth/chatgpt, internal/config, internal/permission, internal/prompt, internal/provider, internal/provider/openai, internal/provider/chatgpt, internal/tool, internal/tool/fs, internal/tool/bash, internal/tool/search, internal/tool/webfetch

internal/cli -> internal/agent, internal/agentloop, internal/auth, internal/config, internal/message, internal/permission, internal/permission/permissiondb, golang.org/x/term
             -> (ttyPrompter implements internal/permission.Prompter; it owns terminal I/O and never leaks into internal/agent or internal/tool/fs)
             -> (internal/cli/{tui,chat}.go each open their own project-scoped permissiondb.Store — the persistent Options.PermissionStore backing the real binary)

(future) internal/tui, internal/persistence -> internal/message
```

Rules:

- `cmd/agens` contains no business logic.
- `internal/cli` owns Cobra commands and help behavior.
- Future domain packages must not import Cobra.
- `internal/config` owns TOML bootstrap config loading, project/global merge, environment expansion, and read-only diagnostics.
- `internal/message` is a leaf package: it depends on nothing internal (stdlib + `google/uuid` only) and defines `Message`, `Role`, and the closed `Part` union (`TextPart`, `ToolUsePart`, `ToolResultPart`) with their JSON codec. Future providers (mapping wire formats to/from this model), the agent loop, the TUI, and persistence will all depend on `internal/message`; it must never depend on any of them.
- `internal/tool/fs` confines all file access through a single `os.Root` per `agent.BuildLoop` invocation; it never imports Cobra, `internal/agentloop`, `internal/agent`, or `internal/cli`, keeping the filesystem tools reusable outside the CLI.
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

TOML config files are for hand-authored bootstrap inputs only. Runtime state that Agens mutates — sessions, remembered permissions, model caches, discovered MCP state, and last-used values — belongs in a SQLite-backed store, not in config files. Sessions live in `internal/session/sessiondb`; remembered permission grants live in `internal/permission/permissiondb` (see below) — deliberately separate databases and packages, so neither domain's migrations or failure modes couple to the other's.

## Permissions configuration

An optional `[permissions]` table configures the permission engine's ruleset:

```toml
[permissions]
deny  = ["read(**/.env)"]
allow = ["read(**)", "engram_mem_save(**)"]
```

`allow`/`deny` entries are matchers over agens' NATIVE runtime tool names —
lowercase native tools (`bash`, `read`, `edit`, ...) and `serverName_toolName`
for MCP tools (e.g. `engram_mem_save`) — never a `mcp__…__…` or `Bash(...)`
alias layer. Each entry is `tool(argPattern)` (a doublestar glob matched
against a semantic projection of the call — bash → command, fs → path,
webfetch → url — via the Engine's `Projector`, so an argument-scoped matcher
like `read(**/.env)` above genuinely blocks a `.env` read through the real
gate, not just an isolated engine test) or a bare `tool` matching on name
alone.

Doublestar's glob matching is path-segment based: a bare `*` does not cross a
literal `/`. A pattern meant to block a whole subtree needs `**` written as
its own path segment (for example `command/**`), not a bare `*` — a matcher
like `bash(rm -rf *)` only matches a slash-free command and would NOT block
`rm -rf /` or `rm -rf some/dir`; do not author a deny rule assuming a single
`*` bridges directories. To actually deny `rm -rf` against any path, anchor
`**` at the segment boundary right after the command, for example
`bash(rm -rf /**)` — verified to match both `rm -rf /` and
`rm -rf /some/dir`, unlike the trailing-`*` form above; a bare `**` with no
leading `/` (`bash(rm -rf **)`) does NOT match either, since it never aligns
to a path-segment boundary.

`internal/config` keeps global and project permissions in separate buckets
(`Permissions.{Global,Project}{Allow,Deny}`); a project `[permissions]` patch
is routed only into its own `Project*` fields and never concatenated with the
`Global*` fields. This physical separation is what lets the permission engine
treat a global `deny` as absolute: a project `allow` can never reach, widen,
or override it. Matcher syntax validation happens at composition time
(`permission.ParseRule`), not during config load.

## Chat and edit modes

The Engine enforces a live-mutable operating `Mode` as a hard pre-check
running before the ruleset and before the Prompter: `ModeEdit` (the default)
is today's Allow/Ask/Deny behavior, unaffected. `ModeChat` blocks every call a
`WriteClassifier` flags as a write — the native `write`, `edit`, and `bash`
tools unconditionally (ALL bash is blocked in chat mode, a deliberate
over-approximation versus "only mutating bash"; switch to edit mode to run a
read-only bash command), plus any MCP tool that does not verifiably carry a
`readOnlyHint` annotation. Mode gating can only tighten a decision, never
loosen one: it cannot allow a call the ruleset already denies, and a Deny is
never bypassed by any mode.

Both the parent agent's gate and every subagent's gate share one
`*permission.ModeState`, so a mode switch takes effect for delegations too,
with no loop rebuild. The TUI's `/mode [chat|edit]` command (blank toggles
between the two) flips it live; `--mode` (default `edit`) on both `agens` and
`agens chat` sets the starting mode. Mode is not persisted across sessions.

## Persistent permission grants (permissiondb)

Answering an Ask decision with "allow always" or "deny always" persists a
`permission.Rule` through `internal/permission/permissiondb`, a
`permission.Store` implementation backed by a SQLite database at
`<data home>/agens/permissions.db`. A grant is scoped to both the invoking
project and the specific argument that triggered the Ask (for example
`bash(git status)`, not a blanket `bash` grant), so it never widens beyond
what was actually approved and never leaks into a different project.

`permissiondb.Open(path, project)` binds the project at construction, so the
unchanged `permission.Store` interface (`Append`, `Rules`) stays
project-agnostic in shape while grants stay isolated per project underneath.
It mirrors `internal/session/sessiondb`'s conventions: `SetMaxOpenConns(1)`
plus a mutex for single-writer access, WAL journaling, `synchronous=NORMAL`,
a `busy_timeout`, an idempotent `CREATE TABLE IF NOT EXISTS` migration
stamped with `PRAGMA user_version`, and a lazily created database file. It is
hand-written SQL rather than sqlc-generated, since the store is a two-query
package that does not warrant sqlc's generation step.

## Repository contracts

- `justfile` is the canonical developer command surface.
- `flake.nix` owns the reproducible development environment.
- `CODE_STYLE.md` owns formatting, linting, and testing expectations.
- `AGENTS.md` owns agent-specific workflow rules.

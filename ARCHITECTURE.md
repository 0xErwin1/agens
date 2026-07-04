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
internal/tool    uniform Tool contract and Registry the agent loop dispatches against; Cobra-free, depends on internal/message, internal/provider, jsonschema-go
internal/tool/fs read/write/edit Tool implementations confined to a worktree via os.Root; Cobra-free, depends only on internal/tool, jsonschema-go, and the standard library
internal/tool/bash bash Tool implementation (bash -c from the project root); turn-ctx-aware per-command timeout with turn-cancellation differentiation, process-group kill (no orphaned grandchildren), capped combined stdout/stderr output; Cobra-free, not sandboxed
internal/tool/search grep and glob Tool implementations, worktree-confined via the same os.Root FS as internal/tool/fs (consumed through its FS() accessor); Cobra-free, depends only on internal/tool, doublestar, jsonschema-go, and the standard library
internal/tool/webfetch webfetch Tool implementation: single HTTP GET per call, HTML responses converted to readable text (raw passthrough otherwise), response capped at 100 KiB, 30s default timeout; dial-time SSRF guard blocks link-local and cloud-metadata addresses on every connection attempt, including redirect hops; Ask-default (no seeded permission rule); Cobra-free
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

internal/agentloop -> internal/message, internal/provider

internal/agent -> internal/agentloop, internal/auth, internal/auth/chatgpt, internal/config, internal/permission, internal/provider, internal/provider/openai, internal/provider/chatgpt, internal/tool, internal/tool/fs, internal/tool/bash, internal/tool/search, internal/tool/webfetch

internal/cli -> internal/agent, internal/agentloop, internal/auth, internal/config, internal/message, internal/permission, golang.org/x/term
             -> (ttyPrompter implements internal/permission.Prompter; it owns terminal I/O and never leaks into internal/agent or internal/tool/fs)

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

TOML config files are for hand-authored bootstrap inputs only. Runtime state that Agens mutates — sessions, remembered permissions, model caches, discovered MCP state, and last-used values — belongs in a future SQLite-backed store, not in config files.

## Repository contracts

- `justfile` is the canonical developer command surface.
- `flake.nix` owns the reproducible development environment.
- `CODE_STYLE.md` owns formatting, linting, and testing expectations.
- `AGENTS.md` owns agent-specific workflow rules.

# Agens

Agens is a Rust coding-agent CLI with a terminal interface, one-shot chat, guarded project tools, MCP tool integration, and persisted completed turns.

## Current capabilities

- Interactive TUI launched by bare `agens`.
- One-shot agent turns through `agens chat <prompt>`.
- OpenAI Responses API access with `provider.type = "openai-api"` and `OPENAI_API_KEY` or an existing `auth.json` entry.
- ChatGPT subscription Responses access with OAuth login through `agens auth login`.
- Moonshot AI (Kimi) chat-completions access with `provider.type = "moonshotai"` and `MOONSHOT_API_KEY` or an `auth.json` entry, written by `agens auth login api-key moonshotai`.
- A cancellation-aware provider/tool loop with a 120-second top-level deadline.
- Project-confined native tools: `read`, `write`, `list`, `search`, and bounded `bash`.
- Permission evaluation before tool execution, including global/project TOML rules, temporary unsafe bypass, and persisted project grants. Unresolved approval requests fail closed.
- MCP tools loaded from global configuration over stdio, streamable HTTP, or SSE transports.
- Completed-turn and project-grant persistence in SQLite.
- Nix-first development and one canonical verification gate.

Agens exposes two runtime surfaces: the interactive TUI and headless `chat` command. The TUI uses the same provider, permission, tool, cancellation, and persistence runtime as headless chat.

## Quick start

Enter the development shell and build:

```sh
nix develop --no-pure-eval
just build
```

For ChatGPT subscription authentication:

```sh
./target/debug/agens auth login
./target/debug/agens auth status
```

For API-key authentication, set the key and select the provider in configuration:

```sh
export OPENAI_API_KEY="..."      # provider.type = "openai-api"
export MOONSHOT_API_KEY="..."    # provider.type = "moonshotai"
```

```toml
[provider]
type = "openai-api"
```

An unrecognized `provider.type` is rejected at startup rather than falling back to a
default, so a typo cannot send a run to a provider you did not name.

Run the TUI or a one-shot prompt:

```sh
./target/debug/agens
./target/debug/agens chat "Explain this repository"
```

## Command surface

```text
agens [--resume [session-id]]
agens auth <status|login|logout>
agens chat [--model <id>] [--system <prompt>] [--max-iterations <n>] [--mode <chat|edit>] [--dangerously-allow-all] <prompt>
agens config doctor
agens sessions <list|show|rm>
agens models
agens --help
agens --version
```

`--dangerously-allow-all` temporarily bypasses tool confirmation for that turn. It is unsafe and should be limited to controlled environments.

The TUI accepts normal prompts and these slash commands:

- `/new` starts a fresh session context.
- `/sessions` lists completed turns.
- `/resume <id>` restores saved assistant text as context for the next prompt.
- `/bypass` toggles permission-prompt bypass for the session; `Ctrl+Shift+P` does the same.

Keyboard controls shown by the TUI include Enter to send, Shift+Enter for a newline, Ctrl+C to cancel or quit, Page Up/Page Down to scroll, and End to follow.

`/bypass` and `Ctrl+Shift+P` upgrade `Ask` decisions to `Allow` for the rest of the session; a footer `BYPASS` segment shows while it is active. The toggle never writes configuration and decides only calls that no rule and no grant matches: a matched `deny` or `ask`, configured or declared by an agent, still decides, as do the unconditional safety checks (worktree escapes, writes in chat mode). Subagents launched from the TUI inherit the active session's bypass; a subagent the model launches itself through the `task` tool does not. Setting `agent.bypass_permission_prompts = true` in the GLOBAL configuration turns bypass on by default for every new session and for `/new`; a project `.agens/config.toml` cannot enable it, and `agens config doctor` warns if one declares it anyway. Resuming a session restores whatever value that session last recorded, regardless of the current configuration.

## Configuration

Agens loads hand-authored TOML from:

| Scope | Path |
|-------|------|
| Global override | `$AGENS_CONFIG_HOME/config.toml` |
| XDG global | `$XDG_CONFIG_HOME/agens/config.toml` |
| Default global | `~/.config/agens/config.toml` |
| Project | `<project-root>/.agens/config.toml` |

The project root is the nearest Git ancestor or the current directory outside a repository. Project values override global values. Missing files are valid.

A minimal configuration can select the provider, model, runtime limits, and permission policy:

```toml
[provider]
type = "openai-chatgpt"
model = "gpt-5.5"

[agent]
system_prompt = "You are a careful coding agent."
max_iterations = 60

[permissions]
allow = ["read(**)", "list(**)", "search(**)"]
deny = ["bash(rm *)"]
```

The optional `[options].data_dir` changes the runtime-state directory. Environment expressions are supported by the configuration parser. MCP server definitions are global-only; project configuration cannot define them.

Beyond the keys above, `[tools]` bounds the native tools (`max_list_entries`, `max_search_entries`, `max_search_results`, `max_search_depth`, `operation_timeout_ms`, `bash_timeout_ms`), `[subagents]` bounds the task tool (`max_iterations`, `max_concurrency`, `max_output_chars`), `[mcp_defaults]` supplies `timeout_ms` and `max_retries` to servers that omit their own, and `[agent]` also accepts `default_agent`, `reasoning_effort`, and `bypass_permission_prompts` (bool, default `false`; global configuration only — see "Command surface" for the runtime `/bypass` toggle). Setting `[options].debug = false` stops agens from capturing diagnostics to disk.

Every key is validated on load: an unknown key, a wrong type, or a value outside its documented range fails startup and names the offending field. The authoritative list is the settings catalog in `crates/agens-config`; `agens config init` renders it as a commented starter file at `<project-root>/.agens/config.toml` and refuses to overwrite an existing one.

Inspect resolved paths and validation status with:

```sh
./target/debug/agens config doctor
```

## Subagents

The `task` tool delegates work to a subagent, which runs its own turn loop with its own tool dispatcher and cannot delegate further.

**What a subagent can reach.** It inherits the parent's native tool surface — `read`, `list`, `search`, `grep`, `glob`, `git_read`, `write`, `edit`, `bash`, `webfetch` — plus `task_control` and `task_message` for reporting back. The read-class tools (`read`, `list`, `search`, `grep`, `glob`, `git_read`) are authorized automatically. `write`, `edit`, `bash` and `webfetch` are present but unauthorized: a call to one is refused unless the agent definition declares `allow`, or the session is in dangerous mode. `webfetch` is excluded from the automatic grant deliberately — it is network egress rather than a worktree read.

**What the automatic grant costs.** The read-class grant is not bounded by the `[permissions]` path rules. `grep` and `glob` are matched on their pattern argument, and `bash` on its command line, so a `deny` naming a path never reaches any of the three — and `grep` reports the lines it matched. A subagent that declares nothing can therefore read the contents of any file in the worktree, including one the configuration denies to `read`. On the primary agent that same `grep` call stops at an approval prompt; a subagent has no prompt to stop at.

**Narrowing and granting.** Agent definitions are markdown files with YAML frontmatter, discovered in `<project-root>/.agens/agents/` and in `agents/` beside the global configuration. A `permissions:` list adjusts the inherited surface:

```markdown
---
name: reviewer
description: Review a change without modifying it
mode: subagent
permissions:
  - deny write
  - deny edit
  - deny bash
  - deny webfetch
---
```

Each entry is `<allow|deny|ask> <tool> [target]`, split on whitespace, so a multi-word target such as `rm -rf /**` cannot be written here and belongs in the TOML `[permissions]` block instead.

| Declaration | Effect in the subagent |
|-------------|------------------------|
| `allow <tool>` | Authorizes the tool with no approval prompt. |
| `deny <tool>` | Removes the tool from the catalog entirely — the subagent is never told it exists. |
| `deny <tool> <target>` | Keeps the tool; precedence decides each call, so the narrower rule wins. |
| `ask <tool>` | Refused. A subagent cannot reach a human, and the denial says the prompt was unreachable rather than pretending to be a plain `deny`. |

**Ceilings a declaration cannot raise.** A declaration only narrows. An `allow` naming a tool the parent does not hold, or one the operator's `[permissions]` block denies outright, fails the delegation and names the offending tool rather than silently clamping it. Configured rules always reach the subagent: a configured `deny` or `ask` holds there even against a declared `allow`, while a configured `allow` grants a subagent nothing it did not declare for itself.

Built-in `explore` ships with `deny write`, `deny edit`, `deny bash` and `deny webfetch`, so it stays read-only across upgrades. Granting `bash` to a subagent grants the same unconfined shell described under "Persistence and security".

## Persistence and security

Credentials live in `auth.json` under the selected config home, keyed by provider. Each
provider reads its own environment variable — `OPENAI_API_KEY` or `MOONSHOT_API_KEY` — which
takes precedence over the stored entry. A key configured for one provider never authenticates
a run against another. ChatGPT OAuth writes only its own provider entry and preserves the
others.

Mutable runtime state lives under `[options].data_dir` or `${XDG_DATA_HOME:-~/.local/share}/agens` in a single `agens.db` SQLite file:

- Sessions and completed turn events.
- Project-scoped permission grants.
- The last model and reasoning effort chosen in the terminal UI. A new session reuses them only when neither a CLI flag nor configuration names a model.

Credential and runtime-state directories/files are created with restrictive Unix permissions. CLI diagnostics and transport errors are designed to avoid exposing secret values. The native filesystem tools — `read`, `write`, `edit`, `list`, `search`, `grep`, `glob` — are confined beneath the project root.

`bash` is not. It is permission-gated and time-bounded, and it starts in the project root, but a granted command can still reach paths outside it. The `[permissions]` rules are the only bound on what it runs, and they are pattern matching over the command line rather than containment: they see through chained commands, wrappers such as `sudo`, and `/bin/rm` versus `rm`, but a command written to obscure which binary it invokes can still evade them. Grant `bash` deliberately, and treat any `deny` rule on a path as unenforced against it.

Hand-authored TOML is configuration, not a runtime database. Do not store mutable sessions or grants in TOML.

## Architecture

The workspace contains seven crates:

| Crate | Responsibility |
|-------|----------------|
| `agens-core` | Messages, turn state, cancellation, errors, permissions, and adapter ports |
| `agens-config` | TOML validation, merging, expansion, paths, MCP definitions, and permission rules |
| `agens-providers` | OpenAI and ChatGPT authentication and streaming adapters |
| `agens-tools` | Native tools, permission dispatch, MCP transports, and reusable skill/sub-agent library contracts |
| `agens-store` | SQLite completed turns, persisted project grants, and remembered selections |
| `agens-tui` | Terminal rendering and input over the shared runtime |
| `agens-cli` | Command parsing, adapter wiring, and the `agens` binaries |

`agens-cli` is the composition root. `agens-core` does not depend on adapters, and `agens-tui` is a surface adapter rather than a separate runtime. See `ARCHITECTURE.md` for the canonical dependency direction.

## Development

Use the root `justfile` inside `nix develop --no-pure-eval`:

```sh
just fmt-check    # check rustfmt without modifying files
just lint         # Clippy for all workspace targets with warnings denied
just test         # workspace tests
just build        # build the workspace
just contracts    # repository bootstrap and standards contracts
just deny         # dependency advisories, licenses, bans, and sources
just verify       # canonical complete gate
just clean        # manual build-output cleanup
```

Build outputs are `target/{debug,release}/agens`. The directory has a 50 GiB budget. Verification checks the budget and never cleans automatically; cleanup is manual only with `just clean`.

Before considering a change complete, run:

```sh
nix develop --no-pure-eval -c just verify
```

## Known limitations

- `agens models` is reserved in the command surface but currently reports that the capability is unavailable.
- The production tool catalog wires native tools, configured MCP tools, and the `skill` and `task` tools. A delegated subagent reaches native tools only; MCP tools are not passed through to it.
- TUI model and reasoning-effort palettes are not implemented; use configuration or `agens chat --model` for model selection.
- Packaging, release automation, and editor protocol integrations are not provided.

## Documentation

- `ARCHITECTURE.md`: crate boundaries and runtime dependency direction, including how global and project `AGENTS.md` instructions are discovered and appended to every agent's prompt.
- `AGENTS.md`: concise execution rules for coding agents.
- `CODE_STYLE.md`: Rust engineering, lint, security, and verification standards.
- `CONTRIBUTING.md`: setup, TDD, review, dependency, and security workflow.
- `CLAUDE.md`: thin pointer to the canonical documents.

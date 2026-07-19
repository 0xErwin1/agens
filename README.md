# Agens

Agens is a Rust coding-agent CLI with a terminal interface, one-shot chat, guarded project tools, MCP tool integration, and persisted completed turns.

## Current capabilities

- Interactive TUI launched by bare `agens`.
- One-shot agent turns through `agens chat <prompt>`.
- OpenAI Responses API access with `provider.type = "openai-api"` and `OPENAI_API_KEY` or an existing `auth.json` entry.
- ChatGPT subscription Responses access with OAuth login through `agens auth login`.
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

For OpenAI API authentication, set the key and select the provider in configuration:

```sh
export OPENAI_API_KEY="..."
```

```toml
[provider]
type = "openai-api"
```

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

Keyboard controls shown by the TUI include Enter to send, Shift+Enter for a newline, Ctrl+C to cancel or quit, Page Up/Page Down to scroll, and End to follow.

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

Inspect resolved paths and validation status with:

```sh
./target/debug/agens config doctor
```

## Persistence and security

Credentials live in `auth.json` under the selected config home. `OPENAI_API_KEY` takes precedence for the OpenAI API provider. ChatGPT OAuth writes only its own provider entry and preserves other entries.

Mutable runtime state lives under `[options].data_dir` or `${XDG_DATA_HOME:-~/.local/share}/agens`:

- `rust-sessions.db` stores completed turn events.
- `rust-permissions.db` stores project-scoped permission grants.

Credential and runtime-state directories/files are created with restrictive Unix permissions. CLI diagnostics and transport errors are designed to avoid exposing secret values. Native filesystem operations are confined beneath the project root, and process tools remain permission-gated and bounded.

Hand-authored TOML is configuration, not a runtime database. Do not store mutable sessions or grants in TOML.

## Architecture

The workspace contains seven crates:

| Crate | Responsibility |
|-------|----------------|
| `agens-core` | Messages, turn state, cancellation, errors, permissions, and adapter ports |
| `agens-config` | TOML validation, merging, expansion, paths, MCP definitions, and permission rules |
| `agens-providers` | OpenAI and ChatGPT authentication and streaming adapters |
| `agens-tools` | Native tools, permission dispatch, MCP transports, and reusable skill/sub-agent library contracts |
| `agens-store` | SQLite completed turns and persisted project grants |
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

Build outputs are `target/{debug,release}/agens`. The directory has a 20 GiB budget. Verification checks the budget and never cleans automatically; cleanup is manual only with `just clean`.

Before considering a change complete, run:

```sh
nix develop --no-pure-eval -c just verify
```

## Known limitations

- `agens models` is reserved in the command surface but currently reports that the capability is unavailable.
- CLI-managed OpenAI API-key login is not implemented; use `OPENAI_API_KEY` or an existing `openai-api` entry in `auth.json`.
- The production tool catalog currently wires native tools and configured MCP tools. Skill discovery and sub-agent contracts exist in `agens-tools` but are not exposed as production tools yet.
- TUI model and reasoning-effort palettes are not implemented; use configuration or `agens chat --model` for model selection.
- Packaging, release automation, and editor protocol integrations are not provided.

## Documentation

- `ARCHITECTURE.md`: crate boundaries and runtime dependency direction.
- `AGENTS.md`: concise execution rules for coding agents.
- `CODE_STYLE.md`: Rust engineering, lint, security, and verification standards.
- `CONTRIBUTING.md`: setup, TDD, review, dependency, and security workflow.
- `CLAUDE.md`: thin pointer to the canonical documents.

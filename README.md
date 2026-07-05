# Agens

Agens is a Go CLI coding agent with an interactive terminal UI, one-shot chat mode, provider integrations, guarded tool execution, and saved conversations.

## What works today

- Interactive Bubble Tea TUI launched by bare `agens`.
- One-shot prompt streaming with `agens chat`.
- Provider support for:
  - `openai-api`: OpenAI Chat Completions API with an API key.
  - `openai-chatgpt`: ChatGPT/Codex subscription backend with OAuth login.
- Model listing and live model selection.
- Reasoning-effort selection for providers that support it.
- Agent loop with tool calls.
- Built-in tools:
  - `read`, `write`, `edit` for worktree-confined filesystem access.
  - `grep`, `glob` for worktree-confined search.
  - `bash` for project-root shell commands.
  - `webfetch` for one HTTP GET with SSRF protections and capped output.
- Permission gate for tool calls. Read/search tools are allowed by default; mutating, shell, and web tools ask unless overridden.
- Project instructions discovery from `AGENTS.md` and `CLAUDE.md`.
- Session persistence and resume in the TUI.
- Nix-first developer environment and `just` workflows.

Still intentionally out of scope: MCP, skills, sub-agents, packaging/release automation, and a SQLite runtime store.

## Quick start

Enter the development shell and build the binary:

```sh
nix develop
just build
```

Authenticate with one provider:

```sh
# ChatGPT/Codex subscription OAuth login
./agens auth login

# or OpenAI API-key auth
./agens auth login api-key openai-api
```

Run the interactive UI:

```sh
./agens
```

Or send a one-shot prompt:

```sh
./agens chat "Explain this repository"
```

## Commands

```sh
./agens --help
./agens auth login
./agens auth login api-key openai-api
./agens auth status
./agens auth logout <provider>
./agens config doctor
./agens models
./agens chat [prompt]
./agens --resume [session-id]
```

Common flags on interactive and chat modes:

```sh
--model <id>                 override the configured model
--system <prompt>            override the configured system prompt
--max-iterations <n>         override the agent loop iteration limit
--dangerously-allow-all      auto-approve every tool call without prompting (unsafe)
```

## Interactive UI

Run `agens` with no subcommand to open the TUI.

Slash commands:

- `/new` or `/clear` — start a fresh conversation.
- `/model` — choose a model from the provider catalog.
- `/effort` — set reasoning effort.
- `/sessions` — resume a saved conversation.
- `/help` — show commands and shortcuts.
- `/quit` — exit.

Shortcuts:

- `enter` — send.
- `ctrl+c` — cancel a running turn, clear input, or quit from idle.
- `ctrl+o` — expand/collapse tool output and thinking.
- `ctrl+p` — toggle detailed token usage.
- `pgup` / `pgdn` — scroll the conversation.
- `esc` — close palette or deny a permission prompt.

Type `@` in the prompt to open the project file picker; selected file references are expanded into the user message.

## Configuration

Agens uses TOML files for hand-authored bootstrap configuration:

- Global: `~/.config/agens/config.toml`.
- If `AGENS_CONFIG_HOME` is set: `$AGENS_CONFIG_HOME/config.toml`.
- If `XDG_CONFIG_HOME` is set: `$XDG_CONFIG_HOME/agens/config.toml`.
- Project: `<project-root>/.agens/config.toml`, where project root is the nearest `.git` ancestor or the current directory outside git.

Project config overrides global config. Missing files are valid and load defaults.

Example:

```toml
[options]
# Enables debug-oriented behavior where supported.
debug = false
# Runtime data directory. Environment variables are expanded.
data_dir = "$HOME/.local/share/agens"

[provider]
# Optional. Valid values: "openai-api" or "openai-chatgpt".
# If omitted, Agens infers the provider from stored credentials, preferring
# ChatGPT OAuth credentials over an OpenAI API key when both exist.
type = "openai-chatgpt"
# Optional. Defaults by provider: gpt-4.1 for openai-api, gpt-5.5 for openai-chatgpt.
model = "gpt-5.5"
# Optional provider base URL override. Environment variables are expanded.
base_url = ""

[agent]
# Optional replacement for the built-in base system prompt.
system_prompt = ""
# Maximum model/tool loop iterations for one prompt. Must be >= 1.
# CLI --max-iterations overrides this value; unset uses the internal default.
max_iterations = 60
# Allow providers to emit independent tool calls in the same assistant turn.
# Set false as a rollback knob if grouped tool calls cause provider issues.
parallel_tool_calls = true
```

Validate the loaded config with:

```sh
./agens config doctor
```

## Credentials and sessions

Credentials are stored in `auth.json` under the same config home used for `config.toml`. The file stores provider entries keyed by provider id (`openai-api`, `openai-chatgpt`). CLI status output redacts secrets.

Sessions are currently stored as JSON files under:

```text
${XDG_DATA_HOME:-~/.local/share}/agens/sessions
```

Mutable runtime state should not be added to TOML config. Future persistent state should move to the planned runtime store.

## Development

This project is Nix-first:

```sh
nix develop
```

Canonical commands:

```sh
just fmt        # format Go code
just fmt-check  # check formatting
just lint       # golangci-lint
just test       # go test ./...
just build      # build local ./agens binary
just verify     # fmt-check + lint + test + build
just clean      # remove local build output
```

Before considering a code change complete, run:

```sh
nix develop -c just verify
```

## Architecture

The entrypoint is intentionally thin:

```text
cmd/agens -> internal/app -> internal/cli
```

Core package responsibilities are documented in `ARCHITECTURE.md`. Contributor workflow and style rules live in `CONTRIBUTING.md`, `CODE_STYLE.md`, and `AGENTS.md`.

## TDD

For non-trivial production changes:

1. Write or update the failing test first.
2. Capture the red result.
3. Implement the smallest change.
4. Capture the green result.
5. Finish with `nix develop -c just verify`.

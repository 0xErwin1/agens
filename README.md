# Agens

Agens is a Rust CLI coding agent with an interactive terminal UI, one-shot chat mode, provider integrations, guarded tool execution, and saved conversations.

## What works today

- Interactive Rust TUI launched by bare `agens`.
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

Still intentionally out of scope: packaging/release automation and editor protocol integrations.

## Quick start

Enter the development shell and build the binary:

```sh
nix develop --no-pure-eval
just build
```

Authenticate with one provider:

```sh
# ChatGPT/Codex subscription OAuth login
./target/debug/agens auth login

# or OpenAI API-key auth
./target/debug/agens auth login api-key openai-api
```

Run the interactive UI:

```sh
./target/debug/agens
```

Or send a one-shot prompt:

```sh
./target/debug/agens chat "Explain this repository"
```

## Commands

```sh
./target/debug/agens --help
./target/debug/agens auth login
./target/debug/agens auth login api-key openai-api
./target/debug/agens auth status
./target/debug/agens auth logout <provider>
./target/debug/agens config doctor
./target/debug/agens models
./target/debug/agens chat [prompt]
./target/debug/agens --resume [session-id]
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
./target/debug/agens config doctor
```

## Credentials and sessions

Credentials are stored in `auth.json` under the same config home used for `config.toml`. The file stores provider entries keyed by provider id (`openai-api`, `openai-chatgpt`). CLI status output redacts secrets.

Sessions are stored in a SQLite database under:

```text
${XDG_DATA_HOME:-~/.local/share}/agens/sessions.db
```

Mutable runtime state belongs in the runtime store, not in TOML config.

## Development

This project uses devenv through the Nix development shell:

```sh
nix develop --no-pure-eval
```

Rust is the only product implementation. `just build` produces the development binary at `target/debug/agens`; release builds use `target/release/agens`.

Canonical commands:

```sh
just fmt          # format the Rust workspace
just fmt-check    # check Rust formatting
just lint         # run Clippy with warnings denied
just test         # run Rust workspace tests
just build        # build target/debug/agens
just verify       # Rust budget, format, lint, test, and build gate
just clean        # remove Rust build output
```

Rust `target/` has a 20 GiB limit. Verification reports an overage but never deletes artifacts; cleanup is manual only with `just clean`.

Before considering a code change complete, run:

```sh
nix develop --no-pure-eval -c just verify
```

## Architecture

The entrypoint is intentionally thin:

```text
crates/agens-cli -> agens-core, agens-config, agens-providers, agens-tools, agens-store, agens-tui
```

Core package responsibilities are documented in `ARCHITECTURE.md`. Contributor workflow and style rules live in `CONTRIBUTING.md`, `CODE_STYLE.md`, and `AGENTS.md`.

## TDD

For non-trivial production changes:

1. Write or update the failing test first.
2. Capture the red result.
3. Implement the smallest change.
4. Capture the green result.
5. Finish with `nix develop --no-pure-eval -c just verify`.

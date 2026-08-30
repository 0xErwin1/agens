# Agens

Agens is a Rust coding-agent CLI with a terminal interface, one-shot chat, guarded project tools, MCP tool integration, and persisted completed turns.

## Current capabilities

- Interactive TUI launched by bare `agens`.
- One-shot agent turns through `agens chat <prompt>`.
- OpenAI Responses API access with `provider.model = "openai-api/<model>"` and `OPENAI_API_KEY` or an existing `auth.json` entry.
- ChatGPT subscription Responses access with OAuth login through `agens auth login`.
- Moonshot AI (Kimi) chat-completions access with `provider.model = "moonshotai/<model>"` and `MOONSHOT_API_KEY` or an `auth.json` entry, written by `agens auth login api-key moonshotai`.
- Cancellation-only CLI turns with optional inherited deadlines and finite provider/tool operation timeouts.
- Project-confined native tools: `read`, `write`, `list`, `search`, and bounded `bash`.
- A session that can move: `worktree` creates a git worktree for the work and moves into it, `cd` moves the session to another reachable directory, and every later call resolves relative paths and runs commands there.
- Permission evaluation before tool execution, including global/project TOML rules, temporary unsafe bypass, persisted project grants, and externally answerable unattended questions. Unresolved approval requests fail closed.
- MCP tools loaded from global configuration over stdio, streamable HTTP, or SSE transports.
- Completed-turn and project-grant persistence in SQLite.
- Nix-first development and one canonical verification gate.

Agens exposes two runtime surfaces: the interactive TUI and headless `chat` command. The TUI uses the same provider, permission, tool, cancellation, and persistence runtime as headless chat.

## Quick start

Enter the development shell and build:

```sh
nix develop --no-pure-eval
build
```

For ChatGPT subscription authentication:

```sh
./target/debug/agens auth login
./target/debug/agens auth status
```

For API-key authentication, set the key and select the provider in configuration:

```sh
export OPENAI_API_KEY="..."      # openai-api
export MOONSHOT_API_KEY="..."    # moonshotai
```

```toml
[provider]
model = "openai-api/gpt-4.1"
```

The model identifier is what says which provider a request goes to. A `provider/model`
prefix names it outright. A bare identifier resolves only while exactly one
authenticated provider serves it: with two, the run is refused by name rather than sent,
and its spend charged, to a provider you did not name. `agens models` lists every
provider's catalog under the identifier that names it.

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
model = "openai-chatgpt/gpt-5.5"

[agent]
system_prompt = "You are a careful coding agent."
max_iterations = 60

[permissions]
allow = ["read(**)", "list(**)", "search(**)"]
deny = ["bash(rm *)"]
```

The optional `[options].data_dir` changes the runtime-state directory. Environment expressions are supported by the configuration parser. MCP server definitions are global-only; project configuration cannot define them.

Beyond the keys above, `[tools]` bounds the native tools (`max_list_entries`, `max_search_entries`, `max_search_results`, `max_search_depth`, `operation_timeout_ms`, `bash_timeout_ms`), `[subagents]` configures advisory progress checks and task-tool resource bounds (`check_interval`, `max_concurrency`, `max_output_chars`), `[mcp_defaults]` supplies `timeout_ms` and `max_retries` to servers that omit their own (`timeout_ms` bounds a single MCP tool call; the connect handshake and the tool listing keep their own floors of 10s and only widen when `timeout_ms` is larger), and `[agent]` also accepts `default_agent`, `reasoning_effort`, `bypass_permission_prompts`, `unattended_permission_wait_ms` (5 minutes by default), and `deny_unattended_permission_prompts` (bool, default `false`). Headless parents and unattended children open a durable permission question and wait up to that budget for `agens direct --answer`; setting `deny_unattended_permission_prompts = true` explicitly restores the prior immediate-denial behavior. Setting `[options].debug = false` stops agens from capturing diagnostics to disk.

Each provider operation has a fixed 10-minute timeout, capped by any earlier inherited deadline.

Every key is validated on load: an unknown key, a wrong type, or a value outside its documented range fails startup and names the offending field. The authoritative list is the settings catalog in `crates/agens-config`; `agens config init` renders it as a commented starter file at `<project-root>/.agens/config.toml` and refuses to overwrite an existing one.

Inspect resolved paths and validation status with:

```sh
./target/debug/agens config doctor
```

## Subagents

The `task` tool delegates work to a subagent, which runs its own turn loop with its own tool dispatcher and cannot delegate further.

**Where the session works.** The session starts in its confinement root and can move: `worktree <name>` creates a git worktree on a new branch under the Agens data directory and moves the session into it, and `cd <path>` moves it to any directory under the confinement root or under this project's own worktrees. Nothing else is reachable, so the move is bounded by the same root a path is. Later calls — `read`, `write`, `edit`, `list`, `search`, `grep`, `glob`, `bash` — resolve against wherever the session is, and the footer names it. The move outlives the turn it was made in and is recorded in the run's diagnostics as `working_directory_changed`.

**What a subagent can reach.** It inherits the parent's native tool surface — `read`, `list`, `search`, `grep`, `glob`, `git_read`, `write`, `edit`, `bash`, `webfetch` — plus `task_control` and `task_message` for reporting back. It never holds `cd` or `worktree`: moving the session is the session's own decision, and a child moves nothing its parent can see. The read-class tools (`read`, `list`, `search`, `grep`, `glob`, `git_read`) are authorized automatically. `write`, `edit`, `bash` and `webfetch` are present but unauthorized: a call to one is refused unless the agent definition declares `allow`, or the session is in dangerous mode. `webfetch` is excluded from the automatic grant deliberately — it is network egress rather than a worktree read.

**What the automatic grant costs.** The read-class grant is bounded by the `[permissions]` rules that name each tool, and by nothing else. A rule names one tool, so `deny grep(**/.env)` keeps a subagent out of that file while `deny read(**/.env)` alone does not, and neither covers `search`.

**Which tools a path deny reaches.** A path deny reaches a tool when that tool asks the rules about the file. There are exactly two ways it can:

1. **The call names the file.** The path the call is given is the target the rule is matched against, so a deny refuses the call outright.
2. **The call reports files it never named.** The tool asks the same rules once per file, while it runs, and omits what they deny.

Four kinds of call that can return the contents of a file do neither, and each is an exception for its own reason:

- **`bash`** — a rule written for it is matched against the command line rather than against any path, and the command chooses what it prints. The exception is total.
- **`skill`** — a skill call is named by a skill name, so a rule written against a path does not select it by its target. Skills come from two roots. A skill installed under `<project-root>/.agens/skills/` reports the file the call would open, so `deny skill(**/.agens/**)` refuses it the same way a `deny` on a search's path does. A skill installed beside the global configuration normally has no project-relative path for a rule to name, and there the exception stands — bounded by what the tool can open: a skill's files are read relative to that skill's own directory, under a single plain filename with no traversal, refusing symbolic links and files carrying more than one link. It can return that skill's own installed assets and nothing else, and a subagent never holds it at all. The exception is a property of the two paths rather than of the origin: where the global skills directory does sit under the session's root — a session rooted at your home directory — a global skill has a project-relative path too, and a path deny binds it.
- **`task`** — a task call is named by the subagent it resolves to, so `deny task(reviewer)` refuses every delegation to that agent and no path rule selects one. What the call returns is whatever the subagent reports, which no rule here reaches — but the subagent read those files under these same rules, so a file this configuration denies was already withheld before the report was written.
- **MCP tools** — a remote tool is named `<server>::<tool>`, and what its arguments mean is defined by the server that serves it, so agens cannot tell which file, if any, a given call would read. There is no path to decide, and a rule written against one binds the tool and then selects none of its calls: `deny filesystem::read_text_file(**/.env)` resolves at startup and matches nothing ever after. The rule that binds names the tool and no target — `deny filesystem::read_text_file` refuses every call to it. **So the answer to "can an MCP server reach my `.env`?" is: if that server can read files, yes, and no path rule in `[permissions]` stops it.** What stops it is not configuring the server, or denying its tools by name. This exception is the whole class, whatever a given server returns.

That is the test to apply to any tool added later: if it can return the contents of a file and does neither of the two, a path deny does not bind it, and it belongs on this list with the reason written down.

**The two names one remote tool answers to.** A rule may name a remote tool as `<server>::<tool>` or as `<server>_<tool>`, the name the model itself is advertised. Both bind the same tool. Only the first says on its own that the name is remote — the second is shaped exactly like a bare native name, and nothing distinguishes `filesystem_read_text_file` from a misspelt `webfetc` except your own `[mcp.*]` blocks. Prefer the `::` spelling in a rule; agens accepts either.

**Writing a rule for a server that is not running.** A rule naming a remote tool is resolved against what this session actually reached, and a server that failed to start or is configured `disabled` contributes no tools. Such a rule is kept exactly as written — in either spelling — and binds the moment that server is reachable again. The cost is that it decides nothing while the server is away, which is no weaker than the server not being there.

What makes that possible is your configuration naming the server. A rule for a server no `[mcp.*]` block declares is indistinguishable from a typo, as is a tool name misspelt against a server that IS running — that surface can answer for it — and as is a misspelt native name.

**A `deny` or an `ask` written that way is rejected; an `allow` is dropped.** An unresolvable `allow` could only ever widen, and a grant for a tool nothing can call grants nothing, so refusing to start over one buys you nothing. It also costs more than it looks: `[permissions]` may live in a project file committed to a repository while `[mcp.*]` is global-only, so a stale `allow` naming a removed server's tool travels into every collaborator's checkout. A `deny` or an `ask` is the opposite case — dropping one would leave you believing a restriction is in force when the name you wrote reaches nothing.

**What "rejected" costs you.** A rejected name is refused when the permission policy is built, which is not the same moment on both surfaces. `agens chat` builds it once for the turn it was asked to run, so the command fails and prints the error. The TUI builds it per submission, so agens starts, draws its interface, and then fails *every prompt you send* with the same error until the rule is fixed. In both cases nothing runs under a rule agens could not resolve, and the error names the rule it refused and what could not be resolved about it.

**Approving a remote call approves all of them.** A permission prompt for a remote tool shows the tool's name and nothing else, because the tool's own name is the only target agens can project — the arguments belong to the server. Answering **Allow always** therefore stores a grant matching *every future call to that tool*, for as long as the grant lives. That is the mirror image of the deny side, and it is unique to this class: an `Allow always` for `bash` is remembered per command text, for `task` per subagent, for `skill` per skill name. If you want a remote tool authorized for some calls and not others, there is nothing to scope it by — grant it once per call, or `deny` it.

| Tool | Returns file contents | How a path deny reaches it |
|------|-----------------------|----------------------------|
| `read` | yes | the target is the file |
| `list` | no — names only | the target is the directory |
| `glob` | no — names only | not as a path; its target is the pattern, matched as text |
| `search` | yes | the target is the root it is given, and again per file |
| `grep` | yes | the target is its pattern and the root it is given, and again per file |
| `git_read` | yes, `diff` only | per file; the target is the operation keyword |
| `write` | no | the target is the file |
| `edit` | the region it rewrote | the target is the file |
| `webfetch` | no — `http`/`https` responses | not as a path; its target is the URL |
| `bash` | whatever it chooses to print | **it does not** — the target is the command line |
| `skill` (primary only) | yes — a skill's own files | not by its target, which is the skill name; by the file it opens, which a global skill has only when the global skills directory sits under the session's root |
| `task` (primary only) | what the subagent reports | **not here** — its target is the subagent's name; the subagent read under these same rules |
| `task_control`, `task_message` | no — execution state only | not as a path; they never address a file |
| `ask_user` (primary only) | no — only the answer given at the terminal | not as a path; its target is the tool's own name and no call to it addresses a file |
| `cd` (primary only) | no — only the directory it moved to | the target is the directory |
| `worktree` (primary only) | no — only the worktree it created | not as a path; its target is the worktree's name |
| `<server>::<tool>` from MCP (primary only) | whatever that server returns | **it does not** — the target is the tool's own name, and its arguments belong to the server |

The remaining limit is over names rather than contents: `glob`'s pattern denotes a set while a rule is matched as text, so `deny glob(**/.env)` does not stop `glob(**)` from listing that name — it discloses a name, which `list(**)` already discloses, not what the file holds.

**Bounding a call that reads files it never named.** Such a call still runs. It returns everything it is allowed to return, omits the denied files, and ends with one line saying some files were not read. That line carries no path, no count and no root, so a single call discloses only that something under what it reached was withheld. It does not make the withheld file unfindable: a caller free to re-scope the call narrows it down — by searching from a narrower root, down to a directory, or by diffing a narrower revision range, down to a single commit. A call that names the denied file directly is refused outright instead, because the rule denies the whole of what that call asked for. This is why `deny git_read(**/.env)` is written against a path while `deny git_read(diff)` is written against an operation: the first decides the files a diff reports, the second decides whether the diff runs at all.

The notice answers for every file the call walked, not only for the files it reported. `grep`'s own `glob` argument narrows what comes back and never what the rules were asked about, so `grep(pattern, glob="*.rs")` in a tree holding a denied `.env` still says a file was withheld — the `.rs` results are complete even so. That order is deliberate: deciding after the caller's filter would make the notice exact and would also turn the filter into a way of narrowing the withheld set one filename at a time, which is the limit above.

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

`bash` is not. It is permission-gated and time-bounded, and it starts in the project root, but a granted command can still reach paths outside it. The `[permissions]` rules are the only bound on what it runs, and they are pattern matching over the command line rather than containment: they see through chained commands, wrappers such as `sudo`, and `/bin/rm` versus `rm`, but a command written to obscure which binary it invokes can still evade them. Grant `bash` deliberately, and treat any `deny` rule on a path as unenforced against it. True isolation is the layer you run Agens in — a container or VM you own; any future `bash` confinement would bound the blast radius of a confused command, not prevent a granted one from reading and sending what your account can reach.

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

Repository tasks are devenv scripts declared in `devenv.nix`. Entering the development shell, through direnv or manually, puts each one on `PATH`:

```sh
fmt-check    # check rustfmt without modifying files
lint         # Clippy for all workspace targets with warnings denied
check         # workspace tests
build        # build the workspace
contracts    # repository bootstrap and standards contracts
deny         # dependency advisories, licenses, bans, and sources
verify       # canonical complete gate
clean        # manual build-output cleanup
```

Build outputs are `target/{debug,release}/agens`. The directory has a 50 GiB budget. Verification checks the budget and never cleans automatically; cleanup is manual only with `clean`.

Before considering a change complete, run:

```sh
nix develop --no-pure-eval -c verify
```

## Known limitations

- `agens models` is reserved in the command surface but currently reports that the capability is unavailable.
- The production tool catalog wires native tools, configured MCP tools, and the `skill`, `task` and `ask_user` tools. A delegated subagent reaches native tools only; MCP tools are not passed through to it.
- TUI model and reasoning-effort palettes are not implemented; use configuration or `agens chat --model` for model selection.
- Packaging, release automation, and editor protocol integrations are not provided.

## Documentation

- `ARCHITECTURE.md`: crate boundaries and runtime dependency direction, including how global and project `AGENTS.md` instructions are discovered and appended to every agent's prompt.
- `AGENTS.md`: concise execution rules for coding agents.
- `CODE_STYLE.md`: Rust engineering, lint, security, and verification standards.
- `CONTRIBUTING.md`: setup, TDD, review, dependency, and security workflow.
- `CLAUDE.md`: thin pointer to the canonical documents.

# AGENTS.md — Agens

Guidelines for AI agents and contributors working in this Rust codebase.

## Project overview

Agens is a Rust CLI coding agent with providers, tools, MCP, skills, sub-agents, persistence, and a TUI.

Use these root documents as canonical references:

- `ARCHITECTURE.md` — package boundaries and dependency direction.
- `CODE_STYLE.md` — Rust style, linting, testing, and error-handling conventions.
- `CONTRIBUTING.md` — local setup and verification workflow.

## Environment

This project uses devenv through the Nix development shell. Prefer:

```sh
nix develop --no-pure-eval -c just verify
```

If you enter the shell interactively, use:

```sh
nix develop --no-pure-eval
```

Rust is the only implementation. Binaries are written to `target/{debug,release}/agens`.

## Canonical commands

| Task | Command |
|------|---------|
| Format | `just fmt` |
| Format check | `just fmt-check` |
| Lint | `just lint` |
| Test | `just test` |
| Build | `just build` |
| Config diagnostics | `just build && ./target/debug/agens config doctor` |
| Verification | `just verify` |
| Clean build output manually | `just clean` |

The Rust `target/` budget is 20 GiB. Gates never clean it automatically; cleanup is manual only with `just clean`.

## Working principles

- Prefer small, boring, explicit code.
- Do not invent APIs or behavior; read existing code and docs first.
- Keep `crates/agens-cli` as the composition root; domain crates must not depend on it.
- Keep `agens-tui` as a surface adapter over the shared runtime.
- Keep hand-authored TOML configuration separate from SQLite-backed runtime state.
- Keep comments rare and focused on non-obvious why.
- Never log or print secrets.

## Strict TDD

For every non-trivial production change:

1. Write the failing test first.
2. Capture the red result in the apply artifact.
3. Implement the smallest change.
4. Capture the green result.
5. Run `nix develop --no-pure-eval -c just verify` before marking complete.

Tests should live near the package they cover and prefer table-driven cases when behavior branches.

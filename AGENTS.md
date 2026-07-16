# AGENTS.md — Agens

Guidelines for AI agents and contributors working in this Go codebase.

## Project overview

Agens is a Go CLI coding agent. The codebase starts with a headless, testable core and will later grow into providers, tools, MCP, skills, sub-agents, persistence, and a TUI.

Use these root documents as canonical references:

- `ARCHITECTURE.md` — package boundaries and dependency direction.
- `CODE_STYLE.md` — Go style, linting, testing, and error-handling conventions.
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

Go remains official at `./agens`; the build-only Rust binary stays at `target/{debug,release}/agens`.

## Canonical commands

| Task | Command |
|------|---------|
| Format | `just fmt` |
| Format check | `just fmt-check` |
| Lint | `just lint` |
| Test | `just test` |
| Build | `just build` |
| Config diagnostics | `just build && ./agens config doctor` |
| Default/Go gate | `just verify` / `just verify-go` |
| Rust gate | `just verify-rust` |
| Go then Rust | `just verify-dual` |
| Clean Go output | `just clean` |
| Clean Rust output manually | `just target-clean` |

The Rust `target/` budget is 20 GiB. Gates never clean it automatically; cleanup is manual only with `just target-clean`.

## Working principles

- Prefer small, boring, explicit code.
- Do not invent APIs or behavior; read existing code and docs first.
- Keep `cmd/agens` thin. It only adapts process exit behavior to `internal/app`.
- Keep Cobra details in `internal/cli`; future domain logic must not depend on Cobra types.
- Do not add `pkg/` until Agens has a real external Go API.
- Do not add providers, Codex integration, TUI, packaging, or release automation as part of foundation tasks unless their SDD scope explicitly includes them.
- Keep hand-authored TOML config separate from future SQLite-backed runtime state.
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

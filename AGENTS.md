# AGENTS.md - Agens

Agens is a Rust coding-agent CLI. This file is the canonical execution guide for coding agents; detailed engineering rules live in `CODE_STYLE.md`, architecture in `ARCHITECTURE.md`, and contributor workflow in `CONTRIBUTING.md`.

## Ground rules

- Read the relevant code, tests, manifests, and canonical documents before changing behavior.
- Do not invent APIs, flags, dependency behavior, or implementation status. Separate facts from inferences and state uncertainty.
- Make the smallest complete change. Do not reformat, rename, or clean unrelated code.
- Preserve user work and dirty files. Never discard changes that were not created for the active task.
- Keep comments rare and limited to non-obvious constraints, invariants, or safety reasoning.
- Never print, log, commit, or expose credentials, tokens, authorization headers, or secret-bearing command output.
- Propagate or handle failures explicitly. Do not silently discard fallible results.

## Environment and commands

Rust is the only implementation. Use the Nix development shell and the root `justfile` command surface:

```sh
nix develop --no-pure-eval
```

| Task | Command |
|------|---------|
| Format check | `just fmt-check` |
| Lint | `just lint` |
| Test | `just test` |
| Build | `just build` |
| Bootstrap contracts | `just contracts` |
| Supply-chain policy | `just deny` |
| Full gate | `just verify` |
| Manual cleanup | `just clean` |

The canonical completion gate is:

```sh
nix develop --no-pure-eval -c just verify
```

Build outputs are `target/{debug,release}/agens`. The `target/` budget is 20 GiB. Verification checks the budget before and after the gate and never cleans automatically; cleanup is manual only with `just clean`.

## Strict TDD

For every non-trivial production or contract change:

1. Add or update the focused test or contract first.
2. Run it and record the expected failure.
3. Implement the smallest change that makes it pass.
4. Re-run the focused test and relevant crate checks.
5. Run the canonical full gate before declaring the change complete.

Do not weaken assertions, lint levels, or error handling to manufacture a green result.

## Architecture boundaries

- `agens-cli` is the composition root and sole binary crate.
- `agens-core` owns domain contracts and must not depend on adapters.
- `agens-tui` is a terminal surface over the shared runtime, not a second runtime.
- Providers, tools, stores, and configuration remain adapters around core contracts.
- Hand-authored TOML configuration remains separate from SQLite runtime state.
- Preserve the dependency graph documented in `ARCHITECTURE.md` and enforced by `tests/bootstrap/assert-workspace.sh`.

## Change safety

- Validate external input at filesystem, process, network, provider, MCP, and persistence boundaries.
- Preserve cancellation and deadline propagation across blocking and async work.
- Keep unsafe code confined to audited operating-system boundaries with explicit invariants; see `CODE_STYLE.md`.
- Use check-only formatting during scoped or dirty-worktree work. Do not run a mutating workspace formatter unless all affected files are intentionally in scope.

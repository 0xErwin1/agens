# Code Style

Agens favors strict, boring, maintainable Rust. Keep changes small and behavior explicit.

## Naming

| Element | Convention | Example |
|---------|------------|---------|
| Crates | lowercase kebab-case | `agens-cli`, `agens-core` |
| Types and traits | PascalCase | `TurnEvent` |
| Functions and modules | snake_case | `run_headless_chat` |
| Constants | UPPER_SNAKE_CASE | `MAX_OUTPUT_BYTES` |
| Tests | descriptive snake_case | `resume_uses_saved_session_context` |

Use full words when clarity matters. Avoid clever abbreviations.

## File organization

- Keep `crates/agens-cli` as the composition root and binary surface.
- Keep crate APIs small and explicit.
- Preserve the dependency direction in `ARCHITECTURE.md`.
- Prefer adding code to the crate that owns the behavior instead of creating parallel structures.

## Formatting and linting

Formatting is mandatory:

```sh
just fmt
just fmt-check
```

Linting is mandatory:

```sh
just lint
```

`just verify` runs the Rust target budget, format check, Clippy, tests, and build. Binaries stay at `target/{debug,release}/agens`.

Enter devenv with `nix develop --no-pure-eval`. Rust `target/` has a 20 GiB limit; verification never cleans it, and cleanup is manual only with `just clean`.

## Error handling

- Return typed errors; do not panic for normal failures.
- Add context at adapter boundaries where it helps users act.
- Do not silently discard errors.
- Avoid global mutable state.

## Comments

Default to no comments. Add a comment only when it explains a non-obvious constraint, invariant, or tradeoff.

## Testing

Use strict TDD for non-trivial changes:

1. Write a failing test.
2. See it fail.
3. Implement the smallest change.
4. See it pass.
5. Run `just verify`.

Testing conventions:

- Tests live beside the crate or integration boundary under test.
- Prefer table-driven tests when behavior has multiple cases.
- Test public package contracts and important boundary behavior.
- Keep network behavior behind local protocol doubles unless a task explicitly requires a live boundary.

## Architecture guardrails

- `agens-core` does not depend on adapter crates.
- `agens-tui` remains a surface adapter over the shared runtime.
- Configuration format and credential-path compatibility belong in `agens-config`.
- Cross-compilation and release automation remain out of scope.

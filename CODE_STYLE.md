# Code Style

Agens favors strict, boring, maintainable Go. Keep changes small and behavior explicit.

## Naming

| Element | Convention | Example |
|---------|------------|---------|
| Packages | short lowercase words | `cli`, `config` |
| Exported types/functions | PascalCase | `NewRootCommand` |
| Unexported names | camelCase | `configHome` |
| Constants | PascalCase or camelCase by visibility | `AppName` |
| Tests | `Test` + behavior | `TestRootCommandHelp` |

Use full words when clarity matters. Avoid clever abbreviations.

## File organization

- Keep `cmd/agens` thin.
- Keep Cobra-specific code in `internal/cli`.
- Keep package APIs small and explicit.
- Do not create `pkg/` until Agens exposes a real external Go API.
- Prefer adding code to the package that owns the behavior instead of creating parallel structures.

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

`just verify` includes format check, `golangci-lint`, tests, and local build.

## Error handling

- Return `error`; do not panic for normal failures.
- Wrap errors at boundaries where context helps.
- Do not silently discard errors.
- Avoid global mutable state except for build metadata seams such as `internal/version`.

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

- Tests live beside the package under test.
- Prefer table-driven tests when behavior has multiple cases.
- Test public package contracts and important boundary behavior.
- Do not add integration or network tests in `AGN-1`.

## Architecture guardrails

- Future domain logic must not depend on Cobra.
- Provider-specific logic does not belong in `AGN-1`.
- Deep configuration loading belongs to `AGN-2`.
- Cross-compilation and release automation are out of scope for `AGN-1`.

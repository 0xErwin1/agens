# Contributing

## Setup

Agens is Nix-first:

```sh
nix develop
```

With direnv enabled:

```sh
direnv allow
```

## Workflow

Use `just` for all routine commands:

```sh
just fmt
just lint
just test
just build
just verify
```

Before a change is considered complete, `just verify` must pass.

## TDD requirement

For non-trivial code changes, follow and record the red/green loop:

1. Add or update a failing test.
2. Record the failure.
3. Implement the smallest change.
4. Record the passing test.
5. Run `nix develop -c just verify`.

## Scope discipline

Keep changes scoped to the active Atlas task and SDD artifacts. For `AGN-1`, do not add providers, ChatGPT/Codex integration, TUI, cross-compilation, packaging, or release automation.

## Documentation

Update root docs when commands, boundaries, or conventions change. Keep docs factual and current with the code.

## Commits

Use small work-unit commits. Keep tests and implementation together once the red/green evidence has been recorded in the apply artifact.

# Contributing

## Setup

Agens uses devenv through its Nix development shell:

```sh
nix develop --no-pure-eval
```

Rust is the only product implementation. Development and release binaries are written to `target/{debug,release}/agens`.

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
just clean
```

`just verify` runs the Rust budget, format, lint, test, and build gate. Before a change is considered complete, this gate must pass.

Rust `target/` is limited to 20 GiB. Verification never deletes build output; cleanup is manual only with `just clean`.

## TDD requirement

For non-trivial code changes, follow and record the red/green loop:

1. Add or update a failing test.
2. Record the failure.
3. Implement the smallest change.
4. Record the passing test.
5. Run `nix develop --no-pure-eval -c just verify`.

## Scope discipline

Keep changes scoped to the active Atlas task and SDD artifacts. For `AGN-1`, do not add providers, ChatGPT/Codex integration, TUI, cross-compilation, packaging, or release automation.

## Documentation

Update root docs when commands, boundaries, or conventions change. Keep docs factual and current with the code.

## Commits

Use small work-unit commits. Keep tests and implementation together once the red/green evidence has been recorded in the apply artifact.

# Contributing

Agens is a Rust workspace with a Nix-first development environment. Keep contributions focused, test-driven, reviewable, and grounded in implemented behavior.

## Setup

The development shell is entered by direnv:

```sh
direnv allow
```

Without direnv, enter the same shell manually:

```sh
nix develop --no-pure-eval
```

Rust 1.97.1, `cargo-deny`, Clippy, rustfmt, and rust-analyzer are provided by the shell. Repository tasks are devenv scripts declared in `devenv.nix`, so entering the shell puts each one on `PATH` under its own name and prints the list. Tests are `check` rather than `test`, because a command named `test` would be shadowed by the shell builtin.

Build and inspect the CLI:

```sh
build
./target/debug/agens --help
```

Development and release binaries are written to `target/{debug,release}/agens`.

## Branch and worktree hygiene

- Start from the intended base revision and keep one concern per branch or worktree.
- Inspect `git status` before editing and before reporting completion.
- Do not overwrite, restore, format, or stage unrelated user changes.
- Keep generated output and personal working notes out of the repository.
- Rebase or merge only when the chosen collaboration flow requires it; Agens does not impose an inherited branch identity policy.

## Strict TDD workflow

For non-trivial production or contract changes:

1. Add or update the focused test first.
2. Run it and capture the expected failure.
3. Implement the smallest complete fix or feature.
4. Re-run the focused test and affected crate checks.
5. Run the final gate.

Use the verification matrix in `CODE_STYLE.md` to select focused commands. Repository contracts run with:

```sh
contracts
```

The canonical final gate is:

```sh
nix develop --no-pure-eval -c verify
```

It checks the target budget, bootstrap contracts, formatting, Clippy, tests, build, supply-chain policy, and the target budget again.

## Build-output budget

The Rust `target/` directory has a 50 GiB budget. Verification reports an overage and never removes artifacts automatically. Cleanup is a deliberate manual action:

```sh
clean
```

## Commits and review units

Use Conventional Commits:

```text
<type>(<optional-scope>): <imperative summary>
```

Common types are `feat`, `fix`, `refactor`, `perf`, `docs`, `test`, `ci`, and `chore`.

- Keep one coherent work unit per commit.
- Keep a behavior change and its tests together after red/green evidence is captured.
- Explain non-obvious motivation and tradeoffs in the commit body.
- Avoid mixing dependency updates, formatting churn, refactors, and user-visible behavior unless they are inseparable.
- Treat roughly 400 changed lines as a review-load signal. Split larger changes into ordered review units when a clean boundary exists; generated files and lockfiles should be identified separately rather than used to hide authored size.

## Pull requests

Pull requests must make claims that reviewers can verify. Use `.github/pull_request_template.md` and include:

- The problem and user-visible outcome.
- The implementation approach and relevant tradeoffs.
- Exact focused and final validation commands with results.
- Red/green evidence for non-trivial behavior changes.
- Known limitations, untested boundaries, and deliberate exclusions.

Do not check a validation item based on expectation. If a platform, provider, live network, or full gate was not exercised, say so explicitly.

## Running the daemon

`agens serve` runs the headless daemon for the machine. Two operator decisions gate it, and both are closed by default.

Name the checkouts it may create runs against:

```toml
[team]
project_roots = ["/home/dev/checkouts"]
```

A root admits itself and everything under it, compared as whole path components. With none configured, every `CreateRun` is refused and the refusal names this key and the file it belongs in. The daemon reads the roots at start, so a new one takes effect on the next `agens serve`.

Provisioning hooks are repository code run with the daemon's whole environment, so they run only for a repository somebody decided about. The first run of an undecided repository proceeds with its hooks unrun and opens a durable question; answering it decides that repository for good. To decide ahead of the first run:

```sh
agens serve trust /home/dev/checkouts/agens
```

That writes the control plane the daemon reads, so it works whether or not a daemon is running and needs no restart. The checkout has to be one `team.project_roots` already serves.

`team.hook_exports` lists the environment names a hook may export into the hooks after it. It is empty by default, which is what a repository that never asked for an export expects.

## Documentation ownership

- `README.md` is the current product and development portal.
- `ARCHITECTURE.md` owns crate roles, runtime boundaries, and dependency direction.
- `AGENTS.md` owns concise agent execution rules.
- `CODE_STYLE.md` owns detailed Rust and verification standards.
- `CONTRIBUTING.md` owns setup and collaboration workflow.
- `CLAUDE.md` remains a thin pointer and must not duplicate policy.

Update the owning document in the same work unit when commands, boundaries, behavior, or standards change. Keep documentation factual and remove stale claims rather than labeling implementation plans as current features.

## Dependency changes

Dependency changes must be intentional and minimal:

- Explain why the standard library or an existing dependency is insufficient.
- Review default features, transitive size, maintenance status, license, advisory history, and platform impact.
- Keep `Cargo.lock` synchronized and use `--locked` in verification.
- Run focused tests, `lint`, and `deny`.
- Do not introduce build profiles, linkers, installers, containers, or release tooling without a measured project need.

Dependabot groups routine monthly minor and patch updates. Major upgrades remain deliberate review work.

## Security reporting

Do not publish suspected vulnerabilities, credentials, tokens, authorization headers, or secret-bearing logs in a public issue. Use GitHub private vulnerability reporting if it is enabled for the repository, or an existing private maintainer channel. Public bug reports must contain redacted, minimal reproduction evidence.

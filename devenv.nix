# Development shell and repository tasks, imported by flake.nix.
{ pkgs }:

{
  packages = [
    pkgs.cargo-deny
    pkgs.jq
    pkgs.mold
  ];

  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  # Repository tasks. direnv loads this shell, so every name below is
  # callable directly inside the repository. Tests are `check` rather
  # than `test` because a script named `test` is shadowed by the bash
  # builtin and could never be invoked by name.
  scripts = {
    fmt = {
      description = "rewrite Rust sources with rustfmt";
      exec = "cargo fmt --all";
    };

    fmt-check = {
      description = "check rustfmt without modifying files";
      exec = "cargo fmt --all -- --check";
    };

    lint = {
      description = "Clippy for all workspace targets with warnings denied";
      exec = "cargo clippy --workspace --all-targets --locked -- -D warnings";
    };

    check = {
      description = "workspace tests";
      exec = "cargo test --workspace --all-targets --locked --no-fail-fast";
    };

    build = {
      description = "build the workspace";
      exec = "cargo build --workspace --locked";
    };

    deny = {
      description = "dependency advisories, licenses, bans, and sources";
      exec = "cargo deny check";
    };

    contracts = {
      description = "repository bootstrap and standards contracts";
      exec = ''
        set -euo pipefail
        cd "$DEVENV_ROOT"
        tests/bootstrap/assert-workspace.sh
        tests/bootstrap/assert-surface-boundary.sh
        tests/bootstrap/docs-contract.sh
        tests/bootstrap/target-budget.sh
        tests/bootstrap/verify-contracts.sh
        tests/bootstrap/standards-contract.sh
        tests/bootstrap/perf-offpath.sh
        tests/bootstrap/discarded-results-contract.sh
      '';
    };

    verify = {
      description = "canonical complete gate";
      exec = ''
        set -euo pipefail
        target-budget
        contracts
        fmt-check
        lint
        check
        build
        deny
        target-budget
      '';
    };

    target-size = {
      description = "report the size of the Rust target directory";
      exec = ''
        set -euo pipefail
        target_dir="''${CARGO_TARGET_DIR:-$DEVENV_ROOT/target}"
        if [[ -d "$target_dir" ]]; then
            du --apparent-size --bytes --summarize "$target_dir"
        else
            printf '0\t%s\n' "$target_dir"
        fi
      '';
    };

    target-budget = {
      description = "fail when the target directory exceeds its budget";
      exec = ''exec "$DEVENV_ROOT/scripts/check-target-budget.sh"'';
    };

    target-clean = {
      description = "manual build-output cleanup";
      exec = "cargo clean";
    };

    clean = {
      description = "manual build-output cleanup";
      exec = "cargo clean";
    };

    perf-audit = {
      description = "record a TUI render trace (optional run id)";
      exec = ''exec "$DEVENV_ROOT/scripts/perf-audit.sh" "''${1-}"'';
    };

    perf-diff = {
      description = "compare two render traces: perf-diff <base> <new>";
      exec = ''
        set -euo pipefail
        if [[ $# -ne 2 ]]; then
            echo "perf-diff needs a base trace and a new trace" >&2
            exit 2
        fi
        cargo run -p agens-tui --features perf-audit --bin agens-perf-audit --locked -- \
            diff "$1" "$2"
      '';
    };

    perf-baseline = {
      description = "regenerate the committed render baseline";
      exec = ''exec "$DEVENV_ROOT/scripts/perf-regen-baseline.sh"'';
    };
  };

  # The devenv CLI reachable from a flake integration is a wrapper without an
  # `info` command, so the task list is spelled out rather than delegated to it.
  enterShell = ''
    echo "Agens dev shell — tasks: verify, contracts, fmt, fmt-check, lint, check, build, deny, clean, target-*, perf-*"
  '';
}

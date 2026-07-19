#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$repo_root"

for file in \
    CLAUDE.md \
    rustfmt.toml \
    deny.toml \
    .gitattributes \
    .github/workflows/verify.yml \
    .github/dependabot.yml \
    .github/pull_request_template.md \
    .github/ISSUE_TEMPLATE/bug_report.yml \
    .github/ISSUE_TEMPLATE/feature_request.yml; do
    test -f "$file"
done

grep -Fx 'edition = "2024"' rustfmt.toml >/dev/null
grep -Fx 'max_width = 100' rustfmt.toml >/dev/null
grep -Fx 'tab_spaces = 4' rustfmt.toml >/dev/null
grep -Fx 'newline_style = "Unix"' rustfmt.toml >/dev/null

grep -F '[workspace.lints.rust]' Cargo.toml >/dev/null
grep -F '[workspace.lints.clippy]' Cargo.toml >/dev/null
grep -F 'publish = false' Cargo.toml >/dev/null

for manifest in crates/*/Cargo.toml; do
    grep -F '[lints]' "$manifest" >/dev/null
    grep -F 'workspace = true' "$manifest" >/dev/null
    grep -F 'publish.workspace = true' "$manifest" >/dev/null
done

grep -F 'pkgs.cargo-deny' flake.nix >/dev/null
grep -F 'cargo deny check' justfile >/dev/null
grep -F 'just contracts' justfile >/dev/null
grep -F 'tests/bootstrap/standards-contract.sh' justfile >/dev/null

grep -F '* text=auto eol=lf' .gitattributes >/dev/null
grep -F 'nix develop --no-pure-eval -c just verify' .github/workflows/verify.yml >/dev/null
if grep -Eq 'uses: [^@[:space:]]+@v[0-9]' .github/workflows/verify.yml; then
    echo "GitHub Actions must be pinned to exact commits" >&2
    exit 1
fi

if grep -Fq 'AGN-1' CONTRIBUTING.md; then
    echo "CONTRIBUTING.md contains obsolete AGN-1 scope" >&2
    exit 1
fi

for doc in CLAUDE.md AGENTS.md CODE_STYLE.md CONTRIBUTING.md README.md; do
    if grep -Eqi '\bgo(lang)?\b|gofmt|goimports|golangci|verify-(go|rust|dual)' "$doc"; then
        echo "$doc contains an active legacy runtime reference" >&2
        exit 1
    fi
done

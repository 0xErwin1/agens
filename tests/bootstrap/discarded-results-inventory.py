#!/usr/bin/env python3
"""List production `let _ =` discards, skipping tests and comments.

Default dump prints `path<TAB>stripped_line`. `--check` compares that
inventory to a classified allowlist. `--generate` writes a classified
allowlist for bootstrap only; the contract never auto-classifies.
"""

from __future__ import annotations

import argparse
import collections
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
CRATES = ROOT / "crates"
SKIP_DIR_NAMES = {"tests", "benches", "examples"}
SKIP_FILE_NAMES = {"test_support.rs", "fake-mcp-child.rs"}
LET_DISCARD = re.compile(r"\blet\s+_\s*=")
TEST_CFG = re.compile(r"^#\[cfg\(\s*test\b")

CLASSES = (
    "secondary_cleanup",
    "closed_channel",
    "best_effort_observability",
    "documented_no_op",
    "taxonomy_collapse",
)

# First-line fragments of multiline discards the scanner cannot classify
# from the stripped line alone.
HARDCODED = {
    ("crates/agens-tui/src/bridge.rs", "let _ = parked"): "closed_channel",
    ("crates/agens-tui/src/conversation.rs", "let _ = conversation"): "closed_channel",
    ("crates/agens-tui/src/conversation.rs", "let _ ="): "closed_channel",
    ("crates/agens-tui/src/lib.rs", "let _ = self"): "documented_no_op",
    ("crates/agens-tui-app/src/engine.rs", "let _ = task_controls"): "documented_no_op",
}

ALLOWLIST_HEADER = """\
# Classified production `let _ =` discards.
#
# Classes:
#   secondary_cleanup         cleanup after the primary result is already decided
#   closed_channel            send/publish/reply when the consumer may be gone
#   best_effort_observability stdio / browser UX that must not change the primary result
#   documented_no_op          intentional unused / keep-alive / UI no-op
#   taxonomy_collapse         fire-and-forget MCP discovery refresh (not a Result)
#
# New production `let _ =` must be classified here with one of those classes,
# or the discard must be removed or replaced with best_effort / a surfaced error.
# Format: path<TAB>stripped_line<TAB>class
"""


def is_production_source(path: pathlib.Path) -> bool:
    if path.suffix != ".rs":
        return False
    if path.name.endswith("_tests.rs") or path.name == "tests.rs" or path.name in SKIP_FILE_NAMES:
        return False
    if any(part in SKIP_DIR_NAMES for part in path.parts):
        return False
    return "src" in path.parts


def production_lines(text: str) -> list[str]:
    """Drop `#[cfg(test)]` items so crate-local tests do not enter the inventory."""

    lines: list[str] = []
    skip_depth = 0
    pending_test_item = False

    for raw in text.splitlines():
        stripped = raw.strip()

        if skip_depth > 0:
            skip_depth += raw.count("{") - raw.count("}")
            continue

        if TEST_CFG.search(stripped):
            pending_test_item = True
            continue

        if pending_test_item:
            if not stripped or stripped.startswith("//") or stripped.startswith("#"):
                continue
            opens = raw.count("{")
            closes = raw.count("}")
            if stripped.endswith(",") and not stripped.startswith(
                ("fn ", "pub fn ", "mod ", "impl ", "struct ", "enum ")
            ):
                pending_test_item = False
                continue
            if opens == 0 and not stripped.endswith(";"):
                continue
            pending_test_item = False
            skip_depth = max(0, opens - closes)
            continue

        lines.append(raw)

    return lines


def inventory() -> list[tuple[str, str]]:
    found: list[tuple[str, str]] = []

    for path in sorted(CRATES.rglob("*.rs")):
        if not is_production_source(path):
            continue

        relative = path.relative_to(ROOT).as_posix()
        text = path.read_text(encoding="utf-8")

        for raw in production_lines(text):
            stripped = raw.strip()
            if stripped.startswith("//"):
                continue
            if not LET_DISCARD.search(stripped):
                continue
            found.append((relative, stripped))

    return found


def classify(path: str, stripped: str) -> str | None:
    hardcoded = HARDCODED.get((path, stripped))
    if hardcoded is not None:
        return hardcoded

    if "discover_server(" in stripped:
        return "taxonomy_collapse"

    if any(
        token in stripped
        for token in (
            "stderr().flush",
            "stdout().flush",
            "writeln!(",
            "write!(",
            "stream.flush()",
            "open_browser",
        )
    ):
        return "best_effort_observability"

    if any(
        token in stripped
        for token in (
            "&self.lock",
            "let _ = relative;",
            "wait_timeout",
            "scheduler.reduce",
            "remove_queue_entry",
            "handle_key",
            "begin_turn",
            "apply_submission_outcome",
            "apply_busy_submission_outcome",
            "apply_unverified_model",
            "apply_reasoning_effort",
            "send_task_message",
            "task_controls",
            "transition(",
        )
    ):
        return "documented_no_op"

    if any(
        token in stripped
        for token in (
            "events.publish",
            "bridge.publish",
            "progress.send",
            "sender.send",
            "ready.send",
            "response.send",
            "completion_sender.send",
            "route_sender.send",
            "parked.sender.send",
            "request.response.send",
            "permission_bridge.reply",
            "bridge.reply",
            "self.reply",
            "conversation.apply",
            ".result.send",
        )
    ):
        return "closed_channel"

    if any(
        token in stripped
        for token in (
            "remove_file",
            "remove_dir_all",
            "prune_orphans",
            "child.kill",
            "child.wait",
            "terminate_process_group",
            "wait_for_readers",
            "transport.close",
            ".terminate()",
            "self.close()",
            "handle.join",
            ".join()",
            "stash_pop",
            "stash_remove_at",
            "sync_all",
            "guard.restore",
            "self.restore()",
            "TuiPermissionBridge::close",
            "TuiAskUserBridge::close",
            "coordinator.fail()",
        )
    ):
        return "secondary_cleanup"

    return None


def load_allowlist(path: pathlib.Path) -> list[tuple[str, str, str]]:
    if not path.is_file():
        raise SystemExit(f"allowlist not found: {path}")

    rows: list[tuple[str, str, str]] = []

    for lineno, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        stripped = raw.strip()
        if not stripped or stripped.startswith("#"):
            continue

        parts = stripped.split("\t")
        if len(parts) != 3:
            raise SystemExit(f"{path}:{lineno}: expected path<TAB>stripped_line<TAB>class")

        file_path, line, class_name = parts
        if not file_path or not line:
            raise SystemExit(f"{path}:{lineno}: empty path or stripped_line")
        if class_name not in CLASSES:
            raise SystemExit(f"{path}:{lineno}: unknown class {class_name!r}")

        rows.append((file_path, line, class_name))

    return rows


def _emit_counter(
    label: str,
    extras: collections.Counter[tuple[str, str]],
    class_by_key: dict[tuple[str, str], str] | None = None,
) -> None:
    if not extras:
        return

    print(label, file=sys.stderr)
    for key, count in extras.items():
        path, line = key
        suffix = f"\t{class_by_key[key]}" if class_by_key is not None else ""
        for _ in range(count):
            print(f"  {path}\t{line}{suffix}", file=sys.stderr)


def check(allowlist_path: pathlib.Path) -> int:
    allowlist = load_allowlist(allowlist_path)
    live = inventory()

    live_keys = collections.Counter(live)
    allow_keys = collections.Counter((path, line) for path, line, _ in allowlist)
    class_by_key: dict[tuple[str, str], set[str]] = collections.defaultdict(set)
    for path, line, class_name in allowlist:
        class_by_key[(path, line)].add(class_name)

    inconsistent = {key: classes for key, classes in class_by_key.items() if len(classes) > 1}
    unclassified = live_keys - allow_keys
    stale = allow_keys - live_keys

    if inconsistent:
        print("allowlist rows share a site but disagree on class:", file=sys.stderr)
        for (path, line), classes in inconsistent.items():
            joined = ", ".join(sorted(classes))
            print(f"  {path}\t{line}\t{joined}", file=sys.stderr)

    _emit_counter("unclassified production let _ = discards:", unclassified)
    _emit_counter(
        "stale allowlist rows:",
        stale,
        {key: next(iter(classes)) for key, classes in class_by_key.items()},
    )

    if unclassified or stale or inconsistent:
        return 1
    return 0


def generate(allowlist_path: pathlib.Path) -> int:
    live = inventory()
    rows: list[tuple[str, str, str]] = []
    missing: list[tuple[str, str]] = []

    for path, line in live:
        class_name = classify(path, line)
        if class_name is None:
            missing.append((path, line))
            continue
        rows.append((path, line, class_name))

    if missing:
        print("unclassified production let _ = discards:", file=sys.stderr)
        for path, line in missing:
            print(f"  {path}\t{line}", file=sys.stderr)
        return 1

    body = ALLOWLIST_HEADER + "".join(f"{path}\t{line}\t{class_name}\n" for path, line, class_name in rows)
    allowlist_path.write_text(body, encoding="utf-8")
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--check", metavar="ALLOWLIST", type=pathlib.Path)
    group.add_argument("--generate", metavar="ALLOWLIST", type=pathlib.Path)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)

    if args.check is not None:
        return check(args.check)
    if args.generate is not None:
        return generate(args.generate)

    for path, line in inventory():
        sys.stdout.write(f"{path}\t{line}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

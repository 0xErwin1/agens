# The baseline trace

`run.jsonl` is a recorded run of the performance audit, committed so that a later run has something to be compared against. It is a reference point, not a threshold: nothing fails because of it.

## Quick path

```bash
just perf-audit                                             # writes target/perf/<run-id>/run.jsonl
just perf-diff tests/perf/baseline/run.jsonl target/perf/<run-id>/run.jsonl
```

Read the deterministic section first. If it is empty, the render pipeline did the same work in the same shape as the baseline, and nothing further is owed.

## What it is authoritative for

| Signal | Authoritative | Why |
|---|---|---|
| Span shape — which spans exist and how they nest | Yes | Derived from control flow, identical on any machine |
| Call count per span identity | Yes | Same reason. A cache that stopped working shows up here as a count that grew |
| Wall-clock duration | **No** | Depends on the machine, its load at the time, and the build profile |

The diff enforces this split: durations only ever appear in a section labelled advisory, and a duration difference alone is never reported as a regression.

## What it cannot tell you

- **Whether the product got slower on your machine.** Compare two runs from the *same* machine for that. Baseline timings came from whichever machine last regenerated it.
- **Whether the shipped binary behaves this way.** The audit builds with `--features perf-audit`, which the release build never enables, and it drives a `TestBackend` rather than a real terminal — no crossterm write syscalls, no tty latency.

## Regenerating

```bash
just perf-baseline
```

It refuses to run on a dirty worktree: a trace records the commit it ran at, and one captured from uncommitted changes would name a commit that does not describe the code it measured.

Review the diff before committing a new baseline. A changed span shape is a real change to the render pipeline and deserves an explanation; changed timings are just a different machine.

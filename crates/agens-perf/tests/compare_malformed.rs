use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agens_perf::{TraceReadError, read_trace};

static NEXT_TRACE: AtomicUsize = AtomicUsize::new(0);

fn write_trace(contents: &str) -> PathBuf {
    let suffix = NEXT_TRACE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "agens-perf-compare-malformed-{}-{suffix}.jsonl",
        std::process::id()
    ));
    fs::write(&path, contents).expect("trace contents written");
    path
}

/// Spec scenario "Truncated trace": a trace whose last line is cut off
/// mid-object must be rejected naming the file, and the comparison must
/// never be attempted on it. The comparison entry point reads each file with
/// [`read_trace`] before calling `compare`, so the truncation surfaces here,
/// before any trace is ever assembled.
#[test]
fn comparing_a_truncated_trace_fails_naming_the_file_and_the_truncation() {
    let complete_trace = write_trace(
        r#"{"record":"run","schema_version":1,"run_id":"r1","started_at_unix_ms":0,"commit":null,"worktree_dirty":false,"host":null,"debug_assertions":true,"fields":{}}
"#,
    );
    let truncated_trace = write_trace(
        r#"{"record":"run","schema_version":1,"run_id":"r2","started_at_unix_ms":0,"commit":null,"worktree_dirty":false,"host":null,"debug_ass"#,
    );

    let error = read_trace(&truncated_trace).expect_err("a mid-object cut file must be rejected");

    match error {
        TraceReadError::InvalidJson { file, line, .. } => {
            assert_eq!(file, truncated_trace.display().to_string());
            assert_eq!(line, 1);
        }
        other => panic!("expected InvalidJson, got {other:?}"),
    }

    // The complete trace on its own is unaffected: reading it still succeeds,
    // which demonstrates the truncation is what triggers the failure, not
    // some unrelated malformed-trace behaviour.
    read_trace(&complete_trace).expect("the complete trace still reads on its own");

    fs::remove_file(&complete_trace).expect("complete trace removed");
    fs::remove_file(&truncated_trace).expect("truncated trace removed");
}

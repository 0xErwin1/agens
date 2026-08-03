use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agens_perf::{TraceReadError, read_trace};

static NEXT_TRACE: AtomicUsize = AtomicUsize::new(0);

fn write_trace(contents: &str) -> PathBuf {
    let suffix = NEXT_TRACE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "agens-perf-reader-{}-{suffix}.jsonl",
        std::process::id()
    ));
    fs::write(&path, contents).expect("trace contents written");
    path
}

#[test]
fn reader_rejects_a_trace_whose_schema_version_is_unknown() {
    let path = write_trace(
        r#"{"record":"run","schema_version":99,"run_id":"r1","started_at_unix_ms":0,"commit":null,"worktree_dirty":false,"host":null,"debug_assertions":true,"fields":{}}
"#,
    );

    let error = read_trace(&path).expect_err("unknown schema_version must be rejected");

    match error {
        TraceReadError::UnsupportedSchemaVersion {
            found, expected, ..
        } => {
            assert_eq!(found, 99);
            assert_eq!(expected, agens_perf::SCHEMA_VERSION);
        }
        other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
    }

    fs::remove_file(&path).expect("trace file removed");
}

#[test]
fn reader_fails_loudly_naming_the_file_and_reason_on_invalid_json() {
    let path = write_trace("{ this is not json\n");

    let error = read_trace(&path).expect_err("invalid JSON line must be rejected");

    match error {
        TraceReadError::InvalidJson { file, line, .. } => {
            assert_eq!(file, path.display().to_string());
            assert_eq!(line, 1);
        }
        other => panic!("expected InvalidJson, got {other:?}"),
    }

    fs::remove_file(&path).expect("trace file removed");
}

#[test]
fn reader_tolerates_unknown_keys_on_both_record_kinds() {
    let path = write_trace(
        r#"{"record":"run","schema_version":1,"run_id":"r1","started_at_unix_ms":0,"commit":null,"worktree_dirty":false,"host":null,"debug_assertions":true,"fields":{},"future_run_key":"ignored"}
{"record":"span","span_id":1,"parent_span_id":null,"name":"tui.frame","target":"agens_tui::render","thread":0,"start_ns":0,"dur_ns":1,"fields":{},"future_span_key":"ignored"}
"#,
    );

    let records = read_trace(&path).expect("unknown keys must be tolerated");

    assert_eq!(records.len(), 2);

    fs::remove_file(&path).expect("trace file removed");
}

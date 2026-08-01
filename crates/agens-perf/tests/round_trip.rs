use agens_perf::{Record, RunMetadata, SCHEMA_VERSION, SpanRecord};

fn sample_run() -> RunMetadata {
    RunMetadata {
        schema_version: SCHEMA_VERSION,
        run_id: "1735689600000-abc1234".to_string(),
        started_at_unix_ms: 1_735_689_600_000,
        commit: Some("abc1234".to_string()),
        worktree_dirty: false,
        host: Some("dev-box".to_string()),
        debug_assertions: true,
        fields: serde_json::Map::new(),
    }
}

fn sample_span() -> SpanRecord {
    SpanRecord {
        span_id: 1,
        parent_span_id: None,
        name: "tui.frame".to_string(),
        target: "agens_tui::render".to_string(),
        thread: 0,
        start_ns: 100,
        dur_ns: 50,
        fields: serde_json::Map::new(),
    }
}

#[test]
fn run_and_span_records_round_trip_through_jsonl() {
    let run = Record::Run(sample_run());
    let span = Record::Span(sample_span());

    let jsonl = format!(
        "{}\n{}\n",
        serde_json::to_string(&run).expect("run serializes"),
        serde_json::to_string(&span).expect("span serializes"),
    );

    let mut lines = jsonl.lines();

    let round_tripped_run: Record =
        serde_json::from_str(lines.next().expect("run line present")).expect("run parses");
    let round_tripped_span: Record =
        serde_json::from_str(lines.next().expect("span line present")).expect("span parses");

    assert_eq!(round_tripped_run, run);
    assert_eq!(round_tripped_span, span);
    assert!(lines.next().is_none());
}

#[test]
fn record_tag_distinguishes_run_from_span_without_heuristics() {
    let run_json = serde_json::to_value(Record::Run(sample_run())).expect("run serializes");
    let span_json = serde_json::to_value(Record::Span(sample_span())).expect("span serializes");

    assert_eq!(run_json["record"], "run");
    assert_eq!(span_json["record"], "span");
}

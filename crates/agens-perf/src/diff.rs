//! Two-trace comparison: what changed and where.
//!
//! Span identity is the pair `(name, ancestor name path)`, not just `name`,
//! because the same stable name can be reached through different call paths.
//! Shape (which identities exist) and call count are deterministic signals:
//! they do not depend on the machine that produced the trace. Duration is
//! always advisory, because it depends on the machine, load and build
//! profile in ways this tool cannot control for — it is never the sole basis
//! for reporting a regression, and it is never compared against a threshold.
//!
//! `RunMetadata` carries no typed `scenario` or `terminal_size` field; the
//! design's public API reserves those for the open `fields` map. This module
//! reads them from `fields` under the conventional keys `"scenario"` and
//! `"terminal_size"` rather than extending the schema landed by an earlier
//! batch.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use crate::schema::{Record, RunMetadata, SpanRecord};

/// One trace, assembled from its parsed records and validated for internal
/// consistency: exactly one run-metadata record, and every span's
/// `parent_span_id` resolves to a span present in the same trace.
struct Trace {
    metadata: RunMetadata,
    spans: Vec<SpanRecord>,
}

#[derive(Debug)]
pub enum TraceAssemblyError {
    MissingRunMetadata,
    DanglingParentSpan {
        span_name: String,
        parent_span_id: u64,
    },
}

impl fmt::Display for TraceAssemblyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRunMetadata => {
                write!(formatter, "trace has no run-metadata record")
            }
            Self::DanglingParentSpan {
                span_name,
                parent_span_id,
            } => {
                write!(
                    formatter,
                    "span {span_name:?} names parent_span_id {parent_span_id} which is not present in the trace"
                )
            }
        }
    }
}

impl std::error::Error for TraceAssemblyError {}

impl Trace {
    fn assemble(records: Vec<Record>) -> Result<Self, TraceAssemblyError> {
        let mut metadata = None;
        let mut spans = Vec::new();

        for record in records {
            match record {
                Record::Run(run) => metadata = Some(run),
                Record::Span(span) => spans.push(span),
            }
        }

        let metadata = metadata.ok_or(TraceAssemblyError::MissingRunMetadata)?;

        let span_ids: BTreeSet<u64> = spans.iter().map(|span| span.span_id).collect();

        for span in &spans {
            if let Some(parent_id) = span.parent_span_id
                && !span_ids.contains(&parent_id)
            {
                return Err(TraceAssemblyError::DanglingParentSpan {
                    span_name: span.name.clone(),
                    parent_span_id: parent_id,
                });
            }
        }

        Ok(Self { metadata, spans })
    }
}

/// Per-span-identity totals within one trace.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpanAggregate {
    pub count: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    pub cache_hit_true: u64,
    pub cache_hit_false: u64,
}

impl SpanAggregate {
    pub fn mean_ns(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_ns as f64 / self.count as f64
        }
    }
}

/// A deterministic finding: a span identity whose shape (present in only one
/// trace) or call count differs between the two traces.
#[derive(Debug, Clone)]
pub struct SpanFinding {
    pub path: String,
    pub base: Option<SpanAggregate>,
    pub new: Option<SpanAggregate>,
}

impl SpanFinding {
    pub fn count_delta(&self) -> i64 {
        let base_count = self.base.as_ref().map_or(0, |aggregate| aggregate.count) as i64;
        let new_count = self.new.as_ref().map_or(0, |aggregate| aggregate.count) as i64;
        new_count - base_count
    }

    fn is_deterministic_change(&self) -> bool {
        match (&self.base, &self.new) {
            (Some(base), Some(new)) => base.count != new.count,
            _ => true,
        }
    }
}

/// An advisory finding: a span identity present in both traces, carrying
/// their durations. Never a regression on its own.
#[derive(Debug, Clone)]
pub struct AdvisoryFinding {
    pub path: String,
    pub base: SpanAggregate,
    pub new: SpanAggregate,
}

#[derive(Debug)]
pub struct DiffReport {
    pub deterministic: Vec<SpanFinding>,
    pub advisory: Vec<AdvisoryFinding>,
    /// Set when `host`, `debug_assertions` or `worktree_dirty` differ between
    /// the two traces: the advisory section is still produced, but relabelled
    /// so a reader cannot mistake it for a same-machine comparison.
    pub advisory_disclaimer: Option<String>,
    /// One line per metadata field that differs but does not block the
    /// comparison (`commit`, `terminal_size`).
    pub metadata_warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum TraceSide {
    Base,
    New,
}

impl fmt::Display for TraceSide {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Base => write!(formatter, "base"),
            Self::New => write!(formatter, "new"),
        }
    }
}

#[derive(Debug)]
pub enum CompareError {
    Trace {
        side: TraceSide,
        source: TraceAssemblyError,
    },
    SchemaVersionMismatch {
        base: u32,
        new: u32,
    },
    ScenarioMismatch {
        base: Option<serde_json::Value>,
        new: Option<serde_json::Value>,
    },
}

impl fmt::Display for CompareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Trace { side, source } => write!(formatter, "{side} trace: {source}"),
            Self::SchemaVersionMismatch { base, new } => {
                write!(
                    formatter,
                    "schema_version differs and traces are not comparable: base={base} new={new}"
                )
            }
            Self::ScenarioMismatch { base, new } => {
                write!(
                    formatter,
                    "scenario differs and traces are not comparable: base={} new={}",
                    describe_optional_value(base.as_ref()),
                    describe_optional_value(new.as_ref())
                )
            }
        }
    }
}

impl std::error::Error for CompareError {}

fn describe_optional_value(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "unset".to_string(),
    }
}

fn describe_optional_string(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("unset")
}

/// The ancestor-inclusive span-name path used as span identity: the current
/// span's name preceded by every enclosing span's name, root first.
fn span_path(span: &SpanRecord, spans_by_id: &HashMap<u64, &SpanRecord>) -> String {
    let mut segments = vec![span.name.as_str()];
    let mut current = span.parent_span_id;

    while let Some(parent_id) = current {
        let Some(parent) = spans_by_id.get(&parent_id) else {
            break;
        };

        segments.push(parent.name.as_str());
        current = parent.parent_span_id;
    }

    segments.reverse();
    segments.join("/")
}

fn aggregate(trace: &Trace) -> BTreeMap<String, SpanAggregate> {
    let spans_by_id: HashMap<u64, &SpanRecord> = trace
        .spans
        .iter()
        .map(|span| (span.span_id, span))
        .collect();

    let mut aggregates: BTreeMap<String, SpanAggregate> = BTreeMap::new();

    for span in &trace.spans {
        let path = span_path(span, &spans_by_id);
        let entry = aggregates.entry(path).or_default();

        entry.count += 1;
        entry.total_ns += span.dur_ns;
        entry.max_ns = entry.max_ns.max(span.dur_ns);

        match span
            .fields
            .get("cache_hit")
            .and_then(serde_json::Value::as_bool)
        {
            Some(true) => entry.cache_hit_true += 1,
            Some(false) => entry.cache_hit_false += 1,
            None => {}
        }
    }

    aggregates
}

fn metadata_warnings(base: &RunMetadata, new: &RunMetadata) -> Vec<String> {
    let mut warnings = Vec::new();

    if base.commit != new.commit {
        warnings.push(format!(
            "commit differs: base={} new={}",
            describe_optional_string(&base.commit),
            describe_optional_string(&new.commit),
        ));
    }

    let base_terminal_size = base.fields.get("terminal_size");
    let new_terminal_size = new.fields.get("terminal_size");

    if base_terminal_size != new_terminal_size {
        warnings.push(format!(
            "terminal_size differs: base={} new={}",
            describe_optional_value(base_terminal_size),
            describe_optional_value(new_terminal_size),
        ));
    }

    warnings
}

fn advisory_disclaimer(base: &RunMetadata, new: &RunMetadata) -> Option<String> {
    if base.host != new.host
        || base.debug_assertions != new.debug_assertions
        || base.worktree_dirty != new.worktree_dirty
    {
        Some("cross-machine or cross-build: these numbers are not comparable".to_string())
    } else {
        None
    }
}

fn build_findings(
    base_aggregates: &BTreeMap<String, SpanAggregate>,
    new_aggregates: &BTreeMap<String, SpanAggregate>,
) -> (Vec<SpanFinding>, Vec<AdvisoryFinding>) {
    let paths: BTreeSet<&String> = base_aggregates
        .keys()
        .chain(new_aggregates.keys())
        .collect();

    let mut deterministic = Vec::new();
    let mut advisory = Vec::new();

    for path in paths {
        let base_aggregate = base_aggregates.get(path).cloned();
        let new_aggregate = new_aggregates.get(path).cloned();

        if let (Some(base_aggregate), Some(new_aggregate)) = (&base_aggregate, &new_aggregate) {
            advisory.push(AdvisoryFinding {
                path: path.clone(),
                base: base_aggregate.clone(),
                new: new_aggregate.clone(),
            });
        }

        let finding = SpanFinding {
            path: path.clone(),
            base: base_aggregate,
            new: new_aggregate,
        };

        if finding.is_deterministic_change() {
            deterministic.push(finding);
        }
    }

    deterministic.sort_by_key(|finding| std::cmp::Reverse(finding.count_delta().abs()));

    (deterministic, advisory)
}

/// Compares two traces and reports what changed and where. Refuses traces
/// that are not internally consistent, or whose `schema_version` or
/// `scenario` differ — those are not comparable at all. A differing `commit`
/// or `terminal_size` does not block the comparison, but is surfaced as a
/// prominent warning.
pub fn compare(base: Vec<Record>, new: Vec<Record>) -> Result<DiffReport, CompareError> {
    let base = Trace::assemble(base).map_err(|source| CompareError::Trace {
        side: TraceSide::Base,
        source,
    })?;
    let new = Trace::assemble(new).map_err(|source| CompareError::Trace {
        side: TraceSide::New,
        source,
    })?;

    if base.metadata.schema_version != new.metadata.schema_version {
        return Err(CompareError::SchemaVersionMismatch {
            base: base.metadata.schema_version,
            new: new.metadata.schema_version,
        });
    }

    let base_scenario = base.metadata.fields.get("scenario").cloned();
    let new_scenario = new.metadata.fields.get("scenario").cloned();

    if base_scenario != new_scenario {
        return Err(CompareError::ScenarioMismatch {
            base: base_scenario,
            new: new_scenario,
        });
    }

    let metadata_warnings = metadata_warnings(&base.metadata, &new.metadata);
    let advisory_disclaimer = advisory_disclaimer(&base.metadata, &new.metadata);

    let base_aggregates = aggregate(&base);
    let new_aggregates = aggregate(&new);
    let (deterministic, advisory) = build_findings(&base_aggregates, &new_aggregates);

    Ok(DiffReport {
        deterministic,
        advisory,
        advisory_disclaimer,
        metadata_warnings,
    })
}

fn format_deterministic_row(finding: &SpanFinding) -> String {
    match (&finding.base, &finding.new) {
        (Some(base), Some(new)) => {
            format!(
                "  {path}  base={base_count}  new={new_count}  delta={delta:+}\n",
                path = finding.path,
                base_count = base.count,
                new_count = new.count,
                delta = finding.count_delta(),
            )
        }
        (None, Some(new)) => {
            format!(
                "  + {path}  new_count={count}  (absent from base)\n",
                path = finding.path,
                count = new.count,
            )
        }
        (Some(base), None) => {
            format!(
                "  - {path}  base_count={count}  (absent from new)\n",
                path = finding.path,
                count = base.count,
            )
        }
        (None, None) => {
            unreachable!("a deterministic finding always has at least one side present")
        }
    }
}

fn format_advisory_row(finding: &AdvisoryFinding) -> String {
    format!(
        "  ~ {path}  base_mean_ns={base_mean:.0}  new_mean_ns={new_mean:.0}  delta_ns={delta:+.0}\n",
        path = finding.path,
        base_mean = finding.base.mean_ns(),
        new_mean = finding.new.mean_ns(),
        delta = finding.new.mean_ns() - finding.base.mean_ns(),
    )
}

/// Renders a [`DiffReport`] as text with the deterministic and advisory
/// sections kept structurally separate, so a reader skimming the output
/// cannot mistake a duration difference for a deterministic regression.
pub fn render_text(report: &DiffReport) -> String {
    let mut output = String::new();

    for warning in &report.metadata_warnings {
        output.push_str("WARNING: ");
        output.push_str(warning);
        output.push('\n');
    }

    if !report.metadata_warnings.is_empty() {
        output.push('\n');
    }

    output.push_str("DETERMINISTIC — span shape and call count (machine-independent)\n");

    if report.deterministic.is_empty() {
        output.push_str("  (no deterministic differences)\n");
    } else {
        for finding in &report.deterministic {
            output.push_str(&format_deterministic_row(finding));
        }
    }

    output.push('\n');

    match &report.advisory_disclaimer {
        Some(reason) => {
            output.push_str("ADVISORY — ");
            output.push_str(reason);
            output.push('\n');
        }
        None => {
            output.push_str(
                "ADVISORY — wall clock (machine-, load- and profile-dependent; NOT a threshold)\n",
            );
        }
    }

    if report.advisory.is_empty() {
        output.push_str("  (no comparable spans)\n");
    } else {
        for finding in &report.advisory {
            output.push_str(&format_advisory_row(finding));
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect()
    }

    fn run_metadata(
        commit: Option<&str>,
        host: Option<&str>,
        debug_assertions: bool,
        worktree_dirty: bool,
        extra_fields: &[(&str, serde_json::Value)],
    ) -> RunMetadata {
        RunMetadata {
            schema_version: crate::SCHEMA_VERSION,
            run_id: "run-1".to_string(),
            started_at_unix_ms: 0,
            commit: commit.map(str::to_string),
            worktree_dirty,
            host: host.map(str::to_string),
            debug_assertions,
            fields: fields(extra_fields),
        }
    }

    fn span(
        span_id: u64,
        parent_span_id: Option<u64>,
        name: &str,
        dur_ns: u64,
        extra_fields: &[(&str, serde_json::Value)],
    ) -> SpanRecord {
        SpanRecord {
            span_id,
            parent_span_id,
            name: name.to_string(),
            target: "agens_tui::render".to_string(),
            thread: 0,
            start_ns: 0,
            dur_ns,
            fields: fields(extra_fields),
        }
    }

    fn scenario_fields(name: &str) -> Vec<(&'static str, serde_json::Value)> {
        vec![("scenario", serde_json::Value::String(name.to_string()))]
    }

    #[test]
    fn diff_reports_a_settled_turn_cache_regression_as_a_count_delta() {
        let scenario = scenario_fields("settled_turn_regression");

        let base_metadata = run_metadata(Some("abc1234"), Some("dev-box"), true, false, &scenario);
        let base_records = vec![
            Record::Run(base_metadata),
            Record::Span(span(1, None, "perf.scenario", 1_000_000, &[])),
            Record::Span(span(
                2,
                Some(1),
                "tui.transcript.settled_turn",
                1_000,
                &[("cache_hit", serde_json::Value::Bool(true))],
            )),
        ];

        let new_metadata = run_metadata(Some("def5678"), Some("dev-box"), true, false, &scenario);
        let mut new_records = vec![
            Record::Run(new_metadata),
            Record::Span(span(1, None, "perf.scenario", 1_000_000, &[])),
        ];
        for id in 2..98 {
            new_records.push(Record::Span(span(
                id,
                Some(1),
                "tui.transcript.settled_turn",
                1_000,
                &[("cache_hit", serde_json::Value::Bool(false))],
            )));
        }

        let report = compare(base_records, new_records).expect("comparable traces");

        let settled_turn = report
            .deterministic
            .iter()
            .find(|finding| finding.path == "perf.scenario/tui.transcript.settled_turn")
            .expect("settled_turn finding present");

        assert_eq!(settled_turn.base.as_ref().unwrap().count, 1);
        assert_eq!(settled_turn.new.as_ref().unwrap().count, 96);
        assert_eq!(settled_turn.count_delta(), 95);
        assert_eq!(settled_turn.new.as_ref().unwrap().cache_hit_false, 96);
    }

    #[test]
    fn advisory_timing_section_is_labelled_and_separate_from_the_deterministic_section() {
        let scenario = scenario_fields("identical_shape");

        let base_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box"),
                true,
                false,
                &scenario,
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
            Record::Span(span(2, Some(1), "tui.frame", 1_000, &[])),
        ];

        let new_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box"),
                true,
                false,
                &scenario,
            )),
            Record::Span(span(1, None, "perf.scenario", 5_000_000, &[])),
            Record::Span(span(2, Some(1), "tui.frame", 5_000_000, &[])),
        ];

        let report = compare(base_records, new_records).expect("comparable traces");

        assert!(
            report.deterministic.is_empty(),
            "identical shape and call count must yield zero deterministic rows"
        );
        assert_eq!(report.advisory.len(), 2);
        assert!(report.advisory_disclaimer.is_none());

        let text = render_text(&report);
        let deterministic_index = text.find("DETERMINISTIC").expect("deterministic header");
        let advisory_index = text.find("ADVISORY").expect("advisory header");
        assert!(deterministic_index < advisory_index);
        assert!(text.contains("(no deterministic differences)"));
        assert!(text.contains("~ perf.scenario/tui.frame"));
    }

    #[test]
    fn timing_section_is_disclaimed_when_hosts_or_build_profiles_differ() {
        let scenario = scenario_fields("cross_machine");

        let base_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box-a"),
                true,
                false,
                &scenario,
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
        ];

        let new_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box-b"),
                true,
                false,
                &scenario,
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
        ];

        let report = compare(base_records, new_records).expect("host mismatch does not refuse");

        let disclaimer = report
            .advisory_disclaimer
            .as_ref()
            .expect("advisory section must be disclaimed on host mismatch");
        assert!(disclaimer.contains("not comparable"));

        let text = render_text(&report);
        assert!(
            text.contains(
                "ADVISORY — cross-machine or cross-build: these numbers are not comparable"
            )
        );
    }

    #[test]
    fn comparing_traces_with_different_schema_versions_is_refused() {
        let scenario = scenario_fields("same_scenario");

        let mut base_metadata =
            run_metadata(Some("abc1234"), Some("dev-box"), true, false, &scenario);
        base_metadata.schema_version = 1;
        let base_records = vec![
            Record::Run(base_metadata),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
        ];

        let mut new_metadata =
            run_metadata(Some("abc1234"), Some("dev-box"), true, false, &scenario);
        new_metadata.schema_version = 2;
        let new_records = vec![
            Record::Run(new_metadata),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
        ];

        let error = compare(base_records, new_records).expect_err("schema_version differs");

        match error {
            CompareError::SchemaVersionMismatch { base, new } => {
                assert_eq!(base, 1);
                assert_eq!(new, 2);
            }
            other => panic!("expected SchemaVersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn comparing_traces_with_different_scenarios_is_refused() {
        let base_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box"),
                true,
                false,
                &scenario_fields("short_transcript"),
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
        ];

        let new_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box"),
                true,
                false,
                &scenario_fields("long_transcript"),
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
        ];

        let error = compare(base_records, new_records).expect_err("scenario differs");

        match error {
            CompareError::ScenarioMismatch { base, new } => {
                assert_eq!(
                    base.unwrap(),
                    serde_json::Value::String("short_transcript".to_string())
                );
                assert_eq!(
                    new.unwrap(),
                    serde_json::Value::String("long_transcript".to_string())
                );
            }
            other => panic!("expected ScenarioMismatch, got {other:?}"),
        }
    }

    #[test]
    fn comparing_traces_with_different_terminal_sizes_produces_a_warning_naming_both_sizes() {
        let mut base_extra = scenario_fields("resize");
        base_extra.push((
            "terminal_size",
            serde_json::json!({"columns": 80, "rows": 24}),
        ));
        let mut new_extra = scenario_fields("resize");
        new_extra.push((
            "terminal_size",
            serde_json::json!({"columns": 120, "rows": 40}),
        ));

        let base_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box"),
                true,
                false,
                &base_extra,
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
        ];

        let new_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box"),
                true,
                false,
                &new_extra,
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
        ];

        let report =
            compare(base_records, new_records).expect("terminal_size mismatch does not refuse");

        let warning = report
            .metadata_warnings
            .iter()
            .find(|warning| warning.contains("terminal_size"))
            .expect("a terminal_size warning is present");
        assert!(warning.contains("80"));
        assert!(warning.contains("120"));
    }

    #[test]
    fn a_span_present_in_only_one_trace_is_reported_as_added_or_removed_not_zero_time() {
        let scenario = scenario_fields("shape_change");

        let base_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box"),
                true,
                false,
                &scenario,
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
        ];

        let new_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box"),
                true,
                false,
                &scenario,
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
            Record::Span(span(2, Some(1), "tui.syntax.tokens", 500, &[])),
        ];

        let report = compare(base_records, new_records).expect("comparable traces");

        let added = report
            .deterministic
            .iter()
            .find(|finding| finding.path == "perf.scenario/tui.syntax.tokens")
            .expect("added span reported");

        assert!(added.base.is_none());
        assert!(added.new.is_some());

        assert!(
            report
                .advisory
                .iter()
                .all(|finding| finding.path != "perf.scenario/tui.syntax.tokens"),
            "a one-sided span has no comparable duration and must not appear in the advisory section"
        );
    }

    #[test]
    fn comparing_a_trace_with_a_dangling_parent_reference_fails_naming_the_offending_span() {
        let scenario = scenario_fields("dangling_parent");

        let base_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box"),
                true,
                false,
                &scenario,
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
            Record::Span(span(2, Some(99), "tui.frame", 500, &[])),
        ];

        let new_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box"),
                true,
                false,
                &scenario,
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
        ];

        let error =
            compare(base_records, new_records).expect_err("dangling parent must be rejected");

        match error {
            CompareError::Trace {
                side: TraceSide::Base,
                source:
                    TraceAssemblyError::DanglingParentSpan {
                        span_name,
                        parent_span_id,
                    },
            } => {
                assert_eq!(span_name, "tui.frame");
                assert_eq!(parent_span_id, 99);
            }
            other => panic!("expected a dangling-parent TraceAssemblyError, got {other:?}"),
        }
    }

    #[test]
    fn comparing_a_trace_with_no_run_metadata_record_fails_rather_than_guessing() {
        let scenario = scenario_fields("missing_metadata");

        let base_records = vec![Record::Span(span(1, None, "perf.scenario", 1_000, &[]))];

        let new_records = vec![
            Record::Run(run_metadata(
                Some("abc1234"),
                Some("dev-box"),
                true,
                false,
                &scenario,
            )),
            Record::Span(span(1, None, "perf.scenario", 1_000, &[])),
        ];

        let error =
            compare(base_records, new_records).expect_err("missing run metadata must be rejected");

        match error {
            CompareError::Trace {
                side: TraceSide::Base,
                source: TraceAssemblyError::MissingRunMetadata,
            } => {}
            other => panic!("expected MissingRunMetadata, got {other:?}"),
        }
    }
}

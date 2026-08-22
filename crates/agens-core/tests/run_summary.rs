use agens_core::summary::{
    CausalDisposition, CheckpointInput, Constraint, ConstraintSource, CriticalContext,
    EvidenceClass, Finding, FindingInput, FindingSection, Goal, OpenQuestionInput, PathAccess,
    RunHealthInput, RunSummary, RunSummaryInputs, SummaryProjection, SummarySection, TouchedPath,
    render::{
        MAX_TOOL_RESULT_CHARS, TranscriptEntry, render_compaction, render_engram_session_summary,
        render_flat_transcript, render_run_report,
    },
};

fn finding(description: &str, causal_disposition: CausalDisposition) -> Finding {
    Finding {
        description: description.to_owned(),
        evidence_class: EvidenceClass::Deterministic,
        proof_refs: vec!["tests/run_summary.rs".to_owned()],
        causal_disposition,
    }
}

fn populated_inputs() -> RunSummaryInputs {
    RunSummaryInputs {
        goal: Goal {
            scope: "ship the summary schema".to_owned(),
            definition_of_done: "the gate is green".to_owned(),
        },
        constraints: vec![
            Constraint {
                source: ConstraintSource::Spec,
                text: "no consumer wiring".to_owned(),
            },
            Constraint {
                source: ConstraintSource::Preference,
                text: "conventional commits".to_owned(),
            },
        ],
        findings: vec![
            FindingInput {
                finding: finding("froze the section list", CausalDisposition::CandidateCaused),
                section: FindingSection::KeyDecision,
            },
            FindingInput {
                finding: finding("the two harnesses agree", CausalDisposition::PreExisting),
                section: FindingSection::Discovery,
            },
        ],
        checkpoints: vec![
            CheckpointInput {
                declared_goal: "assemble the sections".to_owned(),
                evidenced: true,
                next_goal: Some("write the serializers".to_owned()),
            },
            CheckpointInput {
                declared_goal: "write the serializers".to_owned(),
                evidenced: false,
                next_goal: Some("run the gate".to_owned()),
            },
        ],
        open_questions: vec![OpenQuestionInput {
            blocked_decision: "which crate owns the schema".to_owned(),
        }],
        health: Some(RunHealthInput {
            noop_turns: 2,
            failing_test_signature: Some("run_summary::projections".to_owned()),
        }),
        touched_paths: vec![
            TouchedPath {
                path: "crates/agens-core/src/summary/mod.rs".to_owned(),
                access: PathAccess::Read,
            },
            TouchedPath {
                path: "crates/agens-core/src/summary/mod.rs".to_owned(),
                access: PathAccess::Modified,
            },
            TouchedPath {
                path: "CODE_STYLE.md".to_owned(),
                access: PathAccess::Read,
            },
        ],
    }
}

fn heading(title: &str) -> String {
    format!("## {title}")
}

#[test]
fn every_section_is_present_in_a_summary_assembled_from_nothing() {
    let summary = RunSummary::assemble(RunSummaryInputs::default());
    let rendered = render_compaction(&summary, None, &[]);

    for section in SummarySection::ALL {
        assert!(
            summary.section_is_empty(section),
            "{} should be empty",
            section.title()
        );
        assert!(
            rendered.contains(&heading(section.title())),
            "{} should still be rendered",
            section.title()
        );
    }

    // Six single-body sections, Progress' three groups, Relevant Files' two,
    // the absent previous summary and the empty transcript.
    assert_eq!(rendered.matches("(none)").count(), 13);
}

#[test]
fn each_projection_carries_exactly_the_sections_the_contract_names() {
    let dropped = |projection: SummaryProjection| {
        SummarySection::ALL
            .into_iter()
            .filter(|section| !projection.includes(*section))
            .collect::<Vec<_>>()
    };

    assert!(dropped(SummaryProjection::Compaction).is_empty());
    assert_eq!(
        dropped(SummaryProjection::RunReport),
        vec![
            SummarySection::ConstraintsAndPreferences,
            SummarySection::CriticalContext,
        ]
    );
    assert_eq!(
        dropped(SummaryProjection::Engram),
        vec![
            SummarySection::CriticalContext,
            SummarySection::RelevantFiles,
        ]
    );

    assert_eq!(
        SummaryProjection::Compaction.sections().count(),
        SummarySection::ALL.len()
    );
}

#[test]
fn the_run_report_renders_its_six_sections_and_omits_the_other_two() {
    let mut summary = RunSummary::assemble(populated_inputs());
    summary.set_critical_context(CriticalContext::narrated("the worktree is disposable"));

    let rendered = render_run_report(&summary);

    for section in SummaryProjection::RunReport.sections() {
        assert!(
            rendered.contains(&heading(section.title())),
            "{} should be rendered",
            section.title()
        );
    }

    assert!(!rendered.contains(&heading(SummarySection::ConstraintsAndPreferences.title())));
    assert!(!rendered.contains(&heading(SummarySection::CriticalContext.title())));
    assert!(!rendered.contains("the worktree is disposable"));
    assert!(!rendered.contains("conventional commits"));
}

#[test]
fn the_engram_summary_renames_its_headings_and_drops_files_and_narrative() {
    let mut summary = RunSummary::assemble(populated_inputs());
    summary.set_critical_context(CriticalContext::narrated("the worktree is disposable"));

    let rendered = render_engram_session_summary(&summary);

    for title in [
        "Goal",
        "Instructions",
        "Discoveries",
        "Accomplished",
        "Next Steps",
    ] {
        assert!(
            rendered.contains(&heading(title)),
            "{title} should be rendered"
        );
    }

    assert!(!rendered.contains(&heading(SummarySection::RelevantFiles.title())));
    assert!(!rendered.contains(&heading(SummarySection::CriticalContext.title())));
    assert!(!rendered.contains("the worktree is disposable"));
    assert!(!rendered.contains("CODE_STYLE.md"));

    assert!(rendered.contains("froze the section list"));
    assert!(rendered.contains("the two harnesses agree"));
}

#[test]
fn an_empty_section_is_still_rendered_in_every_projection() {
    let summary = RunSummary::assemble(RunSummaryInputs::default());

    for rendered in [
        render_run_report(&summary),
        render_engram_session_summary(&summary),
    ] {
        assert!(rendered.contains("(none)"));
        assert!(rendered.contains(&heading("Goal")));
        assert!(rendered.contains(&heading("Next Steps")));
    }
}

#[test]
fn a_tool_result_is_truncated_at_two_thousand_characters() {
    let content = "x".repeat(MAX_TOOL_RESULT_CHARS + 37);
    let rendered = render_flat_transcript(&[TranscriptEntry::ToolResult(content)]);

    assert_eq!(rendered.matches('x').count(), MAX_TOOL_RESULT_CHARS);
    assert!(rendered.contains("[truncated, 37 characters omitted]"));
    assert!(rendered.starts_with("[Tool result]: "));
}

#[test]
fn a_tool_result_at_the_limit_is_left_alone() {
    let content = "x".repeat(MAX_TOOL_RESULT_CHARS);
    let rendered = render_flat_transcript(&[TranscriptEntry::ToolResult(content)]);

    assert_eq!(rendered.matches('x').count(), MAX_TOOL_RESULT_CHARS);
    assert!(!rendered.contains("truncated"));
}

#[test]
fn truncation_cuts_characters_and_never_a_multibyte_boundary() {
    let content = "é".repeat(MAX_TOOL_RESULT_CHARS + 5);
    let rendered = render_flat_transcript(&[TranscriptEntry::ToolResult(content)]);

    assert_eq!(rendered.matches('é').count(), MAX_TOOL_RESULT_CHARS);
    assert!(rendered.contains("[truncated, 5 characters omitted]"));
}

#[test]
fn conversation_entries_are_never_truncated() {
    let long = "y".repeat(MAX_TOOL_RESULT_CHARS + 10);
    let rendered = render_flat_transcript(&[
        TranscriptEntry::User(long.clone()),
        TranscriptEntry::Assistant(long.clone()),
    ]);

    assert_eq!(
        rendered.matches('y').count(),
        2 * (MAX_TOOL_RESULT_CHARS + 10)
    );
    assert!(!rendered.contains("truncated"));
}

#[test]
fn a_transcript_is_flat_and_labelled_per_entry() {
    let rendered = render_flat_transcript(&[
        TranscriptEntry::User("add the schema".to_owned()),
        TranscriptEntry::Assistant("assembling".to_owned()),
        TranscriptEntry::ToolResult("ok".to_owned()),
    ]);

    assert_eq!(
        rendered,
        "[User]: add the schema\n[Assistant]: assembling\n[Tool result]: ok"
    );
}

#[test]
fn compaction_carries_the_previous_summary_so_it_is_updated_not_regenerated() {
    let previous = RunSummary::assemble(populated_inputs());
    let mut current = RunSummary::assemble(RunSummaryInputs::default());
    current.set_critical_context(CriticalContext::narrated("still the same run"));

    let rendered = render_compaction(
        &current,
        Some(&previous),
        &[TranscriptEntry::User("carry on".to_owned())],
    );

    assert!(rendered.contains("# Previous Summary"));
    assert!(rendered.contains("froze the section list"));
    assert!(rendered.contains("still the same run"));
    assert!(rendered.contains("[User]: carry on"));

    let without_previous = render_compaction(&current, None, &[]);
    assert!(without_previous.contains("# Previous Summary\n\n(none)"));
}

#[test]
fn findings_are_routed_to_their_section_and_carry_their_proof() {
    let summary = RunSummary::assemble(populated_inputs());

    assert_eq!(summary.key_decisions().len(), 1);
    assert_eq!(summary.discoveries().len(), 1);
    assert_eq!(
        summary
            .key_decisions()
            .first()
            .map(|f| f.description.as_str()),
        Some("froze the section list")
    );

    let rendered = render_run_report(&summary);
    assert!(rendered.contains(
        "- froze the section list [deterministic, candidate-caused; proof: tests/run_summary.rs]"
    ));
}

#[test]
fn the_default_routing_follows_the_causal_disposition() {
    assert_eq!(
        FindingSection::derive(CausalDisposition::CandidateCaused),
        FindingSection::KeyDecision
    );
    assert_eq!(
        FindingSection::derive(CausalDisposition::PreExisting),
        FindingSection::Discovery
    );
    assert_eq!(
        FindingSection::derive(CausalDisposition::Unknown),
        FindingSection::Discovery
    );
}

#[test]
fn progress_splits_checkpoints_by_evidence_and_blocks_on_questions_and_health() {
    let summary = RunSummary::assemble(populated_inputs());
    let progress = summary.progress();

    assert_eq!(progress.done, vec!["assemble the sections".to_owned()]);
    assert_eq!(
        progress.in_progress,
        vec!["write the serializers".to_owned()]
    );
    assert_eq!(
        progress.blocked,
        vec![
            "awaiting an answer: which crate owns the schema".to_owned(),
            "failing test: run_summary::projections".to_owned(),
            "2 turns with no recorded progress".to_owned(),
        ]
    );
}

#[test]
fn next_steps_come_from_the_last_checkpoint() {
    let summary = RunSummary::assemble(populated_inputs());

    assert_eq!(summary.next_steps(), ["run the gate".to_owned()]);
}

#[test]
fn relevant_files_are_deduplicated_with_modified_winning() {
    let summary = RunSummary::assemble(populated_inputs());
    let files = summary.relevant_files();

    assert_eq!(files.read, vec!["CODE_STYLE.md".to_owned()]);
    assert_eq!(
        files.modified,
        vec!["crates/agens-core/src/summary/mod.rs".to_owned()]
    );
}

#[test]
fn a_narrative_pass_can_only_change_critical_context() {
    let assembled = RunSummary::assemble(populated_inputs());
    let mut narrated = assembled.clone();
    narrated.set_critical_context(CriticalContext::narrated("  the rows are the source  "));

    assert_eq!(narrated.goal(), assembled.goal());
    assert_eq!(narrated.constraints(), assembled.constraints());
    assert_eq!(narrated.progress(), assembled.progress());
    assert_eq!(narrated.key_decisions(), assembled.key_decisions());
    assert_eq!(narrated.discoveries(), assembled.discoveries());
    assert_eq!(narrated.next_steps(), assembled.next_steps());
    assert_eq!(narrated.relevant_files(), assembled.relevant_files());

    assert_eq!(
        narrated.critical_context().text(),
        Some("the rows are the source")
    );
    assert_eq!(render_run_report(&narrated), render_run_report(&assembled));
}

#[test]
fn a_blank_narration_leaves_the_section_empty() {
    let mut summary = RunSummary::assemble(populated_inputs());
    summary.set_critical_context(CriticalContext::narrated("   \n  "));

    assert!(summary.section_is_empty(SummarySection::CriticalContext));
    assert!(render_compaction(&summary, None, &[]).contains("## Critical Context\n\n(none)"));
}

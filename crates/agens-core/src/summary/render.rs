//! The three serializations of a [`RunSummary`], one per consumer.
//!
//! Rules the format carries with it, and that every consumer inherits by using
//! these functions instead of writing their own:
//!
//! - A projected section is always emitted, with an explicit `(none)` when it
//!   is empty. A reader can tell a run that decided nothing from a producer
//!   that forgot the section.
//! - A tool result is truncated at [`MAX_TOOL_RESULT_CHARS`] when serialized.
//! - Conversation is serialized as a flat transcript, not as messages, so a
//!   model reads it as material to summarize rather than as a conversation to
//!   continue.
//! - Compaction is iterative: the previous summary travels with the new
//!   material so it is updated rather than regenerated.

use std::borrow::Cow;

use super::{
    Constraint, ConstraintSource, Finding, Progress, RelevantFiles, RunSummary, SummaryProjection,
    SummarySection,
};

/// Where a serialized tool result is cut.
pub const MAX_TOOL_RESULT_CHARS: usize = 2000;

const EMPTY: &str = "(none)";

/// One entry of a flat transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscriptEntry {
    User(String),
    Assistant(String),
    ToolResult(String),
}

impl TranscriptEntry {
    const fn label(&self) -> &'static str {
        match self {
            Self::User(_) => "[User]:",
            Self::Assistant(_) => "[Assistant]:",
            Self::ToolResult(_) => "[Tool result]:",
        }
    }

    /// Only a tool result is truncated. A person's message and a model's reply
    /// are the material being summarized; cutting them would lose the thing
    /// the summary is about.
    fn body(&self) -> Cow<'_, str> {
        match self {
            Self::User(text) | Self::Assistant(text) => Cow::Borrowed(text.as_str()),
            Self::ToolResult(text) => truncate_tool_result(text),
        }
    }
}

fn truncate_tool_result(content: &str) -> Cow<'_, str> {
    let Some((boundary, _)) = content.char_indices().nth(MAX_TOOL_RESULT_CHARS) else {
        return Cow::Borrowed(content);
    };

    let (retained, omitted) = content.split_at(boundary);
    let omitted_characters = omitted.chars().count();

    Cow::Owned(format!(
        "{retained}\n… [truncated, {omitted_characters} characters omitted]"
    ))
}

/// The flat transcript a summarizing model reads.
#[must_use]
pub fn render_flat_transcript(entries: &[TranscriptEntry]) -> String {
    if entries.is_empty() {
        return EMPTY.to_owned();
    }

    let mut rendered = String::new();
    for entry in entries {
        rendered.push_str(&format!("{} {}\n", entry.label(), entry.body()));
    }

    rendered.trim_end().to_owned()
}

/// The compaction input: the summary so far, every section of the new
/// summary, and the transcript that has to be folded into it.
#[must_use]
pub fn render_compaction(
    summary: &RunSummary,
    previous: Option<&RunSummary>,
    transcript: &[TranscriptEntry],
) -> String {
    let previous = previous.map_or_else(
        || EMPTY.to_owned(),
        |previous| render_sections(previous, SummaryProjection::Compaction),
    );

    format!(
        "# Previous Summary\n\n{previous}\n\n# Summary\n\n{}\n\n# Transcript\n\n{}",
        render_sections(summary, SummaryProjection::Compaction),
        render_flat_transcript(transcript),
    )
}

/// The summary alone, under the compaction projection.
///
/// [`render_compaction`] renders the input a summarizing model reads; this
/// renders the output it produced, which is what replaces the head of a
/// history and what a later compaction folds into its own. Every section is
/// projected, because a compacted thread is the only remaining record of what
/// it replaced.
#[must_use]
pub fn render_compaction_summary(summary: &RunSummary) -> String {
    render_sections(summary, SummaryProjection::Compaction)
}

/// The run report: what a reader has to act on, assembled from rows and
/// therefore producible with every provider capped.
#[must_use]
pub fn render_run_report(summary: &RunSummary) -> String {
    render_sections(summary, SummaryProjection::RunReport)
}

/// The body of an Engram `mem_session_summary`.
///
/// Engram mandates its own six headings, so the projection is renamed rather
/// than re-cut: constraints become Instructions, the two finding-backed
/// sections share Discoveries, and Progress lands under Accomplished with its
/// three groups intact instead of dropping the two that are not done. Relevant
/// Files is the sixth Engram heading and is deliberately absent: the paths of
/// an ephemeral worktree are not a lesson.
#[must_use]
pub fn render_engram_session_summary(summary: &RunSummary) -> String {
    let mut rendered = String::new();

    push_heading(&mut rendered, "Goal", &render_goal(summary));
    push_heading(
        &mut rendered,
        "Instructions",
        &render_constraints(summary.constraints()),
    );

    let findings: Vec<&Finding> = summary
        .key_decisions()
        .iter()
        .chain(summary.discoveries())
        .collect();
    push_heading(
        &mut rendered,
        "Discoveries",
        &render_finding_refs(&findings),
    );

    push_heading(
        &mut rendered,
        "Accomplished",
        &render_progress(summary.progress()),
    );
    push_heading(
        &mut rendered,
        "Next Steps",
        &render_list(summary.next_steps()),
    );

    rendered.trim_end().to_owned()
}

fn render_sections(summary: &RunSummary, projection: SummaryProjection) -> String {
    let mut rendered = String::new();

    for section in projection.sections() {
        push_heading(
            &mut rendered,
            section.title(),
            &render_section(summary, section),
        );
    }

    rendered.trim_end().to_owned()
}

fn push_heading(rendered: &mut String, title: &str, body: &str) {
    rendered.push_str(&format!("## {title}\n\n{body}\n\n"));
}

fn render_section(summary: &RunSummary, section: SummarySection) -> String {
    match section {
        SummarySection::Goal => render_goal(summary),
        SummarySection::ConstraintsAndPreferences => render_constraints(summary.constraints()),
        SummarySection::Progress => render_progress(summary.progress()),
        SummarySection::KeyDecisions => render_findings(summary.key_decisions()),
        SummarySection::Discoveries => render_findings(summary.discoveries()),
        SummarySection::NextSteps => render_list(summary.next_steps()),
        SummarySection::CriticalContext => summary
            .critical_context()
            .text()
            .map_or_else(|| EMPTY.to_owned(), str::to_owned),
        SummarySection::RelevantFiles => render_relevant_files(summary.relevant_files()),
    }
}

fn render_goal(summary: &RunSummary) -> String {
    if summary.section_is_empty(SummarySection::Goal) {
        return EMPTY.to_owned();
    }

    let goal = summary.goal();
    format!(
        "Scope: {}\nDefinition of done: {}",
        blank_as_empty(&goal.scope),
        blank_as_empty(&goal.definition_of_done),
    )
}

fn render_constraints(constraints: &[Constraint]) -> String {
    if constraints.is_empty() {
        return EMPTY.to_owned();
    }

    constraints
        .iter()
        .map(|constraint| {
            let source = match constraint.source {
                ConstraintSource::Spec => "spec",
                ConstraintSource::Preference => "preference",
            };
            format!("- [{source}] {}", constraint.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_progress(progress: &Progress) -> String {
    [
        ("Done", &progress.done),
        ("In Progress", &progress.in_progress),
        ("Blocked", &progress.blocked),
    ]
    .into_iter()
    .map(|(title, entries)| format!("{title}:\n{}", render_list(entries)))
    .collect::<Vec<_>>()
    .join("\n\n")
}

fn render_findings(findings: &[Finding]) -> String {
    let findings: Vec<&Finding> = findings.iter().collect();
    render_finding_refs(&findings)
}

fn render_finding_refs(findings: &[&Finding]) -> String {
    if findings.is_empty() {
        return EMPTY.to_owned();
    }

    findings
        .iter()
        .map(|finding| {
            let proof = if finding.proof_refs.is_empty() {
                EMPTY.to_owned()
            } else {
                finding.proof_refs.join(", ")
            };

            format!(
                "- {} [{}, {}; proof: {proof}]",
                finding.description,
                finding.evidence_class.label(),
                finding.causal_disposition.label(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_relevant_files(files: &RelevantFiles) -> String {
    format!(
        "Read:\n{}\n\nModified:\n{}",
        render_list(&files.read),
        render_list(&files.modified),
    )
}

fn render_list(entries: &[String]) -> String {
    if entries.is_empty() {
        return EMPTY.to_owned();
    }

    entries
        .iter()
        .map(|entry| format!("- {entry}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn blank_as_empty(text: &str) -> &str {
    if text.trim().is_empty() { EMPTY } else { text }
}

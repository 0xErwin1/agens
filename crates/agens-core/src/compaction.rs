//! Replacing the oldest stretch of a history with a summary of it.
//!
//! A turn that outruns the model's context window dies today. Compaction is the
//! degradation that keeps it alive: the oldest messages are serialized to a
//! plain transcript, a model summarizes that transcript, and the summary takes
//! their place at the head of the history while the recent tail survives
//! verbatim.
//!
//! Two invariants govern everything here, and both exist because the failure
//! they prevent lands exactly when the system is already trying to recover:
//!
//! - A tool call and its result are never separated. A history whose tail
//!   carries a result for a call that is no longer present is rejected by the
//!   provider, so a cut that lands inside a tool block would turn a recoverable
//!   overflow into an unrecoverable one.
//! - Nothing is mutated until a usable summary exists. Every function in this
//!   module is pure and returns a new history; a summary that is empty, or a
//!   provider that failed to produce one, leaves the caller holding exactly the
//!   history it had.
//!
//! The prefix is serialized as a flat transcript rather than handed over as
//! messages, so the model treats it as a document to summarize instead of a
//! conversation to continue.

use crate::{Message, MessagePart, Role};

/// How much of a single tool result survives into the transcript.
///
/// Tool output is what actually fills a context window — a `read` of a large
/// file or a `bash` invocation with a verbose command dwarfs every message
/// around it — and a summary does not need the body to record what the call
/// established.
pub const TOOL_OUTPUT_MAX_CHARS: usize = 2_000;

/// The default budget, in estimated tokens, for the tail kept verbatim.
pub const DEFAULT_KEEP_RECENT_TOKENS: usize = 8_000;

/// Estimated tokens for a stretch of text.
///
/// A deliberate approximation: no tokenizer for the served models exists in
/// this process, and the value is only ever compared against a budget that is
/// itself a heuristic. Four bytes per token is the ratio the served models
/// average on English prose and source code.
fn estimated_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

fn message_tokens(message: &Message) -> usize {
    message
        .parts
        .iter()
        .map(|part| match part {
            MessagePart::Text(text) | MessagePart::Reasoning(text) => estimated_tokens(text),
            MessagePart::ToolCall { name, input, .. } => {
                estimated_tokens(name) + estimated_tokens(input)
            }
            MessagePart::ToolResult { content, .. } => {
                estimated_tokens(&content[..content.len().min(TOOL_OUTPUT_MAX_CHARS)])
            }
            MessagePart::Media { mime, .. } => estimated_tokens(mime),
        })
        .sum()
}

/// How much recent history a compaction keeps verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionBudget {
    pub keep_recent_tokens: usize,
}

impl Default for CompactionBudget {
    fn default() -> Self {
        Self {
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
        }
    }
}

/// Why a history could not be compacted.
///
/// Every variant is a refusal, never a partial result: the caller's history is
/// untouched in all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionError {
    /// The history is already inside the budget, or holds nothing that could be
    /// summarized without emptying it.
    NothingToCompact,
    /// Every cut point inside the budget falls between a tool call and its
    /// result. Happens when one turn's tool traffic alone outruns the tail
    /// budget; splitting such a turn into two summaries is a separate
    /// mechanism, and until it exists refusing is the only safe answer.
    NoValidCut,
    /// The summarizing model produced nothing usable.
    EmptySummary,
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NothingToCompact => "history has nothing to compact",
            Self::NoValidCut => "no cut point keeps every tool call with its result",
            Self::EmptySummary => "summary was empty",
        })
    }
}

impl std::error::Error for CompactionError {}

/// The summary text a compaction writes into the history.
///
/// The structured schema behind a summary, and the serializer that renders it,
/// belong to the summary format work; this newtype is the whole interface this
/// module needs from it — the rendered text, proven non-empty once, at the
/// boundary. A richer summary type converts into this by serializing itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionSummary(String);

impl CompactionSummary {
    /// Rejects a summary that carries no text, which is how a failed or refused
    /// summarizing call reaches this layer.
    pub fn new(text: impl Into<String>) -> Result<Self, CompactionError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(CompactionError::EmptySummary);
        }
        Ok(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A cut the caller may apply, once it holds a summary of what falls before it.
///
/// Holds no messages of its own: it names an index into the history the caller
/// still owns, so a plan that is never applied costs nothing and changes
/// nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionPlan {
    pinned: usize,
    first_kept: usize,
    transcript: String,
}

impl CompactionPlan {
    /// Index of the first message kept verbatim. Everything from [`Self::pinned`]
    /// up to it is what the summary replaces.
    pub fn first_kept(&self) -> usize {
        self.first_kept
    }

    /// How many leading messages the cut never touches.
    pub fn pinned(&self) -> usize {
        self.pinned
    }

    /// The flat transcript of the stretch being summarized.
    pub fn transcript(&self) -> &str {
        &self.transcript
    }

    /// The prompt handed to the summarizing model.
    ///
    /// A previous summary is folded in rather than discarded, so a session
    /// compacted repeatedly keeps one continuous account of itself instead of
    /// losing everything older than the last cut.
    pub fn prompt(&self, previous_summary: Option<&str>) -> String {
        let mut prompt = String::from(
            "Summarize the transcript below so that work on this session can continue \
             without it. Record decisions taken, files and commands touched, facts \
             established, and anything still open. Write prose, not dialogue, and do \
             not answer or continue the conversation.\n",
        );

        if let Some(previous) = previous_summary {
            prompt.push_str(
                "\nAn earlier stretch of this session was already summarized. Fold the \
                 summary below into your answer so the result covers the session from \
                 its start.\n\n<previous_summary>\n",
            );
            prompt.push_str(previous);
            prompt.push_str("\n</previous_summary>\n");
        }

        prompt.push_str("\n<transcript>\n");
        prompt.push_str(&self.transcript);
        prompt.push_str("\n</transcript>\n");
        prompt
    }
}

/// Chooses where a history can be cut, and serializes what falls before the cut.
///
/// The cut is the first point at or after the budget boundary that leaves every
/// tool call together with its result. Moving it later only shrinks the kept
/// tail, so the budget is never exceeded by the correction.
pub fn plan_compaction(
    messages: &[Message],
    budget: CompactionBudget,
) -> Result<CompactionPlan, CompactionError> {
    let pinned = messages
        .iter()
        .take_while(|message| message.role == Role::System)
        .count();
    let body = &messages[pinned..];
    if body.is_empty() {
        return Err(CompactionError::NothingToCompact);
    }

    let candidate = budget_boundary(body, budget.keep_recent_tokens);
    if candidate == 0 {
        return Err(CompactionError::NothingToCompact);
    }

    let cut = next_balanced_boundary(body, candidate).ok_or(CompactionError::NoValidCut)?;
    if cut == 0 {
        return Err(CompactionError::NothingToCompact);
    }

    Ok(CompactionPlan {
        pinned,
        first_kept: pinned + cut,
        transcript: serialize_transcript(&body[..cut]),
    })
}

/// The earliest index whose suffix still fits the tail budget.
fn budget_boundary(messages: &[Message], keep_recent_tokens: usize) -> usize {
    let mut kept = 0;
    for (index, message) in messages.iter().enumerate().rev() {
        kept += message_tokens(message);
        if kept > keep_recent_tokens {
            return index + 1;
        }
    }
    0
}

/// The first index at or after `from` where no tool call is still awaiting its
/// result, or `None` when no such index exists.
///
/// `messages.len()` counts as a boundary: a history whose whole tail is one
/// unbroken tool block compacts to the summary alone rather than refusing.
fn next_balanced_boundary(messages: &[Message], from: usize) -> Option<usize> {
    let mut open = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if index >= from && open.is_empty() {
            return Some(index);
        }
        for part in &message.parts {
            match part {
                MessagePart::ToolCall { id, .. } => open.push(id.clone()),
                MessagePart::ToolResult { tool_call_id, .. } => {
                    open.retain(|id| id != tool_call_id);
                }
                _ => {}
            }
        }
    }

    open.is_empty().then_some(messages.len())
}

/// Renders messages as a flat transcript.
///
/// Not a message list: handed back as messages, a model reads the stretch as a
/// conversation it is expected to continue rather than a document it is
/// expected to summarize.
pub fn serialize_transcript(messages: &[Message]) -> String {
    let mut transcript = String::new();
    for message in messages {
        transcript.push('[');
        transcript.push_str(role_label(message.role));
        transcript.push_str("]\n");
        for part in &message.parts {
            match part {
                MessagePart::Text(text) => {
                    transcript.push_str(text);
                    transcript.push('\n');
                }
                MessagePart::Reasoning(text) => {
                    transcript.push_str("(reasoning) ");
                    transcript.push_str(text);
                    transcript.push('\n');
                }
                MessagePart::ToolCall { name, input, .. } => {
                    transcript.push_str("call ");
                    transcript.push_str(name);
                    transcript.push('(');
                    transcript.push_str(&bounded_tool_output(input));
                    transcript.push_str(")\n");
                }
                MessagePart::ToolResult {
                    content, is_error, ..
                } => {
                    transcript.push_str(if *is_error { "error " } else { "result " });
                    transcript.push_str(&bounded_tool_output(content));
                    transcript.push('\n');
                }
                MessagePart::Media { mime, .. } => {
                    transcript.push_str("media ");
                    transcript.push_str(mime);
                    transcript.push('\n');
                }
            }
        }
        transcript.push('\n');
    }
    transcript
}

fn bounded_tool_output(value: &str) -> String {
    if value.len() <= TOOL_OUTPUT_MAX_CHARS {
        return value.to_owned();
    }

    let mut end = TOOL_OUTPUT_MAX_CHARS;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} bytes omitted]", &value[..end], value.len() - end)
}

const fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::Supervisor => "supervisor",
    }
}

/// Builds the compacted history: the pinned head, the summary, and the tail.
///
/// Takes the history by reference and returns a new one, so a caller that
/// decides against the result — or that never got a summary — keeps what it
/// had.
pub fn apply_compaction(
    messages: &[Message],
    plan: &CompactionPlan,
    summary: &CompactionSummary,
) -> Vec<Message> {
    let mut compacted = Vec::with_capacity(messages.len() - plan.first_kept + plan.pinned + 1);
    compacted.extend_from_slice(&messages[..plan.pinned]);
    compacted.push(Message {
        role: Role::System,
        parts: vec![MessagePart::Text(format!(
            "Summary of the earlier part of this session:\n\n{}",
            summary.as_str()
        ))],
    });
    compacted.extend_from_slice(&messages[plan.first_kept..]);
    compacted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(role: Role, body: &str) -> Message {
        Message {
            role,
            parts: vec![MessagePart::Text(body.to_owned())],
        }
    }

    fn call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::ToolCall {
                id: id.to_owned(),
                name: "read".to_owned(),
                input: "{\"path\":\"a\"}".to_owned(),
            }],
        }
    }

    fn result(id: &str, content: &str) -> Message {
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: id.to_owned(),
                content: content.to_owned(),
                is_error: false,
            }],
        }
    }

    fn tiny_budget() -> CompactionBudget {
        CompactionBudget {
            keep_recent_tokens: 1,
        }
    }

    #[test]
    fn a_cut_never_separates_a_tool_call_from_its_result() {
        let messages = vec![
            text(Role::User, "first"),
            call("call-1"),
            result("call-1", "body"),
            text(Role::Assistant, "answer"),
            text(Role::User, "second"),
        ];

        for keep in 0..40 {
            let budget = CompactionBudget {
                keep_recent_tokens: keep,
            };
            let Ok(plan) = plan_compaction(&messages, budget) else {
                continue;
            };
            let summary = CompactionSummary::new("summary").expect("summary is non-empty");
            let compacted = apply_compaction(&messages, &plan, &summary);

            let calls: Vec<&String> = compacted
                .iter()
                .flat_map(|message| &message.parts)
                .filter_map(|part| match part {
                    MessagePart::ToolCall { id, .. } => Some(id),
                    _ => None,
                })
                .collect();
            let results: Vec<&String> = compacted
                .iter()
                .flat_map(|message| &message.parts)
                .filter_map(|part| match part {
                    MessagePart::ToolResult { tool_call_id, .. } => Some(tool_call_id),
                    _ => None,
                })
                .collect();
            assert_eq!(calls, results, "orphaned tool traffic at keep={keep}");
        }
    }

    #[test]
    fn a_cut_that_would_land_inside_a_tool_block_moves_past_it() {
        let messages = vec![
            text(Role::User, "first"),
            call("call-1"),
            result("call-1", "body"),
            text(Role::User, "second"),
        ];

        // The budget alone puts the cut at the tool result, which would leave
        // the kept tail holding a result whose call was summarized away.
        let budget = CompactionBudget {
            keep_recent_tokens: 5,
        };
        let plan = plan_compaction(&messages, budget).expect("history can be compacted");

        assert_eq!(plan.first_kept(), 3);
        assert!(plan.transcript().contains("call read"));
    }

    #[test]
    fn a_tool_call_still_awaiting_its_result_refuses_the_cut() {
        let messages = vec![text(Role::User, "first"), call("call-1")];

        assert_eq!(
            plan_compaction(&messages, tiny_budget()),
            Err(CompactionError::NoValidCut)
        );
    }

    #[test]
    fn leading_system_messages_are_never_summarized() {
        let messages = vec![
            text(Role::System, "instructions"),
            text(Role::User, "first"),
            text(Role::User, "second"),
        ];

        let plan = plan_compaction(&messages, tiny_budget()).expect("history can be compacted");
        assert_eq!(plan.pinned(), 1);
        assert!(!plan.transcript().contains("instructions"));

        let summary = CompactionSummary::new("summary").expect("summary is non-empty");
        let compacted = apply_compaction(&messages, &plan, &summary);
        assert_eq!(compacted[0], messages[0]);
    }

    #[test]
    fn a_history_inside_the_budget_is_left_alone() {
        let messages = vec![text(Role::User, "first"), text(Role::Assistant, "answer")];

        assert_eq!(
            plan_compaction(&messages, CompactionBudget::default()),
            Err(CompactionError::NothingToCompact)
        );
    }

    #[test]
    fn an_empty_summary_is_refused_before_anything_is_replaced() {
        assert_eq!(
            CompactionSummary::new("   \n "),
            Err(CompactionError::EmptySummary)
        );
    }

    #[test]
    fn a_transcript_bounds_tool_output_and_keeps_its_own_shape() {
        let body = "x".repeat(TOOL_OUTPUT_MAX_CHARS * 2);
        let transcript = serialize_transcript(&[result("call-1", &body)]);

        assert!(transcript.starts_with("[tool]\n"));
        assert!(transcript.contains("bytes omitted"));
        assert!(transcript.len() < body.len());
    }

    #[test]
    fn a_repeated_compaction_folds_the_previous_summary_into_the_prompt() {
        let messages = vec![
            text(Role::User, "first"),
            text(Role::Assistant, "answer"),
            text(Role::User, "second"),
        ];

        let plan = plan_compaction(&messages, tiny_budget()).expect("history can be compacted");
        let prompt = plan.prompt(Some("what came before"));

        assert!(prompt.contains("what came before"));
        assert!(prompt.contains("<transcript>"));
    }
}

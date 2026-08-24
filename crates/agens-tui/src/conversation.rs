//! Typed, lossless source projection for one visible conversation turn.

use std::time::Duration;

use crate::bridge::SubagentErrorPresentation;
use agens_core::redaction::redact_credential_values;
use agens_core::{
    DiffLine, Message, MessagePart, Role, SubagentStatus, ToolInput, media_chip_label,
};
use agens_core::{TuiExecutionState, TuiSubagentEvent, TuiSubagentUpdate};

/// A source event accepted by the conversation projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationEvent {
    Info(String),
    /// Runtime context the reader must not be able to mistake for prose the
    /// turn produced, because it reports something the runtime could not do.
    FailureNotice(String),
    MarkdownDelta(String),
    MarkdownFinal(String),
    ReasoningDelta(String),
    ToolCall {
        call_id: String,
        name: String,
        input: String,
        parsed: ToolInput,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    /// The edit whose result this diff describes, and the diff itself.
    ///
    /// The call id travels with the lines because the diff alone cannot say
    /// what file it belongs to, and a diff with no file has no language.
    Diff {
        call_id: String,
        lines: Vec<DiffLine>,
    },
    Error {
        message: String,
        action: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionableError {
    pub message: String,
    pub action: String,
}

impl ActionableError {
    fn sanitized(message: String, action: String) -> Self {
        Self {
            message: sanitize_error_message(message),
            action,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub input: String,
    pub parsed: ToolInput,
    pub result: Option<ToolResult>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolBatch {
    pub calls: Vec<ToolCall>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentCard {
    pub id: u64,
    pub agent: String,
    pub task_summary: String,
    pub presentation: TuiExecutionState,
    /// What the subagent runs on, when the runtime reported it.
    pub model: Option<String>,
    pub effort: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_uses: usize,
    pub activities: Vec<String>,
    pub status: Option<SubagentStatus>,
    pub final_result: Option<String>,
    pub started_at: Option<Duration>,
    pub terminal_at: Option<Duration>,
    pub has_activity: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationError {
    OrphanToolResult(String),
    DuplicateToolCall(String),
    DuplicateToolResult(String),
    InvalidMessageOrder,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ConversationItem {
    Info(String),
    FailureNotice(String),
    User(String),
    Assistant(String),
    Reasoning(String),
    ToolCall {
        call_id: String,
        name: String,
        input: String,
        parsed: ToolInput,
        batch: Option<usize>,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    Diff {
        call_id: String,
        lines: Vec<DiffLine>,
    },
    Error(ActionableError),
    SubagentCard(u64),
}

/// What one turn cost: wall time, and the tokens its provider rounds billed.
///
/// The two token figures a turn can report, each aggregated the only way it
/// means anything.
///
/// A turn that runs tools bills one usage report per round. Output is new text
/// every round, so summing it answers how much the turn produced. Input is not:
/// every round resends the whole conversation, so round N's prompt already
/// contains rounds 1..N-1 and summing counts the same tokens once per round.
/// That is how a seven-minute turn over a 130k context came to claim 2.6M
/// tokens in. The prompt figure is therefore the high-water mark — how large
/// the conversation actually grew — and is named for what it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnCost {
    pub duration: Option<Duration>,
    /// Largest prompt the turn sent, which is the size the conversation reached.
    pub context_tokens: Option<u64>,
    /// Every token the turn generated, summed across its rounds.
    pub output_tokens: Option<u64>,
}

impl TurnCost {
    pub(crate) const fn is_empty(self) -> bool {
        self.duration.is_none() && self.context_tokens.is_none() && self.output_tokens.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub user: String,
    pub info: Vec<String>,
    pub live_markdown: String,
    pub final_markdown: Option<String>,
    pub reasoning: String,
    pub tool_batches: Vec<ToolBatch>,
    pub diffs: Vec<DiffLine>,
    pub errors: Vec<ActionableError>,
    pub subagent_cards: Vec<SubagentCard>,
    pub(super) items: Vec<ConversationItem>,
    /// What the turn cost, known only once it settles.
    pub cost: TurnCost,
    last_was_tool_call: bool,
    settled: bool,
}

impl Conversation {
    pub fn new(user: impl Into<String>) -> Self {
        let user = user.into();
        Self {
            items: (!user.is_empty())
                .then(|| ConversationItem::User(user.clone()))
                .into_iter()
                .collect(),
            user,
            info: Vec::new(),
            live_markdown: String::new(),
            final_markdown: None,
            reasoning: String::new(),
            tool_batches: Vec::new(),
            diffs: Vec::new(),
            errors: Vec::new(),
            subagent_cards: Vec::new(),
            cost: TurnCost::default(),
            last_was_tool_call: false,
            settled: false,
        }
    }

    pub(crate) fn new_with_media<'a>(
        user: impl Into<String>,
        media_mimes: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let mut conversation = Self::new(user);

        for (index, mime) in media_mimes.into_iter().enumerate() {
            let chip = media_chip_label(index + 1, mime);
            if !conversation.user.is_empty() {
                conversation.user.push(' ');
            }
            conversation.user.push_str(&chip);
        }
        if !conversation.user.is_empty() {
            conversation.items = vec![ConversationItem::User(conversation.user.clone())];
        }

        conversation
    }
    /// Reconstructs completed conversations from persisted messages.
    ///
    /// Restored tool calls degrade `parsed` to [`ToolInput::Other`] since no
    /// authoritative parser is available at this crate boundary. Use
    /// [`Self::from_messages_with_parser`] when an accurate `parsed` value is
    /// required (the production restore path in `agens-cli`).
    pub fn from_messages(messages: &[Message]) -> Result<Vec<Self>, ConversationError> {
        Self::from_messages_with_parser(messages, |name, input| ToolInput::Other {
            name: name.to_owned(),
            raw: input.to_owned(),
        })
    }

    /// Reconstructs completed conversations from persisted messages, deriving
    /// `parsed` for each restored tool call via `parse_tool_input`.
    ///
    /// Restored tool call names are qualified (e.g. `native::read`), unlike
    /// the bare names carried by the live event stream; callers that reuse a
    /// bare-name parser must strip the `native::`/`mcp::` prefix themselves.
    ///
    /// **Restore never discards history.** Live event apply is strict (orphan
    /// tool results, wrong role order). Persisted sessions can still carry
    /// incomplete or reordered rows after failed turns; this path best-effort
    /// projects every part it can so resume always shows what was saved.
    pub fn from_messages_with_parser(
        messages: &[Message],
        parse_tool_input: impl Fn(&str, &str) -> ToolInput,
    ) -> Result<Vec<Self>, ConversationError> {
        Ok(Self::from_messages_best_effort(messages, parse_tool_input))
    }

    /// Always succeeds: projects every recoverable part of `messages` into
    /// settled conversations. Used by resume so a single bad row cannot wipe
    /// the entire transcript.
    fn from_messages_best_effort(
        messages: &[Message],
        parse_tool_input: impl Fn(&str, &str) -> ToolInput,
    ) -> Vec<Self> {
        let mut conversations = Vec::new();
        let mut current: Option<Self> = None;
        let mut pending_system = Vec::new();

        let ensure_current = |current: &mut Option<Self>, pending_system: &mut Vec<String>| {
            if current.is_none() {
                let mut conversation = Self::new(String::new());
                conversation.settled = true;
                for message in pending_system.drain(..) {
                    let _ = conversation.apply(ConversationEvent::Info(message));
                }
                *current = Some(conversation);
            }
        };

        for message in messages {
            match message.role {
                Role::System => {
                    if let Some(conversation) = current.take() {
                        conversations.push(conversation);
                    }
                    for part in &message.parts {
                        if let MessagePart::Text(text) = part {
                            pending_system.push(text.clone());
                        }
                    }
                }
                // Inside the turn it steered, never starting a new one: a
                // supervisor message arrived while that turn was running, and
                // showing it as a fresh exchange would claim the person spoke.
                Role::Supervisor => {
                    ensure_current(&mut current, &mut pending_system);
                    let conversation = current.as_mut().expect("ensure_current");
                    for part in &message.parts {
                        if let MessagePart::Text(text) = part {
                            let _ = conversation
                                .apply(ConversationEvent::Info(format!("supervisor: {text}")));
                        }
                    }
                }
                Role::User => {
                    if let Some(conversation) = current.take() {
                        conversations.push(conversation);
                    }
                    let mut conversation = Self::new(String::new());
                    conversation.settled = true;
                    for message in pending_system.drain(..) {
                        let _ = conversation.apply(ConversationEvent::Info(message));
                    }
                    let mut media_ordinal = 0_usize;
                    for part in &message.parts {
                        match part {
                            MessagePart::Text(text) => {
                                if let Some(notice) = crate::runtime_scheduled_notice(text) {
                                    let _ = conversation.apply(ConversationEvent::Info(notice));
                                    continue;
                                }
                                conversation.user.push_str(text);
                            }
                            MessagePart::Media { mime, .. } => {
                                media_ordinal += 1;
                                let chip = media_chip_label(media_ordinal, mime);
                                if !conversation.user.is_empty() {
                                    conversation.user.push(' ');
                                }
                                conversation.user.push_str(&chip);
                            }
                            // Unexpected roles on a user row: keep text surface via info.
                            MessagePart::Reasoning(text) => {
                                let _ = conversation
                                    .apply(ConversationEvent::ReasoningDelta(text.clone()));
                            }
                            MessagePart::ToolCall { id, name, input } => {
                                let _ = conversation.apply(ConversationEvent::ToolCall {
                                    call_id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                    parsed: parse_tool_input(name, input),
                                });
                            }
                            MessagePart::ToolResult {
                                tool_call_id,
                                content,
                                is_error,
                            } => {
                                apply_restored_tool_result(
                                    &mut conversation,
                                    tool_call_id,
                                    content,
                                    *is_error,
                                    &parse_tool_input,
                                );
                            }
                        }
                    }
                    if !conversation.user.is_empty() {
                        conversation
                            .items
                            .push(ConversationItem::User(conversation.user.clone()));
                    }
                    current = Some(conversation);
                }
                Role::Assistant => {
                    ensure_current(&mut current, &mut pending_system);
                    let conversation = current.as_mut().expect("ensure_current");
                    for part in &message.parts {
                        match part {
                            MessagePart::Text(text) => {
                                let _ = conversation
                                    .apply(ConversationEvent::MarkdownDelta(text.clone()));
                            }
                            MessagePart::Reasoning(text) => {
                                let _ = conversation
                                    .apply(ConversationEvent::ReasoningDelta(text.clone()));
                            }
                            MessagePart::ToolCall { id, name, input } => {
                                let mut call_id = id.clone();
                                if conversation.has_call(&call_id) {
                                    // Duplicate ids across a long session must not drop the call.
                                    call_id = format!("{call_id}#{}", conversation.items.len());
                                }
                                let _ = conversation.apply(ConversationEvent::ToolCall {
                                    call_id,
                                    name: name.clone(),
                                    input: input.clone(),
                                    parsed: parse_tool_input(name, input),
                                });
                            }
                            MessagePart::ToolResult {
                                tool_call_id,
                                content,
                                is_error,
                            } => {
                                apply_restored_tool_result(
                                    conversation,
                                    tool_call_id,
                                    content,
                                    *is_error,
                                    &parse_tool_input,
                                );
                            }
                            MessagePart::Media { mime, .. } => {
                                let _ = conversation.apply(ConversationEvent::Info(format!(
                                    "[restored media: {mime}]"
                                )));
                            }
                        }
                    }
                }
                Role::Tool => {
                    ensure_current(&mut current, &mut pending_system);
                    let conversation = current.as_mut().expect("ensure_current");
                    for part in &message.parts {
                        if let MessagePart::ToolResult {
                            tool_call_id,
                            content,
                            is_error,
                        } = part
                        {
                            apply_restored_tool_result(
                                conversation,
                                tool_call_id,
                                content,
                                *is_error,
                                &parse_tool_input,
                            );
                        } else if let MessagePart::Text(text) = part {
                            let _ =
                                conversation.apply(ConversationEvent::MarkdownDelta(text.clone()));
                        }
                    }
                }
            }
        }

        if !pending_system.is_empty() {
            ensure_current(&mut current, &mut pending_system);
        }
        if let Some(conversation) = current {
            conversations.push(conversation);
        }
        conversations
    }

    pub(crate) const fn is_settled(&self) -> bool {
        self.settled
    }

    pub(crate) const fn mark_settled(&mut self) {
        self.settled = true;
    }

    pub fn apply(&mut self, event: ConversationEvent) -> Result<(), ConversationError> {
        let is_tool_call = matches!(&event, ConversationEvent::ToolCall { .. });
        match event {
            ConversationEvent::Info(message) => {
                self.info.push(message.clone());
                self.items.push(ConversationItem::Info(message));
            }
            ConversationEvent::FailureNotice(message) => {
                self.info.push(message.clone());
                self.items.push(ConversationItem::FailureNotice(message));
            }
            ConversationEvent::MarkdownDelta(delta) => {
                self.live_markdown.push_str(&delta);
                push_text_item(&mut self.items, delta, false);
            }
            ConversationEvent::MarkdownFinal(markdown) => {
                self.final_markdown = Some(markdown.clone());
                self.items
                    .retain(|item| !matches!(item, ConversationItem::Assistant(_)));
                self.items.push(ConversationItem::Assistant(markdown));
            }
            ConversationEvent::ReasoningDelta(delta) => {
                self.reasoning.push_str(&delta);
                push_text_item(&mut self.items, delta, true);
            }
            ConversationEvent::ToolCall {
                call_id,
                name,
                input,
                parsed,
            } => {
                if self.find_call(&call_id).is_some() {
                    return Err(ConversationError::DuplicateToolCall(call_id));
                }
                let batch = if !self.last_was_tool_call {
                    self.tool_batches.push(ToolBatch::default());
                    Some(self.tool_batches.len())
                } else {
                    None
                };
                self.tool_batches
                    .last_mut()
                    .expect("tool batch was created")
                    .calls
                    .push(ToolCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        parsed: parsed.clone(),
                        result: None,
                    });
                self.items.push(ConversationItem::ToolCall {
                    call_id,
                    name,
                    input,
                    parsed,
                    batch,
                });
            }
            ConversationEvent::ToolResult {
                call_id,
                output,
                is_error,
            } => {
                let Some(call) = self.find_call_mut(&call_id) else {
                    return Err(ConversationError::OrphanToolResult(call_id));
                };
                if call.result.is_some() {
                    return Err(ConversationError::DuplicateToolResult(call_id));
                }
                call.result = Some(ToolResult {
                    output: output.clone(),
                    is_error,
                });
                // Pair each result with its call in the transcript, even when
                // results arrive after a whole parallel batch of calls. Appending
                // would paint every header and then every body; inserting after
                // the call paints call₁, body₁, call₂, body₂.
                let item = ConversationItem::ToolResult {
                    call_id: call_id.clone(),
                    output,
                    is_error,
                };
                self.insert_after_tool_call(&call_id, item);
            }
            ConversationEvent::Diff { call_id, lines } => {
                self.diffs.extend(lines.clone());
                // Keep diffs with their call/result block, not after later tools
                // in the same batch.
                let item = ConversationItem::Diff {
                    call_id: call_id.clone(),
                    lines,
                };
                self.insert_tool_follow_up(&call_id, item);
            }
            ConversationEvent::Error { message, action } => {
                let error = ActionableError::sanitized(message, action);
                self.errors.push(error.clone());
                self.items.push(ConversationItem::Error(error));
            }
        }
        self.last_was_tool_call = is_tool_call;
        Ok(())
    }
    pub(crate) fn apply_subagent_summary(&mut self, event: TuiSubagentEvent, now: Duration) {
        match event.update {
            TuiSubagentUpdate::Started {
                agent,
                task_summary,
                presentation,
                model,
                effort,
            } if self.subagent_cards.iter().all(|card| card.id != event.id) => {
                self.subagent_cards.push(SubagentCard {
                    id: event.id,
                    agent,
                    task_summary,
                    presentation,
                    model,
                    effort,
                    tool_calls: Vec::new(),
                    tool_uses: 0,
                    activities: Vec::new(),
                    status: None,
                    final_result: None,
                    started_at: Some(now),
                    terminal_at: None,
                    has_activity: false,
                });
                self.items.push(ConversationItem::SubagentCard(event.id));
            }
            TuiSubagentUpdate::ToolCall { name, .. } => {
                if let Some(card) = self
                    .subagent_cards
                    .iter_mut()
                    .find(|card| card.id == event.id && card.status.is_none())
                {
                    card.tool_uses += 1;
                    card.has_activity = true;
                    if card.activities.len() < 3
                        && let Some(activity) = subagent_activity(&name)
                    {
                        card.activities.push(activity.into());
                    }
                }
            }
            TuiSubagentUpdate::Reasoning(_)
            | TuiSubagentUpdate::Text(_)
            | TuiSubagentUpdate::ToolResult { .. }
            | TuiSubagentUpdate::Error { .. } => {
                if let Some(card) = self
                    .subagent_cards
                    .iter_mut()
                    .find(|card| card.id == event.id && card.status.is_none())
                {
                    card.has_activity = true;
                }
            }
            TuiSubagentUpdate::Terminal {
                status,
                final_result,
            } => {
                if let Some(card) = self
                    .subagent_cards
                    .iter_mut()
                    .find(|card| card.id == event.id && card.status.is_none())
                {
                    card.status = Some(status);
                    card.final_result = Some(final_result);
                    card.terminal_at = Some(now);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn apply_child_event(&mut self, event: TuiSubagentEvent) {
        let result = match event.update {
            TuiSubagentUpdate::Started { .. } => return,
            TuiSubagentUpdate::Reasoning(delta) => {
                self.apply(ConversationEvent::ReasoningDelta(delta))
            }
            TuiSubagentUpdate::Text(delta) => self.apply(ConversationEvent::MarkdownDelta(delta)),
            TuiSubagentUpdate::ToolCall {
                call_id,
                name,
                input,
                parsed,
            } => self.apply(ConversationEvent::ToolCall {
                call_id,
                name,
                input,
                parsed,
            }),
            TuiSubagentUpdate::ToolResult {
                call_id,
                output,
                is_error,
            } => self.apply(ConversationEvent::ToolResult {
                call_id,
                output,
                is_error,
            }),
            TuiSubagentUpdate::Error { kind, reference } => self.apply(ConversationEvent::Error {
                message: reference.map_or_else(
                    || kind.message().into(),
                    |reference| format!("{} [ref: {reference}]", kind.message()),
                ),
                action: kind.action().into(),
            }),
            TuiSubagentUpdate::Terminal { final_result, .. } => {
                self.final_markdown = Some(final_result.clone());
                self.items.push(ConversationItem::Assistant(final_result));
                Ok(())
            }
        };

        if let Err(error) = result {
            self.record_projection_error(&error);
        }
    }

    /// Surfaces a child event the projection refused instead of dropping it.
    ///
    /// A refusal means the child's stream is inconsistent and the row that
    /// event would have produced is gone for good. Showing a silently shorter
    /// transcript would let the reader trust a record that is missing a step,
    /// so the gap is stated where the step would have been.
    fn record_projection_error(&mut self, error: &ConversationError) {
        let message = match error {
            ConversationError::OrphanToolResult(call_id) => {
                format!("Subagent tool result {call_id} arrived without its call and was dropped.")
            }
            ConversationError::DuplicateToolCall(call_id) => {
                format!("Subagent tool call {call_id} arrived twice; the repeat was dropped.")
            }
            ConversationError::DuplicateToolResult(call_id) => {
                format!("Subagent tool result {call_id} arrived twice; the repeat was dropped.")
            }
            ConversationError::InvalidMessageOrder => {
                "A subagent event arrived out of order and was dropped.".to_owned()
            }
        };

        let actionable = ActionableError::sanitized(
            message,
            "Re-run the delegation if the dropped step matters.".into(),
        );
        self.errors.push(actionable.clone());
        self.items.push(ConversationItem::Error(actionable));
    }

    pub(crate) fn restore_completed_subagent(
        &mut self,
        id: u64,
        agent: String,
        task_summary: String,
        final_result: String,
        tool_uses: usize,
    ) {
        if self.subagent_cards.iter().any(|card| card.id == id) {
            return;
        }

        self.subagent_cards.push(SubagentCard {
            id,
            agent,
            task_summary,
            presentation: TuiExecutionState::CompletedRecent,
            // A restored card carries only what history recorded, and the
            // model a finished subagent ran on was never persisted.
            model: None,
            effort: None,
            tool_calls: Vec::new(),
            tool_uses,
            activities: Vec::new(),
            status: Some(SubagentStatus::Success),
            final_result: Some(final_result),
            started_at: None,
            terminal_at: None,
            has_activity: true,
        });
        self.items.push(ConversationItem::SubagentCard(id));
    }
    /// Corrects a live tool call's `parsed` value once the typed
    /// `TuiRuntimeEvent::ToolStarted` carrier arrives for `call_id`.
    ///
    /// The live projection path (raw `TurnEvent`) has no parser available, so
    /// it records a placeholder `ToolInput::Other` at call time; this is the
    /// single point that ever writes an authoritative `parsed` value for a
    /// live call.
    pub(crate) fn enrich_parsed_tool_input(&mut self, call_id: &str, parsed: ToolInput) {
        if let Some(call) = self.find_call_mut(call_id) {
            call.parsed = parsed.clone();
        }
        for item in &mut self.items {
            if let ConversationItem::ToolCall {
                call_id: id,
                parsed: slot,
                ..
            } = item
                && id == call_id
            {
                *slot = parsed;
                break;
            }
        }
    }

    /// Drops a projected error by exact message.
    ///
    /// A placeholder recorded for an unexplained failure has to give way to the
    /// cause that later explains it, instead of standing beside it as a second
    /// error for the same failure.
    pub(crate) fn remove_error(&mut self, message: &str) {
        if let Some(index) = self
            .errors
            .iter()
            .rposition(|error| error.message == message)
        {
            self.errors.remove(index);
        }
        if let Some(index) = self.items.iter().rposition(
            |item| matches!(item, ConversationItem::Error(error) if error.message == message),
        ) {
            self.items.remove(index);
        }
    }

    fn find_call(&self, call_id: &str) -> Option<&ToolCall> {
        self.tool_batches
            .iter()
            .flat_map(|batch| &batch.calls)
            .find(|call| call.call_id == call_id)
    }

    fn has_call(&self, call_id: &str) -> bool {
        self.find_call(call_id).is_some()
    }

    fn find_call_mut(&mut self, call_id: &str) -> Option<&mut ToolCall> {
        self.tool_batches
            .iter_mut()
            .flat_map(|batch| &mut batch.calls)
            .find(|call| call.call_id == call_id)
    }

    /// Index of the transcript item that opened `call_id`, if any.
    fn tool_call_item_index(&self, call_id: &str) -> Option<usize> {
        self.items.iter().position(|item| {
            matches!(
                item,
                ConversationItem::ToolCall {
                    call_id: id,
                    ..
                } if id == call_id
            )
        })
    }

    /// Places a result immediately after its call header.
    ///
    /// Any diffs already parked after the call stay after the result so the
    /// reader still sees header → body → patch for one tool before the next
    /// tool in the batch.
    fn insert_after_tool_call(&mut self, call_id: &str, item: ConversationItem) {
        let Some(call_pos) = self.tool_call_item_index(call_id) else {
            // The call was found in the batch table before this ran; a missing
            // item would mean the two projections diverged, which is a logic
            // bug. Keep the transcript complete rather than dropping the body.
            self.items.push(item);
            return;
        };
        self.items.insert(call_pos + 1, item);
    }

    /// Places a diff after the call and any result (or earlier diffs) for that
    /// same call, so parallel batch tools do not sandwich a patch under a
    /// later command's header.
    fn insert_tool_follow_up(&mut self, call_id: &str, item: ConversationItem) {
        let Some(call_pos) = self.tool_call_item_index(call_id) else {
            self.items.push(item);
            return;
        };

        let mut insert_at = call_pos + 1;
        while insert_at < self.items.len() {
            let same_call = match &self.items[insert_at] {
                ConversationItem::ToolResult { call_id: id, .. }
                | ConversationItem::Diff { call_id: id, .. } => id == call_id,
                _ => false,
            };
            if !same_call {
                break;
            }
            insert_at += 1;
        }
        self.items.insert(insert_at, item);
    }
}

pub(crate) fn subagent_activity(name: &str) -> Option<&'static str> {
    match name.strip_prefix("native::").unwrap_or(name) {
        "read" => Some("Read files"),
        "grep" | "search" => Some("Search code"),
        "glob" | "list" => Some("List files"),
        "edit" | "write" => Some("Edit files"),
        "bash" => Some("Run command"),
        "skill" => Some("Load skill"),
        _ => None,
    }
}

/// Applies a restored tool result, synthesizing a placeholder call when the
/// matching call is missing so failed/partial sessions still show their output.
fn apply_restored_tool_result(
    conversation: &mut Conversation,
    tool_call_id: &str,
    content: &str,
    is_error: bool,
    parse_tool_input: &impl Fn(&str, &str) -> ToolInput,
) {
    if !conversation.has_call(tool_call_id) {
        let name = "restored";
        let input = "{}";
        let _ = conversation.apply(ConversationEvent::ToolCall {
            call_id: tool_call_id.to_owned(),
            name: name.to_owned(),
            input: input.to_owned(),
            parsed: parse_tool_input(name, input),
        });
    }
    if conversation
        .find_call(tool_call_id)
        .is_some_and(|call| call.result.is_some())
    {
        return;
    }
    let _ = conversation.apply(ConversationEvent::ToolResult {
        call_id: tool_call_id.to_owned(),
        output: content.to_owned(),
        is_error,
    });
}

fn push_text_item(items: &mut Vec<ConversationItem>, text: String, reasoning: bool) {
    match (items.last_mut(), reasoning) {
        (Some(ConversationItem::Reasoning(current)), true)
        | (Some(ConversationItem::Assistant(current)), false) => current.push_str(&text),
        (_, true) => items.push(ConversationItem::Reasoning(text)),
        (_, false) => items.push(ConversationItem::Assistant(text)),
    }
}

/// Withholds credential-shaped values from a runtime error message before it reaches the TUI
/// card. This sink is user-visible-only, so unlike a model-visible sink it keeps host paths —
/// only [`redact_credential_values`] runs here, never [`agens_core::redaction::redact_absolute_paths`].
fn sanitize_error_message(message: String) -> String {
    redact_credential_values(&message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agens_core::ToolInput;

    fn tool_call(id: &str) -> ConversationEvent {
        ConversationEvent::ToolCall {
            call_id: id.into(),
            name: id.into(),
            input: id.into(),
            parsed: ToolInput::Other {
                name: id.into(),
                raw: id.into(),
            },
        }
    }

    fn tool_result(id: &str, output: &str) -> ConversationEvent {
        ConversationEvent::ToolResult {
            call_id: id.into(),
            output: output.into(),
            is_error: false,
        }
    }

    #[test]
    fn restore_keeps_orphan_assistant_and_tool_output_without_failing() {
        use agens_core::{Message, MessagePart, Role};

        let messages = vec![
            Message {
                role: Role::Assistant,
                parts: vec![MessagePart::Text("no user row above me".into())],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "missing-call".into(),
                    content: "still visible output".into(),
                    is_error: true,
                }],
            },
        ];

        let restored = Conversation::from_messages(&messages).expect("restore always Ok");
        assert_eq!(restored.len(), 1);
        assert!(
            restored[0].live_markdown.contains("no user row above me"),
            "assistant text kept: {}",
            restored[0].live_markdown
        );
        assert!(
            restored[0]
                .tool_batches
                .iter()
                .any(|batch| batch.calls.iter().any(|call| call
                    .result
                    .as_ref()
                    .is_some_and(|result| { result.output.contains("still visible output") }))),
            "orphan tool result kept under a synthetic call"
        );
    }

    #[test]
    fn live_and_restored_text_with_media_project_as_one_user_block() {
        let live = Conversation::new_with_media("look", ["image/png"]);
        let restored = Conversation::from_messages(&[Message {
            role: Role::User,
            parts: vec![
                MessagePart::Text("look".into()),
                MessagePart::Media {
                    media_id: 7,
                    mime: "image/png".into(),
                },
            ],
        }])
        .expect("text and media should restore")
        .pop()
        .expect("one restored conversation");

        for conversation in [&live, &restored] {
            let user_blocks = conversation
                .items
                .iter()
                .filter_map(|item| match item {
                    ConversationItem::User(content) => Some(content.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(user_blocks, ["look [Image #1]"]);
            assert_eq!(conversation.user, "look [Image #1]");
        }
    }

    /// Parallel batches announce every call before any result. The transcript
    /// item stream must still pair each body under its own header so the
    /// renderer never paints two headers and then two bodies.
    #[test]
    fn tool_results_insert_beside_their_calls_not_after_the_batch() {
        let mut conversation = Conversation::new("inspect");
        for event in [
            tool_call("one"),
            tool_call("two"),
            tool_result("two", "files"),
            tool_result("one", "contents"),
        ] {
            conversation.apply(event).unwrap();
        }

        let kinds: Vec<String> = conversation
            .items
            .iter()
            .filter_map(|item| match item {
                ConversationItem::ToolCall { call_id, .. } => Some(format!("call:{call_id}")),
                ConversationItem::ToolResult { call_id, .. } => Some(format!("result:{call_id}")),
                _ => None,
            })
            .collect();

        assert_eq!(
            kinds,
            [
                "call:one".to_owned(),
                "result:one".to_owned(),
                "call:two".to_owned(),
                "result:two".to_owned(),
            ],
            "out-of-order results still pair under their calls: {kinds:?}"
        );
    }
}

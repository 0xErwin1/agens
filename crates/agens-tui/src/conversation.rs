//! Typed, lossless source projection for one visible conversation turn.

/// A source event accepted by the conversation projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationEvent {
    MarkdownDelta(String),
    MarkdownFinal(String),
    ReasoningDelta(String),
    ToolCall {
        call_id: String,
        name: String,
        input: String,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    Diff(Vec<DiffLine>),
    Error {
        message: String,
        action: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub number: u32,
    pub kind: DiffLineKind,
    pub text: String,
}

impl DiffLine {
    pub fn new(number: u32, kind: DiffLineKind, text: impl Into<String>) -> Self {
        Self {
            number,
            kind,
            text: text.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Added,
    Removed,
    Context,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionableError {
    pub message: String,
    pub action: String,
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
    pub result: Option<ToolResult>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolBatch {
    pub calls: Vec<ToolCall>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationError {
    OrphanToolResult(String),
    DuplicateToolCall(String),
    DuplicateToolResult(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub user: String,
    pub live_markdown: String,
    pub final_markdown: Option<String>,
    pub reasoning: String,
    pub tool_batches: Vec<ToolBatch>,
    pub diffs: Vec<DiffLine>,
    pub errors: Vec<ActionableError>,
    last_was_tool_call: bool,
}

impl Conversation {
    pub fn new(user: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            live_markdown: String::new(),
            final_markdown: None,
            reasoning: String::new(),
            tool_batches: Vec::new(),
            diffs: Vec::new(),
            errors: Vec::new(),
            last_was_tool_call: false,
        }
    }

    pub fn apply(&mut self, event: ConversationEvent) -> Result<(), ConversationError> {
        let is_tool_call = matches!(&event, ConversationEvent::ToolCall { .. });
        match event {
            ConversationEvent::MarkdownDelta(delta) => self.live_markdown.push_str(&delta),
            ConversationEvent::MarkdownFinal(markdown) => self.final_markdown = Some(markdown),
            ConversationEvent::ReasoningDelta(delta) => self.reasoning.push_str(&delta),
            ConversationEvent::ToolCall {
                call_id,
                name,
                input,
            } => {
                if self.find_call(&call_id).is_some() {
                    return Err(ConversationError::DuplicateToolCall(call_id));
                }
                if !self.last_was_tool_call {
                    self.tool_batches.push(ToolBatch::default());
                }
                self.tool_batches
                    .last_mut()
                    .expect("tool batch was created")
                    .calls
                    .push(ToolCall {
                        call_id,
                        name,
                        input,
                        result: None,
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
                call.result = Some(ToolResult { output, is_error });
            }
            ConversationEvent::Diff(lines) => self.diffs.extend(lines),
            ConversationEvent::Error { message, action } => {
                self.errors.push(ActionableError { message, action })
            }
        }
        self.last_was_tool_call = is_tool_call;
        Ok(())
    }

    fn find_call(&self, call_id: &str) -> Option<&ToolCall> {
        self.tool_batches
            .iter()
            .flat_map(|batch| &batch.calls)
            .find(|call| call.call_id == call_id)
    }

    fn find_call_mut(&mut self, call_id: &str) -> Option<&mut ToolCall> {
        self.tool_batches
            .iter_mut()
            .flat_map(|batch| &mut batch.calls)
            .find(|call| call.call_id == call_id)
    }
}

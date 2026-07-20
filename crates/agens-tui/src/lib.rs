//! Terminal lifecycle and input-event boundary for the interactive surface.

mod app;
mod bridge;
mod conversation;
mod render;
mod terminal;

pub use app::{AppEvent, AppState, Command, Dialog, Effect, Runtime};
pub use bridge::{
    BridgeCancel, BridgeTx, PublishOutcome, ToolResultState, TuiRuntimeEvent, UiEnvelope,
};
pub use conversation::{
    ActionableError, Conversation, ConversationError, ConversationEvent, DiffLine, DiffLineKind,
    ToolBatch, ToolCall, ToolResult,
};
pub use terminal::{
    PendingPermissions, PermissionReply, TerminalControl, TerminalModeGuard, TerminalOperation,
    teardown,
};

use std::{
    collections::BTreeSet,
    io::{self, Stdout, Write},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use agens_core::{MessagePart, TurnEvent, TurnState, Usage};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, KeyCode,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{self as crossterm_terminal, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Terminal as RatatuiTerminal,
    backend::Backend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

/// Cancels the active engine turn. The TUI owns no provider or session logic.
pub trait Engine {
    /// Requests cooperative cancellation of the active turn.
    fn cancel(&mut self);
}

/// Input received from the terminal that affects the TUI event loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// A key that participates in normal interaction.
    Key(Key),
    /// A terminal resize in columns and rows.
    Resize { width: u16, height: u16 },
}

/// Keys handled by the TUI engine boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    /// An ordinary input character.
    Char(char),
    /// Deletes the preceding input character.
    Backspace,
    /// Submits the current input.
    Enter,
    /// Cancels an active turn when one exists.
    Escape,
    /// Cancels an active turn, clears input, or arms quitting.
    CtrlC,
    /// Collapses or expands completed tool outputs in the visible conversation.
    CtrlO,
    ShiftEnter,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

/// The result of handling a single terminal event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Render the current view state.
    Render,
    /// Send this prompt to the composition layer.
    Submit(String),
    /// An active engine turn was asked to cancel.
    Cancel,
    /// End the terminal event loop.
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TuiPresentation {
    provider: String,
    model: String,
    session: String,
}

impl TuiPresentation {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        session: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            session: session.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiSubmissionOutcome {
    ProviderTurn {
        prompt: String,
    },
    LocalInfo(String),
    LocalActionableError {
        message: String,
        action: String,
    },
    ResetSucceeded {
        message: String,
        presentation: TuiPresentation,
    },
    ContextChanged {
        message: String,
        presentation: TuiPresentation,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiProviderOutcome {
    Completed(String),
    Failed { message: String, action: String },
    Cancelled { message: String, action: String },
}

/// A visible conversation entry in chronological order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TranscriptEntry {
    /// A prompt submitted by the user.
    User(String),
    /// Text returned by the shared runtime.
    Assistant(String),
    /// Provider reasoning returned by the shared runtime.
    Reasoning(String),
    /// A sanitized runtime failure.
    Error(String),
    /// A local session or lifecycle note.
    Info(String),
    /// A tool lifecycle result with no tool input exposure.
    Tool(String),
}

/// State passed to renderers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewState<'a> {
    /// The editable prompt text.
    pub input: &'a str,
    /// Current terminal dimensions.
    pub size: (u16, u16),
    /// Whether the composed engine has an active turn.
    pub running: bool,
    /// Whether an idle second Ctrl+C will quit.
    pub quit_armed: bool,
    /// Conversation entries rendered in the order they occurred.
    pub transcript: &'a [TranscriptEntry],
    /// Whether new output advances to the bottom of the transcript.
    pub following_bottom: bool,
    /// The manual transcript offset when bottom following is disabled.
    pub scroll_offset: u16,
    /// Current provider and model selected by the CLI composition root.
    pub provider_model: &'a str,
    /// Active session label supplied by the CLI composition root.
    pub session: &'a str,
    /// Current active-turn state for the dedicated status row.
    pub turn_state: Option<TurnState>,
    /// Tool name currently being dispatched, when known.
    pub active_tool: Option<&'a str>,
    /// Current byte cursor position in the editable prompt.
    pub input_cursor: usize,
    /// Typed metrics retained for rich, lossless presentation.
    pub runtime_events: &'a [TuiRuntimeEvent],
    pub turn_duration: Option<Duration>,
    pub latest_usage: Option<&'a Usage>,
    /// Authoritative typed conversation projection, when a turn is active or completed.
    pub conversation: Option<&'a Conversation>,
    /// Tool outputs collapsed only for presentation; their source output remains retained.
    pub collapsed_tool_outputs: &'a BTreeSet<String>,
    /// A bounded informational dialog rendered above the conversation.
    pub dialog: Option<&'a DialogView>,
}

/// Generic bounded dialog state for interactive surfaces that are introduced later.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DialogView {
    title: String,
    body: String,
}

/// Ratatui renderer usable with both real terminals and `TestBackend`.
pub struct RatatuiRenderer<B: Backend> {
    terminal: RatatuiTerminal<B>,
}

impl<B: Backend> RatatuiRenderer<B> {
    pub fn new(terminal: RatatuiTerminal<B>) -> Self {
        Self { terminal }
    }

    pub fn terminal(&self) -> &RatatuiTerminal<B> {
        &self.terminal
    }
}

impl<B: Backend> Renderer for RatatuiRenderer<B> {
    fn render(&mut self, state: ViewState<'_>) -> io::Result<()> {
        self.terminal
            .draw(|frame| render_frame(frame, state))
            .map(|_| ())
            .map_err(|_| io::Error::other("Ratatui draw failed"))
    }
}

fn render_frame(frame: &mut ratatui::Frame<'_>, state: ViewState<'_>) {
    let area = frame.area();
    let layout = screen_layout(area, state.running);

    if layout.header.height > 0 {
        render_header(frame, layout.header, &state, layout.show_context);
    }

    let mut transcript = state
        .conversation
        .map(|conversation| {
            render::conversation_lines(
                conversation,
                state.runtime_events,
                state.collapsed_tool_outputs,
            )
        })
        .unwrap_or_else(|| transcript_lines(state.transcript));
    transcript.extend(render::detail_lines(
        state.runtime_events,
        state.conversation.is_some(),
    ));
    let visible_rows = layout.transcript.height.saturating_sub(1) as usize;
    let bottom_scroll =
        transcript_rows(&transcript, layout.transcript.width).saturating_sub(visible_rows) as u16;
    let scroll = if state.following_bottom {
        bottom_scroll
    } else {
        bottom_scroll.saturating_sub(state.scroll_offset)
    };
    let scroll_label = if state.following_bottom {
        " LIVE".to_owned()
    } else {
        format!(" SCROLL +{}", state.scroll_offset)
    };
    if layout.transcript.height > 0 {
        frame.render_widget(
            Paragraph::new(Text::from(transcript))
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(Color::DarkGray))
                        .title(Span::styled(
                            " transcript ",
                            Style::default().fg(Color::DarkGray),
                        ))
                        .title_bottom(Span::styled(
                            scroll_label,
                            Style::default().fg(Color::DarkGray),
                        ))
                        .title_alignment(Alignment::Right),
                )
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            layout.transcript,
        );
    }

    if layout.status.height > 0 {
        render_turn_status(frame, layout.status, &state);
    }

    let composer_title = if state.running {
        " Compose · running "
    } else {
        " Compose "
    };
    let composer_color = if state.running {
        Color::Yellow
    } else {
        Color::Cyan
    };
    if layout.composer.height > 0 {
        frame.render_widget(
            Paragraph::new(state.input)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(composer_color))
                        .title(Span::styled(
                            composer_title,
                            Style::default()
                                .fg(composer_color)
                                .add_modifier(Modifier::BOLD),
                        ))
                        .title_bottom(Span::styled(
                            composer_metadata(state.input),
                            Style::default().fg(Color::DarkGray),
                        ))
                        .title_alignment(Alignment::Right),
                )
                .wrap(Wrap { trim: false }),
            layout.composer,
        );
    }

    if layout.footer.height > 0 {
        frame.render_widget(
            Paragraph::new(footer_text(area.width, &state))
                .style(Style::default().fg(Color::DarkGray)),
            layout.footer,
        );
    }

    if let Some(dialog) = state.dialog {
        render_dialog(frame, area, dialog);
    }

    let (line, column) = cursor_position(state.input, state.input_cursor);
    let cursor_y = layout
        .composer
        .y
        .saturating_add(1)
        .saturating_add(line as u16);
    let cursor_x = layout
        .composer
        .x
        .saturating_add(1)
        .saturating_add(column as u16);
    if cursor_y < layout.composer.bottom() && cursor_x < layout.composer.right() {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_dialog(frame: &mut ratatui::Frame<'_>, area: Rect, dialog: &DialogView) {
    let width = area.width.saturating_sub(4).clamp(1, 64);
    let height = area.height.saturating_sub(4).clamp(1, 8);
    let dialog_area = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );

    frame.render_widget(Clear, dialog_area);
    frame.render_widget(
        Paragraph::new(dialog.body.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(Span::styled(
                        format!(" {} ", dialog.title),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )),
            )
            .wrap(Wrap { trim: false }),
        dialog_area,
    );
}

struct ScreenLayout {
    header: Rect,
    transcript: Rect,
    status: Rect,
    composer: Rect,
    footer: Rect,
    show_context: bool,
}

fn screen_layout(area: Rect, running: bool) -> ScreenLayout {
    let show_header = area.height >= 8;
    let show_context = area.width >= 80 && area.height > 16;
    let show_footer = area.height >= 10;
    let show_status = running && area.height > 16;
    let composer_rows = if area.height < 8 { 2 } else { 3 };
    let chunks = Layout::vertical([
        Constraint::Length(u16::from(show_header)),
        Constraint::Min(1),
        Constraint::Length(u16::from(show_status)),
        Constraint::Length(composer_rows),
        Constraint::Length(u16::from(show_footer)),
    ])
    .split(area);

    ScreenLayout {
        header: chunks[0],
        transcript: chunks[1],
        status: chunks[2],
        composer: chunks[3],
        footer: chunks[4],
        show_context,
    }
}

fn render_header(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &ViewState<'_>,
    show_context: bool,
) {
    let state_label = turn_state_label(state.turn_state, state.running);
    let mut left = vec![Span::styled(
        " agens ",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    if show_context {
        left.push(Span::styled("  ", Style::default()));
        left.push(Span::styled(
            state.provider_model,
            Style::default().fg(Color::Gray),
        ));
        left.push(Span::styled("  ·  ", Style::default().fg(Color::DarkGray)));
        left.push(Span::styled(
            state.session,
            Style::default().fg(Color::Gray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(left)), area);
    let state_width = state_label.len() as u16 + 1;
    if area.width > state_width {
        let state_area = Rect::new(area.right() - state_width, area.y, state_width, area.height);
        frame.render_widget(
            Paragraph::new(state_label)
                .style(Style::default().fg(turn_state_color(state.turn_state, state.running)))
                .alignment(Alignment::Right),
            state_area,
        );
    }
}

fn render_turn_status(frame: &mut ratatui::Frame<'_>, area: Rect, state: &ViewState<'_>) {
    let label = match (state.turn_state, state.active_tool) {
        (Some(TurnState::Dispatching), Some(tool)) => {
            format!(" {} Tool: {tool}", activity_marker(state))
        }
        _ => format!(
            " {} {}",
            activity_marker(state),
            turn_state_label(state.turn_state, state.running)
        ),
    };
    frame.render_widget(
        Paragraph::new(label)
            .style(Style::default().fg(turn_state_color(state.turn_state, state.running))),
        area,
    );
}

fn activity_marker(state: &ViewState<'_>) -> &'static str {
    match state.turn_state {
        Some(TurnState::Requesting) => "·",
        Some(TurnState::Streaming) => "~",
        Some(TurnState::Dispatching) => "*",
        Some(TurnState::Cancelled) => "·",
        Some(TurnState::Failed) => "!",
        _ if state.running => "~",
        _ => "·",
    }
}

fn turn_state_label(state: Option<TurnState>, running: bool) -> &'static str {
    match state {
        Some(TurnState::Requesting) => "Waiting",
        Some(TurnState::Streaming) => "Responding",
        Some(TurnState::Dispatching) => "Using tool",
        Some(TurnState::Cancelled) => "Cancelling",
        Some(TurnState::Failed) => "Failed",
        Some(TurnState::Completed) => "Completed",
        _ if running => "Working",
        _ => "Ready",
    }
}

fn turn_state_color(state: Option<TurnState>, running: bool) -> Color {
    match state {
        Some(TurnState::Failed) => Color::Red,
        Some(TurnState::Cancelled) => Color::Yellow,
        Some(TurnState::Dispatching) => Color::Magenta,
        Some(TurnState::Streaming) => Color::Cyan,
        _ if running => Color::Cyan,
        _ => Color::DarkGray,
    }
}

fn composer_metadata(input: &str) -> String {
    let lines = input.lines().count().max(1);
    format!(" {lines} lines · {} chars ", input.chars().count())
}

fn footer_text(width: u16, state: &ViewState<'_>) -> String {
    let duration = state.turn_duration.map_or_else(String::new, |value| {
        if value.as_secs() > 0 {
            format!(" · {}s", value.as_secs())
        } else {
            format!(" · {}ms", value.as_millis())
        }
    });
    let usage = state.latest_usage.map_or_else(String::new, |usage| {
        let tokens = usage
            .total_tokens
            .map_or_else(|| "unavailable".into(), |value| value.to_string());
        let context = usage
            .context_window
            .map_or_else(|| "unavailable".into(), |value| value.to_string());
        format!(" · tokens {tokens} · context {context}")
    });
    let metrics = format!(
        " {}{duration}{usage}",
        turn_state_label(state.turn_state, state.running)
    );
    if width < 60 {
        format!("{metrics}  ·  Enter send  ·  Ctrl+C cancel/quit")
    } else {
        format!(
            "{metrics}  ·  Enter send  ·  Shift+Enter newline  ·  Ctrl+O output  ·  Ctrl+C cancel/quit  ·  PgUp/PgDn scroll  ·  End follow"
        )
    }
}

fn transcript_lines(entries: &[TranscriptEntry]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in entries {
        let (label, color, text, card) = match entry {
            TranscriptEntry::User(text) => ("USER", Color::Green, text, false),
            TranscriptEntry::Assistant(text) => ("ASSISTANT", Color::Cyan, text, false),
            TranscriptEntry::Reasoning(text) => ("THINKING", Color::Blue, text, false),
            TranscriptEntry::Error(text) => ("ERROR", Color::Red, text, true),
            TranscriptEntry::Info(text) => ("INFO", Color::Yellow, text, false),
            TranscriptEntry::Tool(text) => ("TOOL", Color::Magenta, text, true),
        };
        let label_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        if card {
            lines.push(Line::from(Span::styled(
                format!("  ┌ {label} "),
                label_style,
            )));
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(color)),
                Span::raw(text.clone()),
            ]));
            if matches!(entry, TranscriptEntry::Error(_)) {
                lines.push(Line::from(Span::styled(
                    "  │ Action: retry the request or inspect the runtime error.",
                    Style::default().fg(Color::Yellow),
                )));
            }
            lines.push(Line::from(Span::styled("  └", Style::default().fg(color))));
        } else {
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(color)),
                Span::styled(format!("{label:<9} "), label_style),
                Span::raw(text.clone()),
            ]));
        }
        lines.push(Line::default());
    }
    lines
}

fn transcript_rows(lines: &[Line<'_>], width: u16) -> usize {
    let width = usize::from(width.max(1));
    lines
        .iter()
        .map(|line| line.width().div_ceil(width).max(1))
        .sum()
}

fn cursor_position(input: &str, cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(input.len());
    let before_cursor = &input[..cursor];
    let line = before_cursor.bytes().filter(|byte| *byte == b'\n').count();
    let column = before_cursor
        .rsplit('\n')
        .next()
        .map_or(0, |line| line.chars().count());
    (line, column)
}

/// Renders the current TUI state. Rendering is deliberately independent of event handling.
pub trait Renderer {
    /// Draws one frame for the supplied TUI state.
    fn render(&mut self, state: ViewState<'_>) -> io::Result<()>;
}

/// Minimal terminal renderer for the runnable TUI command.
pub struct PlainRenderer;

impl Renderer for PlainRenderer {
    fn render(&mut self, state: ViewState<'_>) -> io::Result<()> {
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[2J\x1b[HAgens\n\n")?;

        for entry in state.transcript {
            match entry {
                TranscriptEntry::User(text) => writeln!(stdout, "You: {text}\n")?,
                TranscriptEntry::Assistant(text) => writeln!(stdout, "Assistant: {text}\n")?,
                TranscriptEntry::Reasoning(text) => writeln!(stdout, "Reasoning: {text}\n")?,
                TranscriptEntry::Error(text) => writeln!(stdout, "Error: {text}\n")?,
                TranscriptEntry::Info(text) => writeln!(stdout, "{text}\n")?,
                TranscriptEntry::Tool(text) => writeln!(stdout, "Tool: {text}\n")?,
            }
        }

        if state.running {
            writeln!(stdout, "Working…")?;
        }
        write!(stdout, "> {}", state.input)?;
        stdout.flush()
    }
}

/// Small event engine shared by the terminal lifecycle and future TUI components.
pub struct Tui<E> {
    engine: E,
    input: String,
    input_cursor: usize,
    size: (u16, u16),
    running: bool,
    quit_armed: bool,
    transcript: Vec<TranscriptEntry>,
    following_bottom: bool,
    scroll_offset: u16,
    provider_model: String,
    session: String,
    turn_state: Option<TurnState>,
    active_tool: Option<String>,
    runtime_events: Vec<TuiRuntimeEvent>,
    turn_duration: Option<Duration>,
    latest_usage: Option<Usage>,
    conversation: Option<Conversation>,
    collapsed_tool_outputs: BTreeSet<String>,
    dialog: Option<DialogView>,
}

impl<E> Tui<E>
where
    E: Engine,
{
    /// Creates a TUI event engine around an injected application engine handle.
    pub fn new(engine: E) -> Self {
        Self {
            engine,
            input: String::new(),
            input_cursor: 0,
            size: (0, 0),
            running: false,
            quit_armed: false,
            transcript: Vec::new(),
            following_bottom: true,
            scroll_offset: 0,
            provider_model: "provider / model".to_owned(),
            session: "new session".to_owned(),
            turn_state: None,
            active_tool: None,
            runtime_events: Vec::new(),
            turn_duration: None,
            latest_usage: None,
            conversation: None,
            collapsed_tool_outputs: BTreeSet::new(),
            dialog: None,
        }
    }

    /// Handles one input or resize event without performing rendering or engine work.
    pub fn handle(&mut self, event: Event) -> Action {
        match event {
            Event::Resize { width, height } => {
                self.size = (width, height);
                self.quit_armed = false;
                Action::Render
            }
            Event::Key(key) => self.handle_key(key),
        }
    }

    /// Returns the input buffer for composition and focused tests.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Returns the most recently received terminal size.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Gives the composition layer mutable access to the shared engine handle.
    pub fn engine(&mut self) -> &mut E {
        &mut self.engine
    }

    /// Updates active-turn state after the composition layer starts or finishes a turn.
    pub fn set_running(&mut self, running: bool) {
        self.running = running;
        if running {
            self.quit_armed = false;
            self.turn_state = Some(TurnState::Requesting);
        } else if !matches!(
            self.turn_state,
            Some(TurnState::Failed | TurnState::Cancelled)
        ) {
            self.turn_state = None;
            self.active_tool = None;
        }
    }

    /// Supplies concise provider, model, and active-session context for the terminal surface.
    pub fn set_presentation(
        &mut self,
        provider: impl AsRef<str>,
        model: impl AsRef<str>,
        session: impl Into<String>,
    ) {
        self.provider_model = format!("{} / {}", provider.as_ref(), model.as_ref());
        self.session = session.into();
    }

    /// Adds a user prompt before the composition layer starts the shared runtime.
    pub fn begin_submission(&mut self, prompt: impl Into<String>) {
        let prompt = prompt.into();
        self.runtime_events.clear();
        self.turn_duration = None;
        self.latest_usage = None;
        self.transcript.push(TranscriptEntry::User(prompt.clone()));
        self.conversation = Some(Conversation::new(prompt));
        self.collapsed_tool_outputs.clear();
        self.set_running(true);
    }

    /// Records a completed runtime result without exposing provider internals.
    pub fn finish_submission(&mut self, result: Result<String, String>) {
        let outcome = match result {
            Ok(output) => TuiProviderOutcome::Completed(output),
            Err(message) => TuiProviderOutcome::Failed {
                message,
                action: "Retry the request or inspect the runtime error.".into(),
            },
        };
        self.finish_provider_turn(outcome);
    }

    /// Adds a local session or lifecycle note to the visible conversation.
    pub fn add_info(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.transcript.push(TranscriptEntry::Info(text.clone()));
        self.project_conversation(ConversationEvent::Info(text));
    }

    pub fn apply_submission_outcome(&mut self, outcome: TuiSubmissionOutcome) -> Option<String> {
        match outcome {
            TuiSubmissionOutcome::ProviderTurn { prompt } => {
                self.begin_submission(prompt.clone());
                Some(prompt)
            }
            TuiSubmissionOutcome::LocalInfo(message) => {
                self.add_info(message);
                None
            }
            TuiSubmissionOutcome::LocalActionableError { message, action } => {
                self.add_error(message, action);
                None
            }
            TuiSubmissionOutcome::ResetSucceeded {
                message,
                presentation,
            } => {
                self.clear_transcript();
                self.apply_presentation(presentation);
                self.add_info(message);
                None
            }
            TuiSubmissionOutcome::ContextChanged {
                message,
                presentation,
            } => {
                self.apply_presentation(presentation);
                self.add_info(message);
                None
            }
        }
    }

    pub fn finish_provider_turn(&mut self, outcome: TuiProviderOutcome) {
        match outcome {
            TuiProviderOutcome::Completed(output) => {
                self.project_conversation(ConversationEvent::MarkdownFinal(output.clone()));
                if !matches!(self.transcript.last(), Some(TranscriptEntry::Assistant(_))) {
                    self.transcript.push(TranscriptEntry::Assistant(output));
                }
                self.set_running(false);
            }
            TuiProviderOutcome::Failed { message, action } => {
                self.running = false;
                self.turn_state = Some(TurnState::Failed);
                self.active_tool = None;
                self.add_error(message, action);
            }
            TuiProviderOutcome::Cancelled { message, action } => {
                self.running = false;
                self.turn_state = Some(TurnState::Cancelled);
                self.active_tool = None;
                self.add_error(message, action);
            }
        }
    }

    /// Clears the current visible conversation for a new session.
    pub fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.conversation = None;
        self.collapsed_tool_outputs.clear();
        self.set_running(false);
    }

    /// Returns the visible conversation for composition and focused tests.
    pub fn transcript(&self) -> &[TranscriptEntry] {
        &self.transcript
    }

    /// Retains typed runtime metrics for the renderer without altering turn persistence.
    pub fn apply_runtime_event(&mut self, event: TuiRuntimeEvent) {
        match &event {
            TuiRuntimeEvent::TurnStarted => self.turn_state = Some(TurnState::Requesting),
            TuiRuntimeEvent::TurnEnded { status, duration } => {
                self.running = false;
                self.turn_state = Some(*status);
                self.turn_duration = *duration;
                self.active_tool = None;
            }
            TuiRuntimeEvent::Usage(usage) => self.latest_usage = Some(usage.clone()),
            TuiRuntimeEvent::Diff { lines, .. } => {
                self.project_conversation(ConversationEvent::Diff(lines.clone()));
            }
            TuiRuntimeEvent::ToolStarted { .. } | TuiRuntimeEvent::ToolEnded { .. } => {}
        }
        self.runtime_events.push(event);
    }

    /// Adds a typed event to the authoritative, lossless conversation projection.
    pub fn apply_conversation_event(
        &mut self,
        event: ConversationEvent,
    ) -> Result<(), ConversationError> {
        self.conversation
            .get_or_insert_with(|| Conversation::new(String::new()))
            .apply(event)
    }

    /// Opens a generic bounded dialog without changing the underlying conversation.
    pub fn show_dialog(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.dialog = Some(DialogView {
            title: title.into(),
            body: body.into(),
        });
    }

    pub fn runtime_events(&self) -> &[TuiRuntimeEvent] {
        &self.runtime_events
    }

    /// Returns an immutable snapshot for a renderer.
    pub fn view(&self) -> ViewState<'_> {
        ViewState {
            input: &self.input,
            size: self.size,
            running: self.running,
            quit_armed: self.quit_armed,
            transcript: &self.transcript,
            following_bottom: self.following_bottom,
            scroll_offset: self.scroll_offset,
            provider_model: &self.provider_model,
            session: &self.session,
            turn_state: self.turn_state,
            active_tool: self.active_tool.as_deref(),
            input_cursor: self.input_cursor,
            runtime_events: &self.runtime_events,
            turn_duration: self.turn_duration,
            latest_usage: self.latest_usage.as_ref(),
            conversation: self.conversation.as_ref(),
            collapsed_tool_outputs: &self.collapsed_tool_outputs,
            dialog: self.dialog.as_ref(),
        }
    }

    pub const fn following_bottom(&self) -> bool {
        self.following_bottom
    }

    /// Applies ordered runtime progress without changing completed persistence semantics.
    pub fn apply_progress(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::ProviderPart(MessagePart::Text(delta)) => {
                self.project_conversation(ConversationEvent::MarkdownDelta(delta.clone()));
                self.turn_state = Some(TurnState::Streaming);
                match self.transcript.last_mut() {
                    Some(TranscriptEntry::Assistant(text)) => text.push_str(&delta),
                    _ => self.transcript.push(TranscriptEntry::Assistant(delta)),
                }
            }
            TurnEvent::ProviderPart(MessagePart::Reasoning(delta)) => {
                self.project_conversation(ConversationEvent::ReasoningDelta(delta.clone()));
                match self.transcript.last_mut() {
                    Some(TranscriptEntry::Reasoning(text)) => text.push_str(&delta),
                    _ => self.transcript.push(TranscriptEntry::Reasoning(delta)),
                }
            }
            TurnEvent::ToolCallRequested { id, name, input } => {
                self.project_conversation(ConversationEvent::ToolCall {
                    call_id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                self.turn_state = Some(TurnState::Dispatching);
                self.active_tool = Some(name.clone());
                self.transcript
                    .push(TranscriptEntry::Tool(format!("{name} started")));
            }
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id,
                content,
                is_error,
            }) => {
                self.project_conversation(ConversationEvent::ToolResult {
                    call_id: tool_call_id.clone(),
                    output: content.clone(),
                    is_error,
                });
                let name = self
                    .transcript
                    .iter()
                    .rev()
                    .find_map(|entry| match entry {
                        TranscriptEntry::Tool(value) => value.strip_suffix(" started"),
                        _ => None,
                    })
                    .unwrap_or("tool");
                let outcome = if is_error { "failed" } else { "completed" };
                self.transcript.push(TranscriptEntry::Tool(format!(
                    "{name} {outcome}: {content}"
                )));
            }
            TurnEvent::StateChanged(TurnState::Completed) => self.set_running(false),
            TurnEvent::StateChanged(state @ (TurnState::Cancelled | TurnState::Failed)) => {
                self.running = false;
                self.turn_state = Some(state);
                self.active_tool = None;
            }
            TurnEvent::StateChanged(state) => self.turn_state = Some(state),
            _ => {}
        }
        if self.following_bottom {
            self.scroll_offset = 0;
        }
    }

    fn handle_key(&mut self, key: Key) -> Action {
        if key != Key::CtrlC {
            self.quit_armed = false;
        }

        match key {
            Key::CtrlO => {
                self.toggle_tool_output_expansion();
                Action::Render
            }
            Key::Char(character) => {
                self.input.insert(self.input_cursor, character);
                self.input_cursor += character.len_utf8();
                Action::Render
            }
            Key::Backspace => {
                if let Some(previous) = self.input[..self.input_cursor].char_indices().last() {
                    self.input_cursor = previous.0;
                    self.input.remove(self.input_cursor);
                }
                Action::Render
            }
            Key::ShiftEnter => {
                self.input.insert(self.input_cursor, '\n');
                self.input_cursor += 1;
                Action::Render
            }
            Key::Left => {
                self.input_cursor = self.input[..self.input_cursor]
                    .char_indices()
                    .last()
                    .map_or(0, |(index, _)| index);
                Action::Render
            }
            Key::Right => {
                self.input_cursor = self.input[self.input_cursor..]
                    .chars()
                    .next()
                    .map_or(self.input.len(), |character| {
                        self.input_cursor + character.len_utf8()
                    });
                Action::Render
            }
            Key::Home => {
                self.input_cursor = self.input[..self.input_cursor]
                    .rfind('\n')
                    .map_or(0, |index| index + 1);
                Action::Render
            }
            Key::End => {
                self.following_bottom = true;
                self.scroll_offset = 0;
                self.input_cursor = self.input[self.input_cursor..]
                    .find('\n')
                    .map_or(self.input.len(), |index| self.input_cursor + index);
                Action::Render
            }
            Key::PageUp => {
                self.following_bottom = false;
                self.scroll_offset = self.scroll_offset.saturating_add(5);
                Action::Render
            }
            Key::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(5);
                if self.scroll_offset == 0 {
                    self.following_bottom = true;
                }
                Action::Render
            }
            Key::Enter if self.input.is_empty() => Action::Render,
            Key::Enter if self.running => {
                self.transcript.push(TranscriptEntry::Info(
                    "A response is already in progress.".into(),
                ));
                Action::Render
            }
            Key::Enter => {
                self.input_cursor = 0;
                Action::Submit(std::mem::take(&mut self.input))
            }
            Key::Escape if self.running => self.cancel_running(),
            Key::Escape => Action::Render,
            Key::CtrlC if self.running => self.cancel_running(),
            Key::CtrlC if !self.input.is_empty() => {
                self.input.clear();
                self.input_cursor = 0;
                Action::Render
            }
            Key::CtrlC if self.quit_armed => Action::Quit,
            Key::CtrlC => {
                self.quit_armed = true;
                Action::Render
            }
        }
    }

    fn cancel_running(&mut self) -> Action {
        self.engine.cancel();
        self.quit_armed = false;
        self.turn_state = Some(TurnState::Cancelled);
        Action::Cancel
    }

    fn toggle_tool_output_expansion(&mut self) {
        let Some(conversation) = self.conversation.as_ref() else {
            return;
        };
        let completed_call_ids = conversation
            .tool_batches
            .iter()
            .flat_map(|batch| &batch.calls)
            .filter(|call| call.result.is_some())
            .map(|call| call.call_id.clone())
            .collect::<Vec<_>>();

        if completed_call_ids.is_empty() {
            return;
        }

        if completed_call_ids
            .iter()
            .all(|call_id| !self.collapsed_tool_outputs.contains(call_id))
        {
            self.collapsed_tool_outputs.extend(completed_call_ids);
        } else {
            for call_id in completed_call_ids {
                self.collapsed_tool_outputs.remove(&call_id);
            }
        }
    }

    fn project_conversation(&mut self, event: ConversationEvent) {
        if self.apply_conversation_event(event).is_err() {
            self.conversation
                .as_mut()
                .expect("conversation is initialized before projection")
                .errors
                .push(ActionableError {
                    message: "Conversation event could not be projected.".into(),
                    action: "Inspect the runtime error and retry the request.".into(),
                });
        }
    }

    fn add_error(&mut self, message: String, action: String) {
        self.project_conversation(ConversationEvent::Error { message, action });
        let message = self
            .conversation
            .as_ref()
            .and_then(|conversation| conversation.errors.last())
            .map_or_else(|| "Runtime request failed.".into(), |error| error.message.clone());
        self.transcript.push(TranscriptEntry::Error(message));
    }

    fn apply_presentation(&mut self, presentation: TuiPresentation) {
        self.set_presentation(
            presentation.provider,
            presentation.model,
            presentation.session,
        );
    }
}

/// Owns raw-mode and alternate-screen restoration for an interactive terminal session.
pub struct Terminal {
    control: CrosstermControl,
    guard: TerminalModeGuard,
}

impl Terminal {
    /// Enters the terminal modes required by the TUI.
    pub fn enter() -> io::Result<Self> {
        let mut control = CrosstermControl {
            stdout: io::stdout(),
        };
        let guard = TerminalModeGuard::enter(&mut control)?;
        Ok(Self { control, guard })
    }

    /// Waits up to `timeout` for a terminal event relevant to the TUI engine.
    pub fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }

        Ok(map_event(event::read()?))
    }

    /// Restores the main screen and normal terminal mode. It is safe to call repeatedly.
    pub fn restore(&mut self) -> io::Result<()> {
        self.guard.restore(&mut self.control)
    }
}

struct CrosstermControl {
    stdout: Stdout,
}

impl TerminalControl for CrosstermControl {
    fn apply(&mut self, operation: TerminalOperation) -> io::Result<()> {
        match operation {
            TerminalOperation::EnableRaw => crossterm_terminal::enable_raw_mode(),
            TerminalOperation::DisableRaw => crossterm_terminal::disable_raw_mode(),
            TerminalOperation::EnterAlternate => {
                execute!(self.stdout, EnterAlternateScreen).map(|_| ())
            }
            TerminalOperation::LeaveAlternate => {
                execute!(self.stdout, LeaveAlternateScreen).map(|_| ())
            }
            TerminalOperation::EnableMouse => execute!(self.stdout, EnableMouseCapture).map(|_| ()),
            TerminalOperation::DisableMouse => {
                execute!(self.stdout, DisableMouseCapture).map(|_| ())
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

trait RuntimeTerminal {
    fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>>;
}

impl RuntimeTerminal for Terminal {
    fn poll(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        Self::poll(self, timeout)
    }
}

/// Runs a terminal event loop and hands rendering to the caller-owned renderer.
pub fn run<E, R>(tui: &mut Tui<E>, renderer: &mut R) -> io::Result<()>
where
    E: Engine,
    R: Renderer,
{
    run_with_runtime_terminal(tui, renderer, Terminal::enter()?)
}

fn run_with_runtime_terminal<E, R, T>(
    tui: &mut Tui<E>,
    renderer: &mut R,
    mut terminal: T,
) -> io::Result<()>
where
    E: Engine,
    R: Renderer,
    T: RuntimeTerminal,
{
    renderer.render(tui.view())?;

    loop {
        let Some(event) = terminal.poll(Duration::from_millis(100))? else {
            continue;
        };

        match tui.handle(event) {
            Action::Quit => return Ok(()),
            Action::Render | Action::Submit(_) | Action::Cancel => renderer.render(tui.view())?,
        }
    }
}

/// Runs the terminal loop while sending prompt submissions through the injected shared runtime.
pub fn run_with_submit<E, R, F>(tui: &mut Tui<E>, renderer: &mut R, submit: F) -> io::Result<()>
where
    E: Engine + Send,
    R: Renderer,
    F: Fn(String) -> Result<String, String> + Send + Sync + 'static,
{
    let submit = Arc::new(submit);
    let (sender, receiver) = mpsc::channel();
    let mut terminal = Terminal::enter()?;
    renderer.render(tui.view())?;

    loop {
        while let Ok(result) = receiver.try_recv() {
            tui.finish_submission(result);
            renderer.render(tui.view())?;
        }

        let Some(event) = terminal.poll(Duration::from_millis(100))? else {
            continue;
        };

        match tui.handle(event) {
            Action::Quit => return Ok(()),
            Action::Submit(prompt) => {
                tui.begin_submission(prompt.clone());
                let submit = Arc::clone(&submit);
                let sender = sender.clone();
                thread::spawn(move || {
                    let _ = sender.send(submit(prompt));
                });
                renderer.render(tui.view())?;
            }
            Action::Render | Action::Cancel => renderer.render(tui.view())?,
        }
    }
}

/// Runs the production fullscreen Ratatui surface and restores the terminal on every exit path.
pub fn run_with_default_submit<E, F>(tui: &mut Tui<E>, submit: F) -> io::Result<()>
where
    E: Engine + Send,
    F: Fn(String) -> Result<String, String> + Send + Sync + 'static,
{
    let terminal = ratatui::try_init()?;
    let _restore = RatatuiRestore;
    let mut renderer = RatatuiRenderer::new(terminal);
    run_with_submit(tui, &mut renderer, submit)
}

/// Runs a submit worker that can forward ordered runtime events while it is active.
pub fn run_with_default_progress_submit<E, F>(tui: &mut Tui<E>, submit: F) -> io::Result<()>
where
    E: Engine + Send,
    F: Fn(String, mpsc::Sender<TurnEvent>, BridgeTx<TuiRuntimeEvent>) -> Result<String, String>
        + Send
        + Sync
        + 'static,
{
    let submit = Arc::new(submit);
    let (sender, receiver) = mpsc::channel();
    let (metrics_sender, metrics_receiver) = BridgeTx::bounded(128);
    let terminal = ratatui::try_init()?;
    let _restore = RatatuiRestore;
    let mut renderer = RatatuiRenderer::new(terminal);
    renderer.render(tui.view())?;

    loop {
        for _ in 0..32 {
            let Ok(envelope) = metrics_receiver.try_recv() else {
                break;
            };
            tui.apply_runtime_event(envelope.into_parts().1);
        }
        for _ in 0..32 {
            let Ok(event) = receiver.try_recv() else {
                break;
            };
            tui.apply_progress(event);
        }
        renderer.render(tui.view())?;
        if !event::poll(Duration::from_millis(25))? {
            continue;
        }
        let Some(event) = map_event(event::read()?) else {
            continue;
        };
        match tui.handle(event) {
            Action::Quit => return Ok(()),
            Action::Submit(prompt) => {
                tui.begin_submission(prompt.clone());
                let submit = Arc::clone(&submit);
                let sender = sender.clone();
                let metrics = metrics_sender.clone();
                thread::spawn(move || {
                    let progress = sender.clone();
                    let _ = sender.send(TurnEvent::StateChanged(TurnState::Requesting));
                    let result = submit(prompt, progress, metrics);
                    match result {
                        Ok(_) => {
                            let _ = sender.send(TurnEvent::StateChanged(TurnState::Completed));
                        }
                        Err(error) => {
                            let _ = sender.send(TurnEvent::StateChanged(TurnState::Failed));
                            let _ = sender.send(TurnEvent::ProviderPart(MessagePart::Text(error)));
                        }
                    }
                });
            }
            Action::Render | Action::Cancel => {}
        }
    }
}

struct RatatuiRestore;

impl Drop for RatatuiRestore {
    fn drop(&mut self) {
        let _ = ratatui::try_restore();
    }
}

fn map_event(event: CrosstermEvent) -> Option<Event> {
    match event {
        CrosstermEvent::Resize(width, height) => Some(Event::Resize { width, height }),
        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => {
            map_key(key.code, key.modifiers)
        }
        _ => None,
    }
}

fn map_key(code: KeyCode, modifiers: KeyModifiers) -> Option<Event> {
    let key = match (code, modifiers) {
        (KeyCode::Char('c' | 'C'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlC
        }
        (KeyCode::Char('o' | 'O'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Key::CtrlO
        }
        (KeyCode::Char(character), modifiers)
            if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
        {
            Key::Char(character)
        }
        (KeyCode::Backspace, _) => Key::Backspace,
        (KeyCode::Enter, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => Key::ShiftEnter,
        (KeyCode::Enter, _) => Key::Enter,
        (KeyCode::Left, _) => Key::Left,
        (KeyCode::Right, _) => Key::Right,
        (KeyCode::Home, _) => Key::Home,
        (KeyCode::End, _) => Key::End,
        (KeyCode::PageUp, _) => Key::PageUp,
        (KeyCode::PageDown, _) => Key::PageDown,
        (KeyCode::Esc, _) => Key::Escape,
        _ => return None,
    };

    Some(Event::Key(key))
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crate::terminal::{TerminalControl, TerminalModeGuard, TerminalOperation};
    use std::{cell::RefCell, rc::Rc};

    #[derive(Default)]
    struct RecordingControl {
        calls: Rc<RefCell<Vec<TerminalOperation>>>,
    }

    impl TerminalControl for RecordingControl {
        fn apply(&mut self, operation: TerminalOperation) -> io::Result<()> {
            self.calls.borrow_mut().push(operation);
            Ok(())
        }
    }

    struct GuardedRuntime {
        guard: TerminalModeGuard,
        control: RecordingControl,
        input_error: io::ErrorKind,
    }

    impl GuardedRuntime {
        fn new(input_error: io::ErrorKind) -> (Self, Rc<RefCell<Vec<TerminalOperation>>>) {
            let mut control = RecordingControl::default();
            let calls = Rc::clone(&control.calls);
            let guard = TerminalModeGuard::enter(&mut control).unwrap();

            (
                Self {
                    guard,
                    control,
                    input_error,
                },
                calls,
            )
        }
    }

    impl RuntimeTerminal for GuardedRuntime {
        fn poll(&mut self, _: Duration) -> io::Result<Option<Event>> {
            Err(io::Error::from(self.input_error))
        }
    }

    impl Drop for GuardedRuntime {
        fn drop(&mut self) {
            let _ = self.guard.restore(&mut self.control);
        }
    }

    struct NoopEngine;

    impl Engine for NoopEngine {
        fn cancel(&mut self) {}
    }

    struct FailingRenderer {
        fail_on_render: usize,
        renders: usize,
    }

    impl Renderer for FailingRenderer {
        fn render(&mut self, _: ViewState<'_>) -> io::Result<()> {
            self.renders += 1;
            if self.renders == self.fail_on_render {
                return Err(io::Error::other("injected renderer failure"));
            }

            Ok(())
        }
    }

    fn expected_terminal_calls() -> Vec<TerminalOperation> {
        vec![
            TerminalOperation::EnableRaw,
            TerminalOperation::EnterAlternate,
            TerminalOperation::EnableMouse,
            TerminalOperation::DisableMouse,
            TerminalOperation::LeaveAlternate,
            TerminalOperation::DisableRaw,
        ]
    }

    #[test]
    fn runtime_restores_each_mode_once_after_renderer_failure() {
        let (terminal, calls) = GuardedRuntime::new(io::ErrorKind::Other);
        let mut tui = Tui::new(NoopEngine);
        let mut renderer = FailingRenderer {
            fail_on_render: 1,
            renders: 0,
        };

        let error = run_with_runtime_terminal(&mut tui, &mut renderer, terminal).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(*calls.borrow(), expected_terminal_calls());
    }

    #[test]
    fn runtime_restores_each_mode_once_after_input_poll_or_read_failure() {
        for input_error in [io::ErrorKind::TimedOut, io::ErrorKind::UnexpectedEof] {
            let (terminal, calls) = GuardedRuntime::new(input_error);
            let mut tui = Tui::new(NoopEngine);
            let mut renderer = FailingRenderer {
                fail_on_render: 2,
                renders: 0,
            };

            let error = run_with_runtime_terminal(&mut tui, &mut renderer, terminal).unwrap_err();

            assert_eq!(error.kind(), input_error);
            assert_eq!(*calls.borrow(), expected_terminal_calls());
        }
    }

    #[test]
    fn maps_control_o_to_tool_output_toggle() {
        assert_eq!(
            map_key(KeyCode::Char('o'), KeyModifiers::CONTROL),
            Some(Event::Key(Key::CtrlO))
        );
    }
}

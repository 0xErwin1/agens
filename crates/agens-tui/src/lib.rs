//! Terminal lifecycle and input-event boundary for the interactive surface.

use std::{
    io::{self, Stdout, Write},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use agens_core::{MessagePart, TurnEvent, TurnState};
use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Terminal as RatatuiTerminal,
    backend::Backend,
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
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

/// A visible conversation entry in chronological order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TranscriptEntry {
    /// A prompt submitted by the user.
    User(String),
    /// Text returned by the shared runtime.
    Assistant(String),
    /// A sanitized runtime failure.
    Error(String),
    /// A local session or lifecycle note.
    Info(String),
    /// A tool lifecycle result with no tool input exposure.
    Tool(String),
}

/// State passed to renderers. Prompt/session/provider presentation belongs to WU-20.
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
    let composer_rows = area.height.clamp(3, 6);
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(composer_rows),
        Constraint::Length(1),
    ])
    .split(area);
    let header = format!(
        " Agens  {}",
        if state.running { "working" } else { "ready" }
    );
    frame.render_widget(
        Paragraph::new(header).style(Style::default().fg(Color::Cyan)),
        chunks[0],
    );

    let lines = transcript_lines(state.transcript);
    let scroll = if state.following_bottom {
        u16::MAX
    } else {
        state.scroll_offset
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::TOP))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        chunks[1],
    );
    let status = if state.running {
        " Responding — Esc/Ctrl+C cancels"
    } else {
        ""
    };
    frame.render_widget(Paragraph::new(status), chunks[2]);
    frame.render_widget(
        Paragraph::new(state.input)
            .block(Block::default().borders(Borders::ALL).title("Compose"))
            .wrap(Wrap { trim: false }),
        chunks[3],
    );
    frame.render_widget(
        Paragraph::new(" Enter send · Shift+Enter newline · PgUp/PgDn scroll · Ctrl+C cancel/quit"),
        chunks[4],
    );

    let (line, column) = cursor_position(state.input);
    let cursor_y = chunks[3].y.saturating_add(1).saturating_add(line as u16);
    let cursor_x = chunks[3].x.saturating_add(1).saturating_add(column as u16);
    if cursor_y < chunks[3].bottom() && cursor_x < chunks[3].right() {
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn transcript_lines(entries: &[TranscriptEntry]) -> Vec<Line<'static>> {
    entries
        .iter()
        .map(|entry| match entry {
            TranscriptEntry::User(text) => Line::from(vec![
                Span::styled("You: ", Style::default().fg(Color::Green)),
                Span::raw(text.clone()),
            ]),
            TranscriptEntry::Assistant(text) => Line::from(vec![
                Span::styled("Assistant: ", Style::default().fg(Color::Cyan)),
                Span::raw(text.clone()),
            ]),
            TranscriptEntry::Error(text) => Line::from(vec![
                Span::styled("Error: ", Style::default().fg(Color::Red)),
                Span::raw(text.clone()),
            ]),
            TranscriptEntry::Info(text) => Line::from(vec![
                Span::styled("Info: ", Style::default().fg(Color::Yellow)),
                Span::raw(text.clone()),
            ]),
            TranscriptEntry::Tool(text) => Line::from(vec![
                Span::styled("Tool: ", Style::default().fg(Color::Magenta)),
                Span::raw(text.clone()),
            ]),
        })
        .collect()
}

fn cursor_position(input: &str) -> (usize, usize) {
    let line = input.lines().count().saturating_sub(1);
    let column = input
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
        }
    }

    /// Adds a user prompt before the composition layer starts the shared runtime.
    pub fn begin_submission(&mut self, prompt: impl Into<String>) {
        self.transcript.push(TranscriptEntry::User(prompt.into()));
        self.set_running(true);
    }

    /// Records a completed runtime result without exposing provider internals.
    pub fn finish_submission(&mut self, result: Result<String, String>) {
        let entry = match result {
            Ok(output) => TranscriptEntry::Assistant(output),
            Err(error) => TranscriptEntry::Error(error),
        };
        self.transcript.push(entry);
        self.set_running(false);
    }

    /// Adds a local session or lifecycle note to the visible conversation.
    pub fn add_info(&mut self, text: impl Into<String>) {
        self.transcript.push(TranscriptEntry::Info(text.into()));
    }

    /// Clears the current visible conversation for a new session.
    pub fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.set_running(false);
    }

    /// Returns the visible conversation for composition and focused tests.
    pub fn transcript(&self) -> &[TranscriptEntry] {
        &self.transcript
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
        }
    }

    pub const fn following_bottom(&self) -> bool {
        self.following_bottom
    }

    /// Applies ordered runtime progress without changing completed persistence semantics.
    pub fn apply_progress(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::ProviderPart(MessagePart::Text(delta)) => match self.transcript.last_mut() {
                Some(TranscriptEntry::Assistant(text)) => text.push_str(&delta),
                _ => self.transcript.push(TranscriptEntry::Assistant(delta)),
            },
            TurnEvent::ProviderPart(MessagePart::Reasoning(delta)) => {
                match self.transcript.last_mut() {
                    Some(TranscriptEntry::Info(text)) if text.starts_with("Reasoning: ") => {
                        text.push_str(&delta)
                    }
                    _ => self
                        .transcript
                        .push(TranscriptEntry::Info(format!("Reasoning: {delta}"))),
                }
            }
            TurnEvent::ToolCallRequested { name, .. } => self
                .transcript
                .push(TranscriptEntry::Tool(format!("{name} started"))),
            TurnEvent::ToolResult(MessagePart::ToolResult {
                content, is_error, ..
            }) => {
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
            TurnEvent::StateChanged(
                TurnState::Completed | TurnState::Cancelled | TurnState::Failed,
            ) => self.set_running(false),
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
        Action::Cancel
    }
}

/// Owns raw-mode and alternate-screen restoration for an interactive terminal session.
pub struct Terminal {
    stdout: Stdout,
    active: bool,
}

impl Terminal {
    /// Enters raw mode and the alternate screen, restoring raw mode if setup fails.
    pub fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;

        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }

        Ok(Self {
            stdout,
            active: true,
        })
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
        if !self.active {
            return Ok(());
        }

        self.active = false;
        let leave_result = execute!(self.stdout, LeaveAlternateScreen);
        let raw_mode_result = terminal::disable_raw_mode();

        leave_result.and(raw_mode_result)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Runs a terminal event loop and hands rendering to the caller-owned renderer.
pub fn run<E, R>(tui: &mut Tui<E>, renderer: &mut R) -> io::Result<()>
where
    E: Engine,
    R: Renderer,
{
    let mut terminal = Terminal::enter()?;
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
    F: Fn(String, mpsc::Sender<TurnEvent>) -> Result<String, String> + Send + Sync + 'static,
{
    let submit = Arc::new(submit);
    let (sender, receiver) = mpsc::channel();
    let terminal = ratatui::try_init()?;
    let _restore = RatatuiRestore;
    let mut renderer = RatatuiRenderer::new(terminal);
    renderer.render(tui.view())?;

    loop {
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
                thread::spawn(move || {
                    let progress = sender.clone();
                    let _ = sender.send(TurnEvent::StateChanged(TurnState::Requesting));
                    let result = submit(prompt, progress);
                    if let Err(error) = result {
                        let _ = sender.send(TurnEvent::StateChanged(TurnState::Failed));
                        let _ = sender.send(TurnEvent::ProviderPart(MessagePart::Text(error)));
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

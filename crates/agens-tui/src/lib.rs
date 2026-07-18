//! Terminal lifecycle and input-event boundary for the interactive surface.

use std::{
    io::{self, Stdout},
    time::Duration,
};

use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
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
}

/// Renders the current TUI state. Rendering is deliberately independent of event handling.
pub trait Renderer {
    /// Draws one frame for the supplied TUI state.
    fn render(&mut self, state: ViewState<'_>) -> io::Result<()>;
}

/// Small event engine shared by the terminal lifecycle and future TUI components.
pub struct Tui<E> {
    engine: E,
    input: String,
    size: (u16, u16),
    running: bool,
    quit_armed: bool,
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
            size: (0, 0),
            running: false,
            quit_armed: false,
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

    /// Returns an immutable snapshot for a renderer.
    pub fn view(&self) -> ViewState<'_> {
        ViewState {
            input: &self.input,
            size: self.size,
            running: self.running,
            quit_armed: self.quit_armed,
        }
    }

    fn handle_key(&mut self, key: Key) -> Action {
        if key != Key::CtrlC {
            self.quit_armed = false;
        }

        match key {
            Key::Char(character) => {
                self.input.push(character);
                Action::Render
            }
            Key::Backspace => {
                self.input.pop();
                Action::Render
            }
            Key::Enter if self.input.is_empty() => Action::Render,
            Key::Enter => Action::Submit(std::mem::take(&mut self.input)),
            Key::Escape if self.running => self.cancel_running(),
            Key::Escape => Action::Render,
            Key::CtrlC if self.running => self.cancel_running(),
            Key::CtrlC if !self.input.is_empty() => {
                self.input.clear();
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
        (KeyCode::Enter, _) => Key::Enter,
        (KeyCode::Esc, _) => Key::Escape,
        _ => return None,
    };

    Some(Event::Key(key))
}

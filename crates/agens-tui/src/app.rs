//! Pure prompt-queue state and effects for the terminal application.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const EXIT_WARNING_WINDOW: Duration = Duration::from_secs(2);
const RUNNING_REFUSAL: &str = "This command is unavailable while a response is in progress.";

/// Whether the application currently owns an active runtime turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Runtime {
    /// No turn is active.
    Idle,
    /// A turn is active; prompts may enter the fixed-capacity queue.
    Running,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Dialog {
    Command,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    ControlC,
    Escape,
    Navigate,
    Display,
    Select,
    Queue,
    New,
    Model,
    Effort,
    Session,
    Agent,
}

/// Events accepted by the prompt reducer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppEvent {
    /// An explicitly safe conversational prompt was submitted.
    SubmitPrompt(String),
    /// The active turn completed successfully with its final output.
    TurnCompleted(String),
    /// The active turn was cancelled.
    TurnCancelled,
    /// The active turn failed.
    TurnFailed,
    Command(Command, Instant),
    ResetSucceeded,
    TimerTick(Instant),
}

/// Work requested by the reducer for the runtime adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    /// Begin a new runtime turn for this prompt.
    StartPrompt(String),
    /// Persist a successfully completed prompt and output pair.
    PersistCompleted {
        prompt: String,
        output: String,
    },
    /// Present a deterministic refusal without mutating history.
    RefusePrompt(String),
    CancelTurn,
    ExitWarning,
    Quit,
    Render,
    DialogCommand(Command),
    ResetConversation,
    RefuseCommand(String),
}

/// Application state whose prompt queue has a fixed capacity for its lifetime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppState {
    runtime: Runtime,
    active_prompt: Option<String>,
    queued_prompts: VecDeque<String>,
    queue_capacity: usize,
    completed_history: Vec<(String, String)>,
    composer: String,
    dialog: Option<Dialog>,
    exit_armed_until: Option<Instant>,
    reset_pending: bool,
}

impl AppState {
    /// Creates application state with a non-zero, fixed prompt queue capacity.
    pub fn new(queue_capacity: usize) -> Self {
        assert!(queue_capacity > 0, "prompt queue capacity must be non-zero");

        Self {
            runtime: Runtime::Idle,
            active_prompt: None,
            queued_prompts: VecDeque::with_capacity(queue_capacity),
            queue_capacity,
            completed_history: Vec::new(),
            composer: String::new(),
            dialog: None,
            exit_armed_until: None,
            reset_pending: false,
        }
    }

    /// Applies one event and returns the runtime work required by its transition.
    pub fn reduce(&mut self, event: AppEvent) -> Vec<Effect> {
        match event {
            AppEvent::SubmitPrompt(prompt) => self.submit_prompt(prompt),
            AppEvent::TurnCompleted(output) => self.complete_turn(output),
            AppEvent::TurnCancelled | AppEvent::TurnFailed => {
                self.runtime = Runtime::Idle;
                self.active_prompt = None;
                self.disarm_exit();
                self.begin_next_queued_turn().into_iter().collect()
            }
            AppEvent::Command(command, now) => self.command(command, now),
            AppEvent::ResetSucceeded => self.reset_after_backend_success(),
            AppEvent::TimerTick(now) => {
                if self.exit_armed_until.is_some_and(|until| now >= until) {
                    self.disarm_exit();
                    vec![Effect::Render]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// Returns the active/idle runtime state.
    pub const fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// Returns queued prompts in their FIFO order.
    pub fn queued_prompts(&self) -> Vec<&str> {
        self.queued_prompts.iter().map(String::as_str).collect()
    }

    /// Returns only successfully completed prompt/output history.
    pub fn completed_history(&self) -> &[(String, String)] {
        &self.completed_history
    }

    pub fn set_composer(&mut self, composer: impl Into<String>) {
        self.composer = composer.into();
    }

    pub fn composer(&self) -> &str {
        &self.composer
    }

    pub fn set_dialog(&mut self, dialog: Option<Dialog>) {
        self.dialog = dialog;
        self.disarm_exit();
    }

    pub const fn dialog(&self) -> Option<&Dialog> {
        self.dialog.as_ref()
    }

    fn submit_prompt(&mut self, prompt: String) -> Vec<Effect> {
        if self.runtime == Runtime::Idle {
            self.active_prompt = Some(prompt.clone());
            self.runtime = Runtime::Running;
            return vec![Effect::StartPrompt(prompt)];
        }

        if self.queued_prompts.len() == self.queue_capacity {
            return vec![Effect::RefusePrompt(
                "A response is already in progress.".into(),
            )];
        }

        self.queued_prompts.push_back(prompt);
        Vec::new()
    }

    fn complete_turn(&mut self, output: String) -> Vec<Effect> {
        let Some(prompt) = self.active_prompt.take() else {
            return Vec::new();
        };

        self.completed_history
            .push((prompt.clone(), output.clone()));
        self.runtime = Runtime::Idle;
        let mut effects = vec![Effect::PersistCompleted { prompt, output }];

        if let Some(effect) = self.begin_next_queued_turn() {
            effects.push(effect);
        }

        effects
    }

    fn begin_next_queued_turn(&mut self) -> Option<Effect> {
        let next_prompt = self.queued_prompts.pop_front()?;

        self.active_prompt = Some(next_prompt.clone());
        self.runtime = Runtime::Running;

        Some(Effect::StartPrompt(next_prompt))
    }

    fn command(&mut self, command: Command, now: Instant) -> Vec<Effect> {
        if self.dialog.is_some() {
            return if command == Command::Escape {
                self.set_dialog(None);
                vec![Effect::Render]
            } else {
                vec![Effect::DialogCommand(command)]
            };
        }

        if command != Command::ControlC {
            self.disarm_exit();
        }

        if command == Command::ControlC {
            return self.control_c(now);
        }

        if self.runtime == Runtime::Running
            && matches!(
                command,
                Command::Model | Command::Effort | Command::Session | Command::Agent | Command::New
            )
        {
            return vec![Effect::RefuseCommand(RUNNING_REFUSAL.into())];
        }

        if command == Command::New {
            self.reset_pending = true;
            return vec![Effect::ResetConversation];
        }

        vec![Effect::Render]
    }

    fn control_c(&mut self, now: Instant) -> Vec<Effect> {
        if self.runtime == Runtime::Running {
            self.disarm_exit();
            return vec![Effect::CancelTurn];
        }

        if !self.composer.is_empty() {
            self.disarm_exit();
            self.composer.clear();
            return vec![Effect::Render];
        }

        if self.exit_armed_until.is_some_and(|until| now < until) {
            self.disarm_exit();
            return vec![Effect::Quit];
        }

        self.exit_armed_until = Some(now + EXIT_WARNING_WINDOW);
        vec![Effect::ExitWarning]
    }

    fn reset_after_backend_success(&mut self) -> Vec<Effect> {
        if !self.reset_pending {
            return Vec::new();
        }

        self.runtime = Runtime::Idle;
        self.active_prompt = None;
        self.queued_prompts.clear();
        self.completed_history.clear();
        self.composer.clear();
        self.reset_pending = false;
        self.disarm_exit();

        vec![Effect::Render]
    }

    fn disarm_exit(&mut self) {
        self.exit_armed_until = None;
    }
}

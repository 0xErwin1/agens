//! Pure prompt-queue state and effects for the terminal application.

use std::{collections::VecDeque, time::Instant};

use crate::Key;
use agens_core::{Message, MessagePart, Role};

const RUNNING_REFUSAL: &str = "This command is unavailable while a response is in progress.";
const QUEUE_FULL_REFUSAL: &str = "Prompt queue is full; draft was kept unchanged.";

fn text_message(text: String) -> Message {
    Message {
        role: Role::User,
        parts: (!text.is_empty())
            .then_some(MessagePart::Text(text))
            .into_iter()
            .collect(),
    }
}

/// Whether the application currently owns an active runtime turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Runtime {
    /// No turn is active.
    Idle,
    /// A turn is active; prompts may enter the fixed-capacity queue.
    Running,
}

static IDLE_RUNTIME: Runtime = Runtime::Idle;
static RUNNING_RUNTIME: Runtime = Runtime::Running;

/// Identifies the foreground route that owns a runtime turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveRoute {
    generation: u64,
    prompt: String,
}

impl ActiveRoute {
    fn new(generation: u64, prompt: String) -> Self {
        Self { generation, prompt }
    }

    /// Returns the monotonically increasing route generation.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the prompt that was dispatched for this route.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }
}

/// The lifecycle of the one foreground turn owned by the scheduler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnLifecycle {
    /// No foreground turn is active.
    Idle,
    /// A foreground route is running.
    Running(ActiveRoute),
    /// The active route has a pending cancellation request.
    Cancelling(ActiveRoute),
}

impl TurnLifecycle {
    /// Returns the route for either active lifecycle state.
    pub const fn active(&self) -> Option<&ActiveRoute> {
        match self {
            Self::Idle => None,
            Self::Running(route) | Self::Cancelling(route) => Some(route),
        }
    }
}

/// A stable FIFO entry that has not yet been dispatched to history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueEntry {
    id: u64,
    prompt: String,
    message: Message,
    resolved: bool,
}

/// A prompt-scheduler transition safe to expose to local diagnostics.
///
/// These variants intentionally carry no route, prompt, task, or user content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptTransition {
    Queued,
    Dequeued,
    Removed,
    CancellationRequested,
    CancellationConfirmed,
    StaleEventDropped,
    AutoTurnCoalesced,
}

/// Sanitized counters and transition history for the prompt scheduler.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PromptObservability {
    queued: u64,
    dequeued: u64,
    removed: u64,
    cancellation_requested: u64,
    cancellation_confirmed: u64,
    stale_event_dropped: u64,
    auto_turn_coalesced: u64,
    transitions: Vec<PromptTransition>,
}

impl PromptObservability {
    pub const fn queued(&self) -> u64 {
        self.queued
    }
    pub const fn dequeued(&self) -> u64 {
        self.dequeued
    }
    pub const fn removed(&self) -> u64 {
        self.removed
    }
    pub const fn cancellation_requested(&self) -> u64 {
        self.cancellation_requested
    }
    pub const fn cancellation_confirmed(&self) -> u64 {
        self.cancellation_confirmed
    }
    pub const fn stale_event_dropped(&self) -> u64 {
        self.stale_event_dropped
    }
    pub const fn auto_turn_coalesced(&self) -> u64 {
        self.auto_turn_coalesced
    }
    pub fn transitions(&self) -> &[PromptTransition] {
        &self.transitions
    }

    fn record(&mut self, transition: PromptTransition) {
        match transition {
            PromptTransition::Queued => self.queued += 1,
            PromptTransition::Dequeued => self.dequeued += 1,
            PromptTransition::Removed => self.removed += 1,
            PromptTransition::CancellationRequested => self.cancellation_requested += 1,
            PromptTransition::CancellationConfirmed => self.cancellation_confirmed += 1,
            PromptTransition::StaleEventDropped => self.stale_event_dropped += 1,
            PromptTransition::AutoTurnCoalesced => self.auto_turn_coalesced += 1,
        }
        self.transitions.push(transition);
    }
}

impl QueueEntry {
    fn new(id: u64, prompt: String) -> Self {
        let message = text_message(prompt.clone());
        Self {
            id,
            prompt,
            message,
            resolved: false,
        }
    }

    fn resolved_message(id: u64, display: String, message: Message) -> Self {
        Self {
            id,
            prompt: display,
            message,
            resolved: true,
        }
    }

    /// Returns this entry's stable identifier.
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Returns the queued prompt without adopting it into history.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// Returns the immutable canonical content owned by this queue entry.
    pub fn message(&self) -> &Message {
        &self.message
    }
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
    /// Queues a catalog-resolved provider prompt without exposing it in history.
    QueuePrompt {
        display: String,
        prompt: String,
    },
    /// Queues canonical ordered provider content without flattening media.
    QueueMessage {
        display: String,
        message: Message,
    },
    /// The active turn completed successfully with its final output.
    TurnCompletedFor {
        generation: u64,
        output: String,
    },
    /// The active turn was cancelled.
    TurnCancelledFor {
        generation: u64,
    },
    /// The active turn failed.
    TurnFailedFor {
        generation: u64,
    },
    /// The active scheduler-owned turn handed work to the background.
    TurnReleasedFor {
        generation: u64,
    },
    /// A background completion needs one deferred, coalesced internal turn.
    DeferAutoTurn,
    /// A terminal key routed through dialog, global, and composer handlers.
    Key(Key, Instant),
    Command(Command, Instant),
    ResetSucceeded,
    TimerTick(Instant),
}

/// Work requested by the reducer for the runtime adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    /// Begin a new runtime turn for this prompt.
    StartPrompt(String),
    /// Begin canonical ordered content owned by a queued entry.
    StartQueuedMessage {
        display: String,
        message: Message,
    },
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
    /// The open dialog consumed this key.
    DialogKey(Key),
    DialogCommand(Command),
    /// Focused composer editing changed its buffer.
    ComposerEdited,
    ResetConversation,
    RefuseCommand(String),
}

/// Application state whose prompt queue has a fixed capacity for its lifetime.
#[derive(Clone, Debug)]
pub struct AppState {
    lifecycle: TurnLifecycle,
    queued_prompts: VecDeque<QueueEntry>,
    queue_capacity: usize,
    next_generation: u64,
    next_queue_entry_id: u64,
    completed_history: Vec<(String, String)>,
    composer: String,
    dialog: Option<Dialog>,
    exit_armed_until: Option<Instant>,
    pending_auto_turns: usize,
    observability: PromptObservability,
}

impl PartialEq for AppState {
    fn eq(&self, other: &Self) -> bool {
        self.lifecycle == other.lifecycle
            && self.queued_prompts == other.queued_prompts
            && self.queue_capacity == other.queue_capacity
            && self.next_generation == other.next_generation
            && self.next_queue_entry_id == other.next_queue_entry_id
            && self.completed_history == other.completed_history
            && self.composer == other.composer
            && self.dialog == other.dialog
            && self.exit_armed_until == other.exit_armed_until
            && self.pending_auto_turns == other.pending_auto_turns
    }
}

impl Eq for AppState {}

impl AppState {
    /// Creates application state with a non-zero, fixed prompt queue capacity.
    pub fn new(queue_capacity: usize) -> Self {
        assert!(queue_capacity > 0, "prompt queue capacity must be non-zero");

        Self {
            lifecycle: TurnLifecycle::Idle,
            queued_prompts: VecDeque::with_capacity(queue_capacity),
            queue_capacity,
            next_generation: 1,
            next_queue_entry_id: 1,
            completed_history: Vec::new(),
            composer: String::new(),
            dialog: None,
            exit_armed_until: None,
            pending_auto_turns: 0,
            observability: PromptObservability::default(),
        }
    }

    /// Applies one event and returns the runtime work required by its transition.
    pub fn reduce(&mut self, event: AppEvent) -> Vec<Effect> {
        match event {
            AppEvent::SubmitPrompt(prompt) => self.submit_prompt(prompt),
            AppEvent::QueuePrompt { display, prompt } => self.queue_prompt(display, prompt),
            AppEvent::QueueMessage { display, message } => self.queue_message(display, message),
            AppEvent::TurnCompletedFor { generation, output } => {
                self.complete_turn(generation, output)
            }
            AppEvent::TurnCancelledFor { generation }
            | AppEvent::TurnFailedFor { generation }
            | AppEvent::TurnReleasedFor { generation } => self.terminate_turn(generation),
            AppEvent::DeferAutoTurn => {
                if self.pending_auto_turns > 0 {
                    self.observability
                        .record(PromptTransition::AutoTurnCoalesced);
                }
                self.pending_auto_turns = self.pending_auto_turns.saturating_add(1);
                Vec::new()
            }
            AppEvent::Key(key, now) => self.key(key, now),
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
    pub const fn runtime(&self) -> &'static Runtime {
        match self.lifecycle {
            TurnLifecycle::Idle => &IDLE_RUNTIME,
            TurnLifecycle::Running(_) | TurnLifecycle::Cancelling(_) => &RUNNING_RUNTIME,
        }
    }

    /// Returns the authoritative foreground lifecycle.
    pub const fn lifecycle(&self) -> &TurnLifecycle {
        &self.lifecycle
    }

    /// Returns queued prompts in their FIFO order.
    pub fn queued_prompts(&self) -> Vec<&str> {
        self.queued_prompts.iter().map(QueueEntry::prompt).collect()
    }

    /// Returns queued entries with stable identities in FIFO order.
    pub fn queued_entries(&self) -> Vec<&QueueEntry> {
        self.queued_prompts.iter().collect()
    }

    /// Removes an undispatched entry by its stable identity.
    pub fn remove_queue_entry(&mut self, id: u64) -> Option<QueueEntry> {
        let position = self
            .queued_prompts
            .iter()
            .position(|entry| entry.id == id)?;
        let entry = self.queued_prompts.remove(position);
        if entry.is_some() {
            self.observability.record(PromptTransition::Removed);
        }
        entry
    }

    /// Returns sanitized scheduler counters and content-free transition names.
    pub const fn observability(&self) -> &PromptObservability {
        &self.observability
    }

    /// Moves an undispatched entry one or more positions while preserving its identity.
    pub fn move_queue_entry(&mut self, id: u64, offset: isize) -> bool {
        let Some(position) = self.queued_prompts.iter().position(|entry| entry.id == id) else {
            return false;
        };
        let destination = position
            .saturating_add_signed(offset)
            .min(self.queued_prompts.len().saturating_sub(1));
        if position == destination {
            return false;
        }
        let entry = self
            .queued_prompts
            .remove(position)
            .expect("located queue entry exists");
        self.queued_prompts.insert(destination, entry);
        true
    }

    /// Returns only successfully completed prompt/output history.
    pub fn completed_history(&self) -> &[(String, String)] {
        &self.completed_history
    }

    pub fn set_composer(&mut self, composer: impl Into<String>) {
        self.composer = composer.into();
        self.disarm_exit();
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

    /// Starts one coalesced internal turn only after all user work is idle.
    pub fn take_ready_auto_turn(&mut self) -> Option<usize> {
        if self.pending_auto_turns == 0
            || self.lifecycle != TurnLifecycle::Idle
            || !self.queued_prompts.is_empty()
            || !self.composer.is_empty()
        {
            return None;
        }

        let finished = std::mem::take(&mut self.pending_auto_turns);
        let _ = self.begin_turn(String::new());
        Some(finished)
    }

    fn submit_prompt(&mut self, prompt: String) -> Vec<Effect> {
        self.disarm_exit();
        if self.lifecycle == TurnLifecycle::Idle {
            return vec![self.begin_turn(prompt)];
        }

        if self.queued_prompts.len() == self.queue_capacity {
            return vec![Effect::RefusePrompt(QUEUE_FULL_REFUSAL.into())];
        }

        let entry = QueueEntry::new(self.next_queue_entry_id, prompt);
        self.next_queue_entry_id += 1;
        self.queued_prompts.push_back(entry);
        self.observability.record(PromptTransition::Queued);
        Vec::new()
    }

    fn queue_prompt(&mut self, display: String, prompt: String) -> Vec<Effect> {
        self.queue_message(display, text_message(prompt))
    }

    fn queue_message(&mut self, display: String, message: Message) -> Vec<Effect> {
        self.disarm_exit();
        if self.lifecycle == TurnLifecycle::Idle {
            return vec![self.begin_turn(display)];
        }

        if self.queued_prompts.len() == self.queue_capacity {
            return vec![Effect::RefusePrompt(QUEUE_FULL_REFUSAL.into())];
        }

        let entry = QueueEntry::resolved_message(self.next_queue_entry_id, display, message);
        self.next_queue_entry_id += 1;
        self.queued_prompts.push_back(entry);
        self.observability.record(PromptTransition::Queued);
        Vec::new()
    }

    fn complete_turn(&mut self, generation: u64, output: String) -> Vec<Effect> {
        let Some(prompt) = self.active_prompt_for_generation(generation) else {
            self.observability
                .record(PromptTransition::StaleEventDropped);
            return Vec::new();
        };

        self.disarm_exit();
        self.completed_history
            .push((prompt.clone(), output.clone()));
        self.transition_to_idle();
        let mut effects = vec![Effect::PersistCompleted { prompt, output }];

        if let Some(effect) = self.begin_next_queued_turn() {
            effects.push(effect);
        }

        effects
    }

    fn terminate_turn(&mut self, generation: u64) -> Vec<Effect> {
        if self.active_prompt_for_generation(generation).is_none() {
            self.observability
                .record(PromptTransition::StaleEventDropped);
            return Vec::new();
        }

        if matches!(self.lifecycle, TurnLifecycle::Cancelling(_)) {
            self.observability
                .record(PromptTransition::CancellationConfirmed);
        }
        self.transition_to_idle();
        self.disarm_exit();
        self.begin_next_queued_turn().into_iter().collect()
    }

    fn active_prompt_for_generation(&self, generation: u64) -> Option<String> {
        self.lifecycle
            .active()
            .filter(|route| route.generation == generation)
            .map(|route| route.prompt.clone())
    }

    fn begin_next_queued_turn(&mut self) -> Option<Effect> {
        let entry = self.queued_prompts.pop_front()?;
        self.observability.record(PromptTransition::Dequeued);
        let display = entry.prompt;
        let effect = self.begin_turn(display.clone());
        if entry.resolved {
            Some(Effect::StartQueuedMessage {
                display,
                message: entry.message,
            })
        } else {
            Some(effect)
        }
    }

    fn begin_turn(&mut self, prompt: String) -> Effect {
        let generation = self.next_generation;
        self.next_generation += 1;
        self.lifecycle = TurnLifecycle::Running(ActiveRoute::new(generation, prompt.clone()));
        self.disarm_exit();

        Effect::StartPrompt(prompt)
    }

    fn transition_to_idle(&mut self) {
        self.lifecycle = TurnLifecycle::Idle;
    }

    fn command(&mut self, command: Command, now: Instant) -> Vec<Effect> {
        if let Some(effects) = self.handle_dialog_command(command) {
            return effects;
        }

        self.global_command(command, now)
    }

    fn key(&mut self, key: Key, now: Instant) -> Vec<Effect> {
        if let Some(effects) = self.handle_dialog_key(key) {
            return effects;
        }

        if let Some(effects) = self.handle_global_key(key, now) {
            return effects;
        }

        self.handle_composer_key(key)
    }

    fn global_command(&mut self, command: Command, now: Instant) -> Vec<Effect> {
        if self.is_unsafe_while_running(command) {
            return vec![Effect::RefuseCommand(RUNNING_REFUSAL.into())];
        }

        if command == Command::ControlC {
            return self.control_c(now);
        }

        self.disarm_exit();
        if command == Command::New {
            return vec![Effect::ResetConversation];
        }

        vec![Effect::Render]
    }

    fn handle_global_key(&mut self, key: Key, now: Instant) -> Option<Vec<Effect>> {
        if key == Key::CtrlC {
            return Some(self.global_command(Command::ControlC, now));
        }
        self.disarm_exit();
        None
    }

    fn is_unsafe_while_running(&self, command: Command) -> bool {
        !matches!(self.lifecycle, TurnLifecycle::Idle)
            && matches!(
                command,
                Command::Model | Command::Effort | Command::Session | Command::Agent | Command::New
            )
    }

    fn handle_dialog_command(&mut self, command: Command) -> Option<Vec<Effect>> {
        match (self.dialog.as_ref(), command) {
            (Some(Dialog::Command), Command::Escape) => {
                self.set_dialog(None);
                Some(vec![Effect::Render])
            }
            (Some(Dialog::Command), Command::Select) => Some(vec![Effect::DialogCommand(command)]),
            _ => None,
        }
    }

    fn handle_dialog_key(&mut self, key: Key) -> Option<Vec<Effect>> {
        let dialog = self.dialog.as_ref()?;

        match (dialog, key) {
            (Dialog::Command, Key::Escape) => {
                self.set_dialog(None);
                Some(vec![Effect::Render])
            }
            (_, Key::CtrlC) => None,
            _ => {
                self.disarm_exit();
                Some(vec![Effect::DialogKey(key)])
            }
        }
    }

    fn handle_composer_key(&mut self, key: Key) -> Vec<Effect> {
        match key {
            Key::Char(character) => {
                self.composer.push(character);
                vec![Effect::ComposerEdited]
            }
            Key::Backspace => {
                self.composer.pop();
                vec![Effect::ComposerEdited]
            }
            Key::ShiftEnter => {
                self.composer.push('\n');
                vec![Effect::ComposerEdited]
            }
            _ => vec![Effect::Render],
        }
    }

    fn control_c(&mut self, now: Instant) -> Vec<Effect> {
        if let TurnLifecycle::Running(route) = &self.lifecycle {
            self.lifecycle = TurnLifecycle::Cancelling(route.clone());
            self.disarm_exit();
            self.observability
                .record(PromptTransition::CancellationRequested);
            return vec![Effect::CancelTurn];
        }
        if matches!(self.lifecycle, TurnLifecycle::Cancelling(_)) {
            self.disarm_exit();
            return vec![Effect::Render];
        }
        if self.exit_armed_until.is_some_and(|until| now < until) {
            self.disarm_exit();
            return vec![Effect::Quit];
        }

        self.exit_armed_until = Some(now + crate::EXIT_WARNING_WINDOW);
        vec![Effect::ExitWarning]
    }

    fn reset_after_backend_success(&mut self) -> Vec<Effect> {
        self.transition_to_idle();
        self.queued_prompts.clear();
        self.completed_history.clear();
        self.composer.clear();
        self.dialog = None;
        self.disarm_exit();

        vec![Effect::Render]
    }

    fn disarm_exit(&mut self) {
        self.exit_armed_until = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_running_commands_refuse_without_changing_state() {
        let now = Instant::now();
        for command in [
            Command::Model,
            Command::Effort,
            Command::Session,
            Command::Agent,
            Command::New,
        ] {
            let mut app = AppState::new(1);
            let _ = app.reduce(AppEvent::SubmitPrompt("active".into()));
            let before = app.clone();

            assert_eq!(
                app.reduce(AppEvent::Command(command, now)),
                vec![Effect::RefuseCommand(RUNNING_REFUSAL.into())]
            );
            assert_eq!(app, before);
        }
    }
}

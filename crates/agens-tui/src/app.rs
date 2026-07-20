//! Pure prompt-queue state and effects for the terminal application.

use std::collections::VecDeque;

/// Whether the application currently owns an active runtime turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Runtime {
    /// No turn is active.
    Idle,
    /// A turn is active; prompts may enter the fixed-capacity queue.
    Running,
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
}

/// Work requested by the reducer for the runtime adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    /// Begin a new runtime turn for this prompt.
    StartPrompt(String),
    /// Persist a successfully completed prompt and output pair.
    PersistCompleted { prompt: String, output: String },
    /// Present a deterministic refusal without mutating history.
    RefusePrompt(String),
}

/// Application state whose prompt queue has a fixed capacity for its lifetime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppState {
    runtime: Runtime,
    active_prompt: Option<String>,
    queued_prompts: VecDeque<String>,
    queue_capacity: usize,
    completed_history: Vec<(String, String)>,
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
                self.begin_next_queued_turn().into_iter().collect()
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
}

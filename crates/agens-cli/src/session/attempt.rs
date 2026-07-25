//! Session attempt lifecycle: registering an attempt as locally active, running it to
//! completion or failure, and recovering an attempt left running by a crashed or killed process.

use std::fmt;
use std::sync::{Mutex, OnceLock};

use agens_core::{
    AttemptKey, BeginSessionAttemptError, CompletedSessionTurn, CompletedTurnSnapshot, Message,
    MessagePart, RecoveryOutcome, Role, SessionMessage, SessionMessageError, SessionMetadata,
};
use agens_store::SessionStore;

use crate::error::CliError;

#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct AttemptActivityRegistry {
    active: Mutex<Vec<AttemptKey>>,
}

static ACTIVE_SESSION_ATTEMPTS: OnceLock<AttemptActivityRegistry> = OnceLock::new();

pub(crate) fn active_session_attempts() -> &'static AttemptActivityRegistry {
    ACTIVE_SESSION_ATTEMPTS.get_or_init(AttemptActivityRegistry::default)
}

#[allow(dead_code)]
impl AttemptActivityRegistry {
    pub(crate) fn begin_and_register(
        &self,
        store: &mut SessionStore,
        metadata: &SessionMetadata,
        prompt: String,
    ) -> Result<agens_core::SessionAttemptSummary, BeginSessionAttemptError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| BeginSessionAttemptError::Store)?;
        let attempt = store.begin_session_attempt(metadata, prompt)?;
        active.push(attempt.key());
        Ok(attempt)
    }

    pub(crate) fn contains(&self, key: AttemptKey) -> bool {
        self.active.lock().is_ok_and(|active| active.contains(&key))
    }

    pub(crate) fn unregister(&self, key: AttemptKey) {
        if let Ok(mut active) = self.active.lock()
            && let Some(index) = active.iter().position(|active_key| *active_key == key)
        {
            active.remove(index);
        }
    }

    pub(crate) fn recover_running_attempt(
        &self,
        store: &mut SessionStore,
        key: AttemptKey,
        finished_at: i64,
    ) -> Result<Option<RecoveryOutcome>, ()> {
        let active = self.active.lock().map_err(|_| ())?;
        if active.contains(&key) {
            return Ok(None);
        }

        store
            .recover_running_attempt(key, finished_at)
            .map(Some)
            .map_err(|_| ())
    }
}

struct RegisteredAttempt<'a> {
    registry: &'a AttemptActivityRegistry,
    key: AttemptKey,
}

impl Drop for RegisteredAttempt<'_> {
    fn drop(&mut self) {
        self.registry.unregister(self.key);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AttemptLifecycleError {
    Begin(BeginSessionAttemptError),
    Runtime {
        error: CliError,
        partial: Option<Box<PartialTurnRecord>>,
    },
}

impl AttemptLifecycleError {
    pub(crate) fn runtime(error: CliError) -> Self {
        Self::Runtime {
            error,
            partial: None,
        }
    }
}

/// History persisted for an attempt that ended without a completed turn, carried out of the
/// failing path so the caller can keep owning the same session instead of minting a new one.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PartialTurnRecord {
    pub(crate) metadata: SessionMetadata,
    pub(crate) messages: Vec<Message>,
}

impl fmt::Debug for PartialTurnRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PartialTurnRecord")
            .field("session", &self.metadata.id)
            .field("messages", &self.messages.len())
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct SessionAttemptCompletion {
    pub(crate) snapshot: CompletedTurnSnapshot,
    pub(crate) metadata: SessionMetadata,
    pub(crate) messages: Vec<Message>,
}

#[allow(dead_code)]
pub(crate) enum ExplicitAttemptRecoveryOutcome {
    LocallyActive,
    Stale,
    Recovered(Box<SessionAttemptCompletion>),
}

impl fmt::Debug for ExplicitAttemptRecoveryOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = match self {
            Self::LocallyActive => "LocallyActive",
            Self::Stale => "Stale",
            Self::Recovered(_) => "Recovered",
        };

        formatter.write_str(status)
    }
}

#[allow(dead_code)]
pub(crate) fn recover_session_attempt_lifecycle(
    registry: &AttemptActivityRegistry,
    store: &mut SessionStore,
    key: AttemptKey,
    finished_at: i64,
    runtime: impl FnOnce(
        Vec<Message>,
        &str,
        &SessionMetadata,
    ) -> Result<(CompletedTurnSnapshot, CompletedSessionTurn), CliError>,
) -> Result<ExplicitAttemptRecoveryOutcome, AttemptLifecycleError> {
    let Some(recovery) = registry
        .recover_running_attempt(store, key, finished_at)
        .map_err(|_| {
            AttemptLifecycleError::runtime(CliError::storage("attempt recovery failed"))
        })?
    else {
        return Ok(ExplicitAttemptRecoveryOutcome::LocallyActive);
    };
    if recovery == RecoveryOutcome::Stale {
        return Ok(ExplicitAttemptRecoveryOutcome::Stale);
    }

    let boundary = store
        .load_retry_boundary(key)
        .map_err(|_| AttemptLifecycleError::runtime(CliError::storage("attempt recovery failed")))?
        .ok_or_else(|| {
            AttemptLifecycleError::runtime(CliError::storage("attempt recovery failed"))
        })?;
    let stored = store
        .load_session_for_resume(key.session_id())
        .map_err(|_| {
            AttemptLifecycleError::runtime(CliError::storage("attempt recovery failed"))
        })?;
    let metadata = stored.metadata;
    let runtime_metadata = metadata.clone();
    let history = stored.messages;
    let prompt = boundary.prompt().to_owned();
    let completion =
        run_session_attempt_lifecycle(registry, store, metadata, prompt.clone(), || {
            runtime(history, &prompt, &runtime_metadata)
        })?;

    Ok(ExplicitAttemptRecoveryOutcome::Recovered(Box::new(
        completion,
    )))
}

pub(crate) fn run_session_attempt_lifecycle(
    registry: &AttemptActivityRegistry,
    store: &mut SessionStore,
    metadata: SessionMetadata,
    prompt: String,
    runtime: impl FnOnce() -> Result<(CompletedTurnSnapshot, CompletedSessionTurn), CliError>,
) -> Result<SessionAttemptCompletion, AttemptLifecycleError> {
    run_session_attempt_lifecycle_with_terminal_writer(
        registry,
        store,
        metadata,
        prompt,
        runtime,
        |store, write| {
            write_terminal_attempt(store, write, &crate::headless::interrupted_turn_note(&[]))
        },
    )
}

/// Terminal state of an attempt whose runtime failed, handed to the writer that records it.
pub(crate) struct TerminalAttemptWrite<'a> {
    key: AttemptKey,
    pub(crate) status: agens_core::SessionAttemptStatus,
    metadata: &'a SessionMetadata,
    prompt: &'a str,
    finished_at: i64,
}

pub(crate) fn run_session_attempt_lifecycle_with_terminal_writer(
    registry: &AttemptActivityRegistry,
    store: &mut SessionStore,
    mut metadata: SessionMetadata,
    prompt: String,
    runtime: impl FnOnce() -> Result<(CompletedTurnSnapshot, CompletedSessionTurn), CliError>,
    terminal_writer: impl FnOnce(
        &mut SessionStore,
        TerminalAttemptWrite<'_>,
    ) -> Result<Option<PartialTurnRecord>, ()>,
) -> Result<SessionAttemptCompletion, AttemptLifecycleError> {
    let attempt = registry
        .begin_and_register(store, &metadata, prompt.clone())
        .map_err(AttemptLifecycleError::Begin)?;
    let _registered = RegisteredAttempt {
        registry,
        key: attempt.key(),
    };
    metadata.id = attempt.key().session_id();

    let (snapshot, turn) = match runtime() {
        Ok(completion) => completion,
        Err(error) => {
            let partial = terminal_writer(
                store,
                TerminalAttemptWrite {
                    key: attempt.key(),
                    status: attempt_failure_status(&error),
                    metadata: &metadata,
                    prompt: &prompt,
                    finished_at: crate::current_session_timestamp(),
                },
            )
            .ok()
            .flatten();

            return Err(AttemptLifecycleError::Runtime {
                error,
                partial: partial.map(Box::new),
            });
        }
    };

    match store
        .persist_completed_session_attempt(
            attempt.key(),
            &metadata,
            &turn,
            crate::current_session_timestamp(),
        )
        .map_err(|error| {
            CliError::storage(format!("completed session could not be saved: {error}"))
        })
        .map_err(AttemptLifecycleError::runtime)?
    {
        agens_core::AttemptFinishOutcome::Finished => {}
        agens_core::AttemptFinishOutcome::Stale => {
            return Err(AttemptLifecycleError::runtime(CliError::storage(
                "completed session could not be saved",
            )));
        }
    }

    let stored = store
        .load_session_for_resume(metadata.id)
        .map_err(|_| CliError::storage("completed session could not be loaded"))
        .map_err(AttemptLifecycleError::runtime)?;

    Ok(SessionAttemptCompletion {
        snapshot,
        metadata: stored.metadata,
        messages: stored.messages,
    })
}

pub(crate) fn attempt_failure_status(error: &CliError) -> agens_core::SessionAttemptStatus {
    match error.category {
        "cancelled" | "timeout" => agens_core::SessionAttemptStatus::Cancelled,
        "auth" | "provider" => agens_core::SessionAttemptStatus::ProviderError,
        _ => agens_core::SessionAttemptStatus::Failed,
    }
}

/// Records an interrupted attempt (explicit cancellation or an expired deadline) as history, so
/// the next turn keeps the prompt and knows the turn stopped early. Every other failure keeps its
/// retained retry prompt instead, because its recovery path replays that prompt rather than
/// continuing the conversation.
pub(crate) fn write_terminal_attempt(
    store: &mut SessionStore,
    write: TerminalAttemptWrite<'_>,
    note: &str,
) -> Result<Option<PartialTurnRecord>, ()> {
    if write.status != agens_core::SessionAttemptStatus::Cancelled {
        return store
            .finish_session_attempt(write.key, write.status, write.finished_at)
            .map(|_| None)
            .map_err(|_| ());
    }

    let turn = interrupted_session_turn(write.prompt, note).map_err(|_| ())?;
    let outcome = store
        .persist_partial_session_attempt(
            write.key,
            write.metadata,
            &turn,
            write.status,
            write.finished_at,
        )
        .map_err(|_| ())?;
    if outcome == agens_core::AttemptFinishOutcome::Stale {
        return Ok(None);
    }

    let stored = store
        .load_session_for_resume(write.metadata.id)
        .map_err(|_| ())?;

    Ok(Some(PartialTurnRecord {
        metadata: stored.metadata,
        messages: stored.messages,
    }))
}

/// Keeps the interrupted turn to the prompt and a plain assistant note: a tool call that never
/// answered must not gain a fabricated result, because the tool may already have changed the
/// project and claiming otherwise would assert something unverified.
fn interrupted_session_turn(
    prompt: &str,
    note: &str,
) -> Result<CompletedSessionTurn, SessionMessageError> {
    let messages = [
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text(prompt.to_owned())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text(note.to_owned())],
        },
    ]
    .into_iter()
    .map(SessionMessage::try_from)
    .collect::<Result<Vec<_>, _>>()?;

    CompletedSessionTurn::new(messages).map_err(|_| SessionMessageError::EmptyParts)
}

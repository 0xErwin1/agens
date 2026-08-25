//! Session attempt lifecycle: registering an attempt as locally active, running it to
//! completion or failure, and recovering an attempt left running by a crashed or killed process.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use agens_core::{
    AttemptKey, BeginSessionAttemptError, CompletedSessionTurn, CompletedTurnSnapshot, Message,
    MessagePart, RecoveryOutcome, Role, SessionMessage, SessionMessageError, SessionMetadata,
};
use agens_store::SessionStore;

use agens_error::CliError;

/// An `AttemptKey` is only a small SQLite autoincrement pair (`session_id`, `attempt_id`) with no
/// discriminator for which database it came from. Two independent `SessionStore`s — each its own
/// `agens.db` file — assign the SAME small keys starting from their own 1, so scoping this
/// registry by `AttemptKey` alone lets one database's `RegisteredAttempt::drop` unregister a
/// DIFFERENT database's still-active attempt that happens to share the same key. Today one
/// process opens one data directory, so this never collides in practice; it becomes reachable the
/// moment one process serves more than one database (for example one daemon process serving
/// several projects, each with its own data directory).
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScopedAttemptKey {
    database_path: PathBuf,
    key: AttemptKey,
}

/// A store operation on an attempt did not complete. Carries no detail on
/// purpose — the store already reported the specific failure — but is a named
/// type rather than `()` so a caller can tell it apart from a unit success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttemptStoreError;

impl std::fmt::Display for AttemptStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("attempt store operation failed")
    }
}

impl std::error::Error for AttemptStoreError {}

#[allow(dead_code)]
#[derive(Default)]
pub struct AttemptActivityRegistry {
    active: Mutex<Vec<ScopedAttemptKey>>,
}

static ACTIVE_SESSION_ATTEMPTS: OnceLock<AttemptActivityRegistry> = OnceLock::new();

pub fn active_session_attempts() -> &'static AttemptActivityRegistry {
    ACTIVE_SESSION_ATTEMPTS.get_or_init(AttemptActivityRegistry::default)
}

#[allow(dead_code)]
impl AttemptActivityRegistry {
    pub fn begin_and_register(
        &self,
        store: &mut SessionStore,
        metadata: &SessionMetadata,
        prompt: String,
    ) -> Result<agens_core::SessionAttemptSummary, BeginSessionAttemptError> {
        self.begin_and_register_with_media(store, metadata, prompt, Vec::new())
    }

    pub fn begin_and_register_with_media(
        &self,
        store: &mut SessionStore,
        metadata: &SessionMetadata,
        prompt: String,
        media_ids: Vec<i64>,
    ) -> Result<agens_core::SessionAttemptSummary, BeginSessionAttemptError> {
        let attempt = store.begin_session_attempt_with_media(metadata, prompt, media_ids)?;
        self.register(store, attempt)
    }

    pub fn begin_and_register_with_user_message(
        &self,
        store: &mut SessionStore,
        metadata: &SessionMetadata,
        user_message: &SessionMessage,
    ) -> Result<agens_core::SessionAttemptSummary, BeginSessionAttemptError> {
        let attempt = store.begin_session_attempt_with_user_message(metadata, user_message)?;
        self.register(store, attempt)
    }

    fn register(
        &self,
        store: &SessionStore,
        attempt: agens_core::SessionAttemptSummary,
    ) -> Result<agens_core::SessionAttemptSummary, BeginSessionAttemptError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| BeginSessionAttemptError::Store)?;
        active.push(ScopedAttemptKey {
            database_path: store.database_path(),
            key: attempt.key(),
        });
        Ok(attempt)
    }

    pub fn contains(&self, database_path: &std::path::Path, key: AttemptKey) -> bool {
        self.active.lock().is_ok_and(|active| {
            active
                .iter()
                .any(|scoped| scoped.database_path == database_path && scoped.key == key)
        })
    }

    pub fn unregister(&self, database_path: &std::path::Path, key: AttemptKey) {
        if let Ok(mut active) = self.active.lock()
            && let Some(index) = active
                .iter()
                .position(|scoped| scoped.database_path == database_path && scoped.key == key)
        {
            active.remove(index);
        }
    }

    pub fn recover_running_attempt(
        &self,
        store: &mut SessionStore,
        key: AttemptKey,
        finished_at: i64,
    ) -> Result<Option<RecoveryOutcome>, AttemptStoreError> {
        let database_path = store.database_path();
        let active = self.active.lock().map_err(|_| AttemptStoreError)?;
        if active
            .iter()
            .any(|scoped| scoped.database_path == database_path && scoped.key == key)
        {
            return Ok(None);
        }

        store
            .recover_running_attempt(key, finished_at)
            .map(Some)
            .map_err(|_| AttemptStoreError)
    }
}

struct RegisteredAttempt<'a> {
    registry: &'a AttemptActivityRegistry,
    database_path: PathBuf,
    key: AttemptKey,
}

impl Drop for RegisteredAttempt<'_> {
    fn drop(&mut self) {
        self.registry.unregister(&self.database_path, self.key);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AttemptLifecycleError {
    Begin(BeginSessionAttemptError),
    Runtime {
        error: CliError,
        partial: Option<Box<PartialTurnRecord>>,
    },
}

impl AttemptLifecycleError {
    pub fn runtime(error: CliError) -> Self {
        Self::Runtime {
            error,
            partial: None,
        }
    }
}

/// History persisted for an attempt that ended without a completed turn, carried out of the
/// failing path so the caller can keep owning the same session instead of minting a new one.
#[derive(Clone, PartialEq, Eq)]
pub struct PartialTurnRecord {
    pub metadata: SessionMetadata,
    pub messages: Vec<Message>,
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
pub struct SessionAttemptCompletion {
    pub snapshot: CompletedTurnSnapshot,
    pub metadata: SessionMetadata,
    pub messages: Vec<Message>,
}

#[allow(dead_code)]
pub enum ExplicitAttemptRecoveryOutcome {
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
pub fn recover_session_attempt_lifecycle(
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
    let completion = run_session_attempt_lifecycle(
        registry,
        store,
        metadata,
        prompt.clone(),
        INTERRUPTED_NOTE,
        || runtime(history, &prompt, &runtime_metadata),
    )?;

    Ok(ExplicitAttemptRecoveryOutcome::Recovered(Box::new(
        completion,
    )))
}

const INTERRUPTED_NOTE: &str = "[interrupted] test note";

/// `interrupted_note` is passed in rather than composed here: it is text for a
/// person, and this layer records attempts, it does not write to anyone.
pub fn run_session_attempt_lifecycle(
    registry: &AttemptActivityRegistry,
    store: &mut SessionStore,
    metadata: SessionMetadata,
    prompt: String,
    interrupted_note: &str,
    runtime: impl FnOnce() -> Result<(CompletedTurnSnapshot, CompletedSessionTurn), CliError>,
) -> Result<SessionAttemptCompletion, AttemptLifecycleError> {
    run_session_attempt_lifecycle_with_terminal_writer(
        registry,
        store,
        metadata,
        prompt,
        Vec::new(),
        |_attempt| runtime(),
        |store, write| write_terminal_attempt(store, write, &[], interrupted_note),
    )
}

/// Terminal state of an attempt whose runtime failed, handed to the writer that records it.
pub struct TerminalAttemptWrite<'a> {
    key: AttemptKey,
    pub status: agens_core::SessionAttemptStatus,
    metadata: &'a SessionMetadata,
    prompt: &'a str,
    finished_at: i64,
}

pub fn run_session_attempt_lifecycle_with_terminal_writer(
    registry: &AttemptActivityRegistry,
    store: &mut SessionStore,
    metadata: SessionMetadata,
    prompt: String,
    media_ids: Vec<i64>,
    runtime: impl FnOnce(AttemptKey) -> Result<(CompletedTurnSnapshot, CompletedSessionTurn), CliError>,
    terminal_writer: impl FnOnce(
        &mut SessionStore,
        TerminalAttemptWrite<'_>,
    ) -> Result<Option<PartialTurnRecord>, AttemptStoreError>,
) -> Result<SessionAttemptCompletion, AttemptLifecycleError> {
    run_session_attempt_lifecycle_inner(
        registry,
        store,
        metadata,
        AttemptUser::Legacy { prompt, media_ids },
        runtime,
        terminal_writer,
    )
}

pub fn run_session_attempt_lifecycle_with_user_message(
    registry: &AttemptActivityRegistry,
    store: &mut SessionStore,
    metadata: SessionMetadata,
    user_message: &SessionMessage,
    runtime: impl FnOnce(AttemptKey) -> Result<(CompletedTurnSnapshot, CompletedSessionTurn), CliError>,
    terminal_writer: impl FnOnce(
        &mut SessionStore,
        TerminalAttemptWrite<'_>,
    ) -> Result<Option<PartialTurnRecord>, AttemptStoreError>,
) -> Result<SessionAttemptCompletion, AttemptLifecycleError> {
    run_session_attempt_lifecycle_inner(
        registry,
        store,
        metadata,
        AttemptUser::Canonical(user_message),
        runtime,
        terminal_writer,
    )
}

enum AttemptUser<'a> {
    Legacy { prompt: String, media_ids: Vec<i64> },
    Canonical(&'a SessionMessage),
}

fn run_session_attempt_lifecycle_inner(
    registry: &AttemptActivityRegistry,
    store: &mut SessionStore,
    mut metadata: SessionMetadata,
    user: AttemptUser<'_>,
    runtime: impl FnOnce(AttemptKey) -> Result<(CompletedTurnSnapshot, CompletedSessionTurn), CliError>,
    terminal_writer: impl FnOnce(
        &mut SessionStore,
        TerminalAttemptWrite<'_>,
    ) -> Result<Option<PartialTurnRecord>, AttemptStoreError>,
) -> Result<SessionAttemptCompletion, AttemptLifecycleError> {
    let (attempt, prompt) = match user {
        AttemptUser::Canonical(user_message) => {
            let prompt = user_message
                .as_message()
                .parts
                .iter()
                .filter_map(|part| match part {
                    MessagePart::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            (
                registry.begin_and_register_with_user_message(store, &metadata, user_message),
                prompt,
            )
        }
        AttemptUser::Legacy { prompt, media_ids } => (
            registry.begin_and_register_with_media(store, &metadata, prompt.clone(), media_ids),
            prompt,
        ),
    };
    let attempt = attempt.map_err(AttemptLifecycleError::Begin)?;
    let _registered = RegisteredAttempt {
        registry,
        database_path: store.database_path(),
        key: attempt.key(),
    };
    metadata.id = attempt.key().session_id();

    let (snapshot, turn) = match runtime(attempt.key()) {
        Ok(completion) => completion,
        Err(mut error) => {
            let partial = match terminal_writer(
                store,
                TerminalAttemptWrite {
                    key: attempt.key(),
                    status: attempt_failure_status(&error),
                    metadata: &metadata,
                    prompt: &prompt,
                    finished_at: crate::context::current_session_timestamp(),
                },
            ) {
                Ok(partial) => partial,
                Err(_) => {
                    error
                        .message
                        .push_str("; failed turn history could not be saved");
                    None
                }
            };

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
            crate::context::current_session_timestamp(),
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

pub fn attempt_failure_status(error: &CliError) -> agens_core::SessionAttemptStatus {
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
pub fn write_terminal_attempt(
    store: &mut SessionStore,
    write: TerminalAttemptWrite<'_>,
    directives: &[Message],
    note: &str,
) -> Result<Option<PartialTurnRecord>, AttemptStoreError> {
    if write.status != agens_core::SessionAttemptStatus::Cancelled {
        return store
            .finish_session_attempt(write.key, write.status, write.finished_at)
            .map(|_| None)
            .map_err(|_| AttemptStoreError);
    }

    let turn =
        interrupted_session_turn(write.prompt, directives, note).map_err(|_| AttemptStoreError)?;
    write_terminal_attempt_with_history(store, write, &turn)
}

/// Persists the actual message history observed before a failed attempt stopped, regardless of
/// failure category, and returns the reloaded session so the caller can keep using the same one.
pub fn write_terminal_attempt_with_history(
    store: &mut SessionStore,
    write: TerminalAttemptWrite<'_>,
    turn: &CompletedSessionTurn,
) -> Result<Option<PartialTurnRecord>, AttemptStoreError> {
    let outcome = store
        .persist_partial_session_attempt(
            write.key,
            write.metadata,
            turn,
            write.status,
            write.finished_at,
        )
        .map_err(|_| AttemptStoreError)?;
    if outcome == agens_core::AttemptFinishOutcome::Stale {
        return Ok(None);
    }

    let stored = store
        .load_session_for_resume(write.metadata.id)
        .map_err(|_| AttemptStoreError)?;

    Ok(Some(PartialTurnRecord {
        metadata: stored.metadata,
        messages: stored.messages,
    }))
}

/// Keeps the interrupted turn to the directives it had already delivered, the prompt, and a plain
/// assistant note: a tool call that never answered must not gain a fabricated result, because the
/// tool may already have changed the project and claiming otherwise would assert something
/// unverified.
fn interrupted_session_turn(
    prompt: &str,
    directives: &[Message],
    note: &str,
) -> Result<CompletedSessionTurn, SessionMessageError> {
    let mut messages = directives.to_vec();
    messages.extend([
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text(prompt.to_owned())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text(note.to_owned())],
        },
    ]);
    let messages = messages
        .into_iter()
        .map(SessionMessage::try_from)
        .collect::<Result<Vec<_>, _>>()?;

    CompletedSessionTurn::new(messages).map_err(|_| SessionMessageError::EmptyParts)
}

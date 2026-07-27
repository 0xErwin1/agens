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

use crate::error::CliError;

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

#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct AttemptActivityRegistry {
    active: Mutex<Vec<ScopedAttemptKey>>,
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
        active.push(ScopedAttemptKey {
            database_path: store.database_path(),
            key: attempt.key(),
        });
        Ok(attempt)
    }

    pub(crate) fn contains(&self, database_path: &std::path::Path, key: AttemptKey) -> bool {
        self.active.lock().is_ok_and(|active| {
            active
                .iter()
                .any(|scoped| scoped.database_path == database_path && scoped.key == key)
        })
    }

    pub(crate) fn unregister(&self, database_path: &std::path::Path, key: AttemptKey) {
        if let Ok(mut active) = self.active.lock()
            && let Some(index) = active
                .iter()
                .position(|scoped| scoped.database_path == database_path && scoped.key == key)
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
        let database_path = store.database_path();
        let active = self.active.lock().map_err(|_| ())?;
        if active
            .iter()
            .any(|scoped| scoped.database_path == database_path && scoped.key == key)
        {
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
    database_path: PathBuf,
    key: AttemptKey,
}

impl Drop for RegisteredAttempt<'_> {
    fn drop(&mut self) {
        self.registry.unregister(&self.database_path, self.key);
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
        |_attempt| runtime(),
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
    runtime: impl FnOnce(AttemptKey) -> Result<(CompletedTurnSnapshot, CompletedSessionTurn), CliError>,
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
        database_path: store.database_path(),
        key: attempt.key(),
    };
    metadata.id = attempt.key().session_id();

    let (snapshot, turn) = match runtime(attempt.key()) {
        Ok(completion) => completion,
        Err(error) => {
            let partial = terminal_writer(
                store,
                TerminalAttemptWrite {
                    key: attempt.key(),
                    status: attempt_failure_status(&error),
                    metadata: &metadata,
                    prompt: &prompt,
                    finished_at: crate::tui::session::current_session_timestamp(),
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
            crate::tui::session::current_session_timestamp(),
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

#[cfg(test)]
mod tests {
    use agens_core::{HeadlessTurnError, PermissionMode, TurnEvent, TurnState};
    use agens_providers::OpenAiResponsesProvider;
    use agens_tui::{
        Engine as TuiEngine, Tui, TuiExecutionEvent, TuiRuntimeEvent, TuiSubagentEvent,
    };

    use super::*;
    use crate::HeadlessChatRequest;
    use crate::headless::{
        HeadlessChatFailure, RequestedSubagent, interrupted_turn_note, provider_messages,
        record_requested_subagent,
    };
    use crate::tools::runtime::production_dangerous_child_tool_runtime;
    use crate::tui::router::tui_provider_outcome;
    use crate::tui::session::TuiSessionContext;
    use crate::tui::turn::complete_tui_turn;
    use crate::turns::completed_session_turn;

    /// Two independent SQLite databases both autoincrement `(session_id, attempt_id)` from 1, so
    /// the SAME `AttemptKey` is reachable from two entirely unrelated sessions once more than one
    /// database is in play in a single process — exactly what a daemon serving multiple projects'
    /// data directories would do. The registry must not treat these as the same active attempt.
    #[test]
    fn the_registry_scopes_active_attempts_by_database_not_just_by_attempt_key() {
        let directory_x =
            std::env::temp_dir().join(format!("agens-registry-scope-x-{}", std::process::id()));
        let directory_y =
            std::env::temp_dir().join(format!("agens-registry-scope-y-{}", std::process::id()));
        std::fs::create_dir_all(&directory_x).unwrap();
        std::fs::create_dir_all(&directory_y).unwrap();
        let mut store_x = SessionStore::open(&directory_x).unwrap();
        let mut store_y = SessionStore::open(&directory_y).unwrap();
        let metadata = SessionMetadata {
            id: 1,
            project: "project".into(),
            title: "title".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
        };
        let registry = AttemptActivityRegistry::default();

        let attempt_x = registry
            .begin_and_register(&mut store_x, &metadata, "prompt-x".into())
            .unwrap();
        let attempt_y = registry
            .begin_and_register(&mut store_y, &metadata, "prompt-y".into())
            .unwrap();
        assert_eq!(
            attempt_x.key(),
            attempt_y.key(),
            "two fresh, unrelated databases must assign the same small AttemptKey, which is the \
             precondition for the collision this test guards against"
        );

        registry.unregister(&store_x.database_path(), attempt_x.key());

        assert!(
            !registry.contains(&store_x.database_path(), attempt_x.key()),
            "unregistering X's own attempt must deactivate it"
        );
        assert!(
            registry.contains(&store_y.database_path(), attempt_y.key()),
            "unregistering X's attempt must not deactivate Y's colliding key from a DIFFERENT \
             database"
        );

        std::fs::remove_dir_all(&directory_x).unwrap();
        std::fs::remove_dir_all(&directory_y).unwrap();
    }

    #[test]
    fn turn_attempt_registry_blocks_same_session_begin_and_preserves_primary_errors() {
        let directory =
            std::env::temp_dir().join(format!("agens-attempt-registry-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let metadata = SessionMetadata {
            id: 1,
            project: "project".into(),
            title: "title".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
        };
        let mut store = SessionStore::open(&directory).unwrap();
        let registry = AttemptActivityRegistry::default();
        let provider_calls = std::sync::atomic::AtomicUsize::new(0);

        let attempt = registry
            .begin_and_register(&mut store, &metadata, "prompt".into())
            .unwrap();
        let second = run_session_attempt_lifecycle(
            &registry,
            &mut store,
            metadata.clone(),
            "second".into(),
            || {
                provider_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Err(CliError::runtime(HeadlessTurnError::Provider))
            },
        );

        assert!(matches!(
            second,
            Err(AttemptLifecycleError::Begin(
                BeginSessionAttemptError::AlreadyRunning(_)
            ))
        ));
        assert_eq!(provider_calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(registry.contains(&store.database_path(), attempt.key()));
        registry.unregister(&store.database_path(), attempt.key());
        assert!(!registry.contains(&store.database_path(), attempt.key()));

        let mut unrelated = metadata.clone();
        unrelated.id = 2;
        let primary_error = run_session_attempt_lifecycle(
            &registry,
            &mut store,
            unrelated,
            "unrelated".into(),
            || Err(CliError::runtime(HeadlessTurnError::Provider)),
        )
        .unwrap_err();

        assert_eq!(
            primary_error,
            AttemptLifecycleError::runtime(CliError::runtime(HeadlessTurnError::Provider))
        );
        assert!(!registry.contains(&store.database_path(), attempt.key()));
        assert_eq!(
            store
                .load_session_for_resume(2)
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::ProviderError
        );

        let mut terminal_failure = metadata.clone();
        terminal_failure.id = 3;
        let terminal_error = run_session_attempt_lifecycle_with_terminal_writer(
            &registry,
            &mut store,
            terminal_failure,
            "terminal failure".into(),
            |_attempt| Err(CliError::runtime(HeadlessTurnError::Cancelled)),
            |_, _| Err(()),
        )
        .unwrap_err();
        let running = store
            .load_session_for_resume(3)
            .unwrap()
            .latest_attempt
            .unwrap();

        assert_eq!(
            terminal_error,
            AttemptLifecycleError::runtime(CliError::runtime(HeadlessTurnError::Cancelled))
        );
        assert_eq!(running.status(), agens_core::SessionAttemptStatus::Running);
        assert!(!registry.contains(&store.database_path(), running.key()));

        let mut successful = metadata.clone();
        successful.id = 4;
        let completion = run_session_attempt_lifecycle(
            &registry,
            &mut store,
            successful,
            "successful".into(),
            || {
                let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
                    TurnEvent::StateChanged(TurnState::Requesting),
                    TurnEvent::StateChanged(TurnState::Streaming),
                    TurnEvent::ProviderPart(MessagePart::Text("answer".into())),
                    TurnEvent::StateChanged(TurnState::Completed),
                ])
                .unwrap();
                let turn = completed_session_turn("successful", &snapshot, None).unwrap();

                Ok((snapshot, turn))
            },
        )
        .unwrap();

        assert_eq!(completion.metadata.completed_turn_count, 1);
        assert_eq!(completion.messages.len(), 2);
        assert_eq!(
            store
                .load_session_for_resume(4)
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Completed
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn interrupted_attempt_persists_prompt_and_note_and_reuses_the_session() {
        let directory =
            std::env::temp_dir().join(format!("agens-interrupted-partial-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut store = SessionStore::open(&directory).unwrap();
        let registry = AttemptActivityRegistry::default();
        let metadata = SessionMetadata {
            id: 1,
            project: "project".into(),
            title: "launch the explorer subagent".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
        };

        let cancelled = run_session_attempt_lifecycle(
            &registry,
            &mut store,
            metadata.clone(),
            "launch the explorer subagent".into(),
            || Err(CliError::runtime(HeadlessTurnError::Cancelled)),
        )
        .unwrap_err();

        let stored = store.load_session_for_resume(metadata.id).unwrap();

        assert_eq!(stored.metadata.completed_turn_count, 1);
        assert_eq!(
            stored.messages.first().map(|message| message.role),
            Some(Role::User)
        );
        assert_eq!(
            stored.messages.first().map(|message| message.parts.clone()),
            Some(vec![MessagePart::Text(
                "launch the explorer subagent".into()
            )])
        );
        let note = match stored.messages.get(1) {
            Some(Message {
                role: Role::Assistant,
                parts,
            }) => match parts.as_slice() {
                [MessagePart::Text(note)] => note.clone(),
                other => panic!("expected a single note part, got {other:?}"),
            },
            other => panic!("expected an assistant note, got {other:?}"),
        };
        assert!(note.contains("interrupted"), "{note:?}");
        assert_eq!(stored.messages.len(), 2);
        assert_eq!(
            stored.latest_attempt.as_ref().unwrap().status(),
            agens_core::SessionAttemptStatus::Cancelled
        );

        let AttemptLifecycleError::Runtime { error, partial } = cancelled else {
            panic!("expected a runtime failure");
        };
        let partial = partial.expect("an interrupted attempt carries its persisted turn");
        assert!(!format!("{partial:?}").contains("launch the explorer subagent"));

        let mut context = TuiSessionContext::fresh();
        assert!(
            complete_tui_turn(
                &mut context,
                Err(HeadlessChatFailure {
                    error,
                    partial: Some(partial),
                }),
                false,
            )
            .is_err()
        );
        assert_eq!(context.identifier, Some(metadata.id));

        let next = context.apply_to(interrupted_turn_test_request(
            "volve a lanzar el subagente que cancele",
        ));
        assert_eq!(next.history, stored.messages);
        assert_eq!(
            next.session.as_ref().map(|session| session.id),
            Some(metadata.id)
        );
        assert!(
            OpenAiResponsesProvider::from_api_key_with_messages_and_tools_and_timeout(
                "test-key".into(),
                None,
                "gpt-5.5".into(),
                provider_messages(&next, false),
                Vec::new(),
                std::time::Duration::from_secs(1),
            )
            .is_ok()
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    fn interrupted_turn_test_request(prompt: &str) -> HeadlessChatRequest {
        HeadlessChatRequest {
            prompt: prompt.to_owned(),
            history: Vec::new(),
            model: None,
            system_prompt: None,
            max_iterations: None,
            mode: PermissionMode::Edit,
            dangerously_allow_all: false,
            dangerous_mode: false,
            request_config: agens_core::RequestConfig::default(),
            session_reasoning_effort: None,
            session: None,
            active_agent: None,
            effective_capabilities: None,
            pending_system_reminder: None,
            skills: None,
        }
    }

    #[test]
    fn timed_out_attempt_notes_the_interruption_with_requested_subagents() {
        let directory = std::env::temp_dir().join(format!(
            "agens-interrupted-subagents-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut store = SessionStore::open(&directory).unwrap();
        let registry = AttemptActivityRegistry::default();
        let metadata = SessionMetadata {
            id: 4,
            project: "project".into(),
            title: "explore the runtime".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
        };
        let requested = Mutex::new(Vec::new());
        record_requested_subagent(
            &requested,
            &TurnEvent::ToolCallRequested {
                id: "call-1".into(),
                name: "native::task".into(),
                input: r#"{"agent":"explorer","description":"map the session writer"}"#.into(),
            },
        );
        record_requested_subagent(
            &requested,
            &TurnEvent::ToolCallRequested {
                id: "call-2".into(),
                name: "native::read".into(),
                input: r#"{"path":"notes.md"}"#.into(),
            },
        );
        assert_eq!(
            requested.lock().unwrap().as_slice(),
            [RequestedSubagent {
                agent: "explorer".into(),
                description: "map the session writer".into(),
            }]
        );

        let note = interrupted_turn_note(&requested.lock().unwrap());
        let timed_out = run_session_attempt_lifecycle_with_terminal_writer(
            &registry,
            &mut store,
            metadata.clone(),
            "explore the runtime".into(),
            |_attempt| Err(CliError::runtime(HeadlessTurnError::TimedOut)),
            |store, write| {
                assert_eq!(write.status, agens_core::SessionAttemptStatus::Cancelled);

                write_terminal_attempt(store, write, &note)
            },
        )
        .unwrap_err();

        assert!(matches!(
            timed_out,
            AttemptLifecycleError::Runtime {
                partial: Some(_),
                ..
            }
        ));
        let stored = store.load_session_for_resume(metadata.id).unwrap();
        let [_, Message { parts, .. }] = stored.messages.as_slice() else {
            panic!("expected a prompt and a note, got {:?}", stored.messages);
        };
        let [MessagePart::Text(note)] = parts.as_slice() else {
            panic!("expected a single note part, got {parts:?}");
        };

        assert!(note.contains("interrupted"), "{note:?}");
        assert!(!note.to_ascii_lowercase().contains("cancel"), "{note:?}");
        assert!(note.contains("explorer"), "{note:?}");
        assert!(note.contains("map the session writer"), "{note:?}");
        assert!(
            OpenAiResponsesProvider::from_api_key_with_messages_and_tools_and_timeout(
                "test-key".into(),
                None,
                "gpt-5.5".into(),
                stored.messages,
                Vec::new(),
                std::time::Duration::from_secs(1),
            )
            .is_ok()
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn explicit_attempt_recovery_is_exact_stale_safe_and_history_preserving() {
        let directory =
            std::env::temp_dir().join(format!("agens-explicit-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let metadata = SessionMetadata {
            id: 9,
            project: "project".into(),
            title: "title".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
        };
        let mut store = SessionStore::open(&directory).unwrap();
        let registry = AttemptActivityRegistry::default();
        let active = registry
            .begin_and_register(&mut store, &metadata, "private retry prompt".into())
            .unwrap();

        assert!(matches!(
            recover_session_attempt_lifecycle(&registry, &mut store, active.key(), 2, |_, _, _| {
                unreachable!("a locally active attempt must not invoke retry runtime")
            })
            .unwrap(),
            ExplicitAttemptRecoveryOutcome::LocallyActive
        ));
        assert_eq!(
            store
                .load_session_for_resume(metadata.id)
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Running
        );

        registry.unregister(&store.database_path(), active.key());
        let recovered = recover_session_attempt_lifecycle(
            &registry,
            &mut store,
            active.key(),
            3,
            |history, prompt, _| {
                assert!(history.is_empty());
                assert_eq!(prompt, "private retry prompt");
                let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
                    TurnEvent::StateChanged(TurnState::Requesting),
                    TurnEvent::StateChanged(TurnState::Streaming),
                    TurnEvent::ProviderPart(MessagePart::Text("recovered answer".into())),
                    TurnEvent::StateChanged(TurnState::Completed),
                ])
                .unwrap();
                let turn = completed_session_turn("private retry prompt", &snapshot, None).unwrap();

                Ok((snapshot, turn))
            },
        )
        .unwrap();

        assert!(matches!(
            recovered,
            ExplicitAttemptRecoveryOutcome::Recovered(_)
        ));
        let stored = store.load_session_for_resume(metadata.id).unwrap();
        assert_eq!(stored.metadata.completed_turn_count, 1);
        assert_eq!(stored.messages.len(), 2);
        assert_eq!(
            store.recover_running_attempt(active.key(), 4).unwrap(),
            agens_core::RecoveryOutcome::Stale
        );
        assert!(!registry.contains(&store.database_path(), active.key()));
        assert!(!format!("{recovered:?}").contains("private retry prompt"));

        let terminal_metadata = SessionMetadata { id: 10, ..metadata };
        let terminal_error = run_session_attempt_lifecycle_with_terminal_writer(
            &registry,
            &mut store,
            terminal_metadata.clone(),
            "terminal retry prompt".into(),
            |_attempt| Err(CliError::runtime(HeadlessTurnError::Cancelled)),
            |_, _| Err(()),
        )
        .unwrap_err();
        let terminal = store
            .load_session_for_resume(terminal_metadata.id)
            .unwrap()
            .latest_attempt
            .unwrap();

        assert_eq!(
            terminal_error,
            AttemptLifecycleError::runtime(CliError::runtime(HeadlessTurnError::Cancelled))
        );
        assert_eq!(terminal.status(), agens_core::SessionAttemptStatus::Running);
        assert!(!registry.contains(&store.database_path(), terminal.key()));

        drop(store);
        let mut reopened = SessionStore::open(&directory).unwrap();
        let empty_registry = AttemptActivityRegistry::default();
        assert_eq!(
            reopened
                .load_session_for_resume(terminal_metadata.id)
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Running
        );
        assert!(matches!(
            recover_session_attempt_lifecycle(
                &empty_registry,
                &mut reopened,
                terminal.key(),
                5,
                |history, prompt, _| {
                    assert!(history.is_empty());
                    assert_eq!(prompt, "terminal retry prompt");
                    let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
                        TurnEvent::StateChanged(TurnState::Requesting),
                        TurnEvent::StateChanged(TurnState::Streaming),
                        TurnEvent::ProviderPart(MessagePart::Text("terminal answer".into())),
                        TurnEvent::StateChanged(TurnState::Completed),
                    ])
                    .unwrap();
                    let turn =
                        completed_session_turn("terminal retry prompt", &snapshot, None).unwrap();

                    Ok((snapshot, turn))
                },
            )
            .unwrap(),
            ExplicitAttemptRecoveryOutcome::Recovered(_)
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reliability_integration_bounds_recovers_attempts_and_sanitizes_failures() {
        let directory = std::env::temp_dir().join(format!(
            "agens-reliability-integration-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut store = SessionStore::open(&directory).unwrap();
        let registry = AttemptActivityRegistry::default();
        let metadata = reliability_integration_metadata(7, 20);

        let failure = run_session_attempt_lifecycle(
            &registry,
            &mut store,
            metadata.clone(),
            "SENTINEL_PRIVATE_PROVIDER_RETRY".into(),
            || Err(CliError::runtime(HeadlessTurnError::ProviderServer)),
        )
        .unwrap_err();
        let failed = store.load_session_for_resume(metadata.id).unwrap();

        assert_eq!(
            failure,
            AttemptLifecycleError::runtime(CliError::runtime(HeadlessTurnError::ProviderServer))
        );
        assert!(failed.messages.is_empty());
        assert_eq!(
            failed.latest_attempt.as_ref().unwrap().status(),
            agens_core::SessionAttemptStatus::ProviderError
        );
        assert!(!format!("{failed:?}").contains("SENTINEL_PRIVATE_PROVIDER_RETRY"));

        let completed_metadata = reliability_integration_metadata(8, 21);
        let completed = run_session_attempt_lifecycle(
            &registry,
            &mut store,
            completed_metadata.clone(),
            "bounded successful prompt".into(),
            || {
                Ok(reliability_integration_completion(
                    "bounded successful prompt",
                    "answer",
                ))
            },
        )
        .unwrap();

        assert_eq!(completed.metadata.completed_turn_count, 1);
        assert_eq!(completed.messages.len(), 2);
        assert_eq!(
            store
                .load_session_for_resume(completed_metadata.id)
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            agens_core::SessionAttemptStatus::Completed
        );

        let recovery_metadata = reliability_integration_metadata(9, 22);
        let active = registry
            .begin_and_register(
                &mut store,
                &recovery_metadata,
                "SENTINEL_PRIVATE_RECOVERY_RETRY".into(),
            )
            .unwrap();
        registry.unregister(&store.database_path(), active.key());
        let recovery = recover_session_attempt_lifecycle(
            &registry,
            &mut store,
            active.key(),
            23,
            |history, prompt, _| {
                assert!(history.is_empty());
                assert_eq!(prompt, "SENTINEL_PRIVATE_RECOVERY_RETRY");
                Ok(reliability_integration_completion(
                    prompt,
                    "recovered answer",
                ))
            },
        )
        .unwrap();

        assert!(matches!(
            recovery,
            ExplicitAttemptRecoveryOutcome::Recovered(_)
        ));
        assert_eq!(
            store
                .load_session_for_resume(recovery_metadata.id)
                .unwrap()
                .metadata
                .completed_turn_count,
            1
        );

        for id in 10..76 {
            let metadata = reliability_integration_metadata(id, id);
            store
                .begin_session_attempt(&metadata, format!("SENTINEL_PRIVATE_PAGE_{id}"))
                .unwrap();
        }
        let first_page = store.list_session_page(None, "", None, 64).unwrap();
        let second_page = store
            .list_session_page(None, "", first_page.next_cursor, 64)
            .unwrap();

        assert_eq!(first_page.sessions.len(), 64);
        assert_eq!(second_page.sessions.len(), 5);
        assert_eq!(first_page.sessions.len() + second_page.sessions.len(), 69);
        assert!(
            first_page
                .sessions
                .iter()
                .all(|session| session.latest_attempt.is_some())
        );
        assert!(!format!("{first_page:?}").contains("SENTINEL_PRIVATE_PAGE_"));

        for error in [
            HeadlessTurnError::ProviderContext,
            HeadlessTurnError::ProviderNetwork,
            HeadlessTurnError::ProviderRateLimited,
            HeadlessTurnError::ProviderServer,
            HeadlessTurnError::ProviderProtocol,
            HeadlessTurnError::Cancelled,
            HeadlessTurnError::TimedOut,
        ] {
            let error = CliError::runtime(error);
            let rendered = tui_provider_outcome(Err(error));
            assert!(!format!("{rendered:?}").contains("SENTINEL_REMOTE_SECRET"));
        }

        let project_root = directory.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        let (catalog, dispatcher) = production_dangerous_child_tool_runtime(
            &project_root,
            agens_config::ToolLimitSettings::default(),
        )
        .unwrap();
        assert_eq!(
            catalog.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            [
                "read", "git_read", "list", "search", "glob", "grep", "write", "edit", "bash",
                "webfetch"
            ]
        );
        assert!(
            dispatcher
                .lock()
                .unwrap()
                .canonical_identity("native::task")
                .is_none()
        );

        let mut tui = Tui::new(ReliabilityTuiEngine);
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: "reviewer".into(),
            event: TuiExecutionEvent::ForegroundStarted { id: 7 },
        });
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(
            TuiSubagentEvent::started(
                7,
                "reviewer",
                "owner hierarchy",
                agens_tui::TuiExecutionState::ForegroundRunning,
            ),
        ));
        tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(TuiSubagentEvent::text(
            7,
            "child-only-sentinel",
        )));
        assert!(
            tui.view().conversation.unwrap().subagent_cards[0]
                .tool_calls
                .is_empty()
        );
        tui.select_transcript(agens_tui::TranscriptId::Subagent(7));
        assert!(
            tui.view()
                .conversation
                .unwrap()
                .live_markdown
                .contains("child-only-sentinel")
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    struct ReliabilityTuiEngine;

    impl TuiEngine for ReliabilityTuiEngine {
        fn cancel(&mut self) {}
    }

    fn reliability_integration_metadata(id: i64, updated_at: i64) -> SessionMetadata {
        SessionMetadata {
            id,
            project: "reliability".into(),
            title: format!("session-{id}"),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: updated_at,
            updated_at,
            completed_turn_count: 0,
            resumable: false,
        }
    }

    fn reliability_integration_completion(
        prompt: &str,
        answer: &str,
    ) -> (CompletedTurnSnapshot, CompletedSessionTurn) {
        let snapshot = CompletedTurnSnapshot::from_persisted_events(vec![
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text(answer.into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ])
        .unwrap();
        let turn = completed_session_turn(prompt, &snapshot, None).unwrap();

        (snapshot, turn)
    }
}

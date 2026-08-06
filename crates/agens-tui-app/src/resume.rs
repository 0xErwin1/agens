//! Resuming a persisted TUI session: loading it from the sessions store,
//! projecting it into a fresh [`SessionContext`], committing it into the
//! live session slot under a race guard, and reconstructing the restored
//! completed-subagent cards shown for its history.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agens_core::{HeadlessTurnError, Message, MessagePart, RetryBoundary};
use agens_store::{SessionStore, StoredSession};
use agens_tools::SkillCatalog;
use agens_tui::{
    Conversation, PaletteEntry, TuiRouteCancellation, TuiRuntimeEvent, TuiSubmissionOutcome,
};

use crate::session::resume_retry_notice;
use crate::turn::tui_session_presentation;
use agens_agents::{persist_pending_agent_correction, reconcile_persisted_active_agent};
use agens_bootstrap::Bootstrap;
use agens_error::CliError;
use agens_models::ModelSelection;
use agens_permissions::ParseToolInput;
use agens_session::context::{ResumeDraft, SessionContext};
use agens_session::provider::{CredentialResolver, ProviderKind};
use agens_session::turns::sanitize_subagent_summary;

#[cfg(any(test, feature = "test-support"))]
pub fn list_tui_sessions(bootstrap: &Bootstrap) -> Result<String, CliError> {
    let project = tui_project_identifier(bootstrap)?;
    let store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let sessions = store
        .list_sessions()
        .map_err(|_| CliError::storage("saved sessions could not be listed"))?
        .into_iter()
        .filter(|session| session.project == project)
        .collect::<Vec<_>>();

    if sessions.is_empty() {
        return Ok("No saved sessions.".to_owned());
    }

    Ok(sessions
        .iter()
        .map(|session| format!("{}\t{} event(s)", session.id, session.completed_turn_count))
        .collect::<Vec<_>>()
        .join("\n"))
}

pub struct LoadedTuiSessionResume {
    pub session: StoredSession,
    pub retry_boundary: Option<RetryBoundary>,
    pub confinement_root: std::path::PathBuf,
    /// The session's own recorded bypass-permission-prompts value. `None` means it was never
    /// recorded (pre-migration row, or a session that never completed a turn); the resume
    /// projection then falls back to configuration, exactly as [`Self::confinement_root`] falls
    /// back to the session's `project` column.
    pub bypass_permission_prompts: Option<bool>,
}

impl std::ops::Deref for LoadedTuiSessionResume {
    type Target = StoredSession;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

/// A resumed session plus the conversation history the surface renders for it.
/// The history is a handoff, not session state: it is derived from the session's
/// own messages and consumed once, so it stays out of [`SessionContext`].
#[derive(Clone, Debug)]
pub struct ResumedTuiSession {
    pub context: SessionContext,
    pub history: Vec<Conversation>,
}

pub fn resume_tui_session(
    bootstrap: &Bootstrap,
    identifier: i64,
    _skills: &SkillCatalog,
    credentials: &CredentialResolver,
) -> Result<ResumedTuiSession, CliError> {
    let session = load_tui_session_for_resume(bootstrap, identifier)?;
    prepare_loaded_tui_session_resume(bootstrap, identifier, session, credentials)
}

pub fn load_tui_session_for_resume(
    bootstrap: &Bootstrap,
    identifier: i64,
) -> Result<LoadedTuiSessionResume, CliError> {
    agens_callcount::note_session_resume_load();

    let store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let session = store
        .load_session_for_resume(identifier)
        .map_err(|_| CliError::storage("saved session is unavailable"))?;
    // A missing retry boundary is not fatal: enter the session without a draft.
    let retry_boundary = session
        .latest_attempt
        .as_ref()
        .filter(|attempt| resume_retry_notice(attempt.status()).is_some())
        .and_then(|attempt| store.load_retry_boundary(attempt.key()).ok().flatten());
    // A saved row is enough to enter. Zero completed turns (failed first turn,
    // aborted attempt, empty shell) must still open — never block the door on
    // "this session looks incomplete".
    let confinement_root = store.confinement_root(identifier).unwrap_or_else(|_| {
        bootstrap
            .project_root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    });
    let bypass_permission_prompts = store.bypass_permission_prompts(identifier).ok().flatten();
    Ok(LoadedTuiSessionResume {
        session,
        retry_boundary,
        confinement_root,
        bypass_permission_prompts,
    })
}

pub fn prepare_loaded_tui_session_resume(
    bootstrap: &Bootstrap,
    identifier: i64,
    loaded: LoadedTuiSessionResume,
    credentials: &CredentialResolver,
) -> Result<ResumedTuiSession, CliError> {
    let LoadedTuiSessionResume {
        session,
        retry_boundary,
        confinement_root,
        bypass_permission_prompts,
    } = loaded;
    agens_callcount::note_session_resume_projection();
    // History projection is best-effort: a malformed transcript must not lock
    // the user out of their own session. Enter with empty history and a notice.
    let (restored_history, history_notice) = match Conversation::from_messages_with_parser(
        &history_without_subagent_turns(&session.messages),
        |name, input| {
            let bare = name
                .strip_prefix("native::")
                .or_else(|| name.strip_prefix("mcp::"))
                .unwrap_or(name);
            agens_core::ToolInput::parse(bare, input)
        },
    ) {
        Ok(history) => (history, None),
        Err(_) => (
            Vec::new(),
            Some("session history could not be fully restored; opened without transcript".to_owned()),
        ),
    };
    let saved_provider = session.metadata.provider_id.as_deref();
    let provider = saved_provider.and_then(ProviderKind::parse);
    let selection_provider =
        provider.or_else(|| bootstrap.provider_type().and_then(ProviderKind::parse));
    let selection = match (session.metadata.model_id.as_deref(), selection_provider) {
        (Some(model), Some(provider)) => {
            let mut selector = ModelSelection::for_source(model, provider.source());
            if selector.apply_model(model).is_err() {
                let _ = selector.apply_unverified_model(model);
            }
            if let Some(effort) = session.metadata.reasoning_effort {
                let _ = selector.apply_reasoning_effort(effort.as_str());
            }
            Some(selector)
        }
        _ => None,
    };
    let resume_error = saved_provider
        .filter(|_| {
            provider.is_none_or(|provider| {
                !credentials
                    .status(&bootstrap.paths.credentials, provider)
                    .available()
            })
        })
        .map(|_| "connect or choose provider".to_owned());
    let session_root =
        agens_bootstrap::session_root::SessionRoot::confined_to(confinement_root.clone());
    // Config resolve is preferred for bypass defaults, but must not block entry.
    let configured_bypass = agens_bootstrap::session_config::SessionConfig::resolve(
        &session_root,
        bootstrap,
    )
    .ok()
    .map(|config| config.bypass_permission_prompts());
    let mut context = SessionContext::restored(
        identifier,
        session.metadata,
        session.messages,
        confinement_root,
    );
    context.provider = provider;
    context.selection = selection;
    context.resume_error = resume_error;
    // A resumed session's own recorded value wins over configuration; configuration only seeds a
    // session that never recorded one (pre-migration row, or one that never completed a turn).
    context.bypass_permissions = bypass_permission_prompts
        .or(configured_bypass)
        .unwrap_or(false);
    if let Some(notice) = history_notice {
        context.resume_notice = Some(notice);
    }
    if let Some(boundary) = retry_boundary {
        if let Some(status) = session
            .latest_attempt
            .as_ref()
            .map(agens_core::SessionAttemptSummary::status)
        {
            if context.resume_notice.is_none() {
                context.resume_notice = resume_retry_notice(status).map(str::to_owned);
            }
        }
        // A runtime-scheduled turn is not the user's to retry, so its prompt is
        // never handed back to the composer.
        if !agens_tui::is_runtime_scheduled_prompt(boundary.prompt()) {
            context.resume_draft = Some(ResumeDraft::new(boundary.prompt().to_owned()));
            // Restore durable media ids only (no source path) so retry re-encodes from the store.
            // A missing blob must not block entry.
            for media_id in boundary.media_ids() {
                if let Ok((mime, _path)) =
                    agens_store::open_media(bootstrap.data_directory(), *media_id)
                {
                    context.push_pending_media(*media_id, mime);
                }
            }
        }
    }
    reconcile_persisted_active_agent(bootstrap, &mut context)?;
    Ok(ResumedTuiSession {
        context,
        history: restored_history,
    })
}

/// Commits a prepared resume into the live session slot under the race guard described on
/// [`TuiRouteCancellation`], then invokes `on_commit` exactly once, only for a resume that has
/// actually won that race — never for one rejected as busy, stale, or cancelled.
///
/// `on_commit` is the caller's hook for refreshing every OTHER piece of session-scoped state that
/// must follow the session's newly recorded root (command/skill catalogs, the `@` picker
/// candidate list, the rendered composer palette): this function only owns the session slot
/// itself, so it cannot refresh those on the caller's behalf. It returns the picker candidates and
/// palette entries to attach to the outcome.
pub fn commit_tui_session_resume(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<SessionContext>>,
    expected: &SessionContext,
    resumed: ResumedTuiSession,
    cancellation: &TuiRouteCancellation,
    on_commit: impl FnOnce(&SessionContext) -> (Vec<String>, Vec<PaletteEntry>),
) -> Result<TuiSubmissionOutcome, CliError> {
    let ResumedTuiSession {
        context: mut resumed,
        history,
    } = resumed;
    let presentation = tui_session_presentation(bootstrap, &resumed);
    let message = resumed.note();
    let draft = resumed.resume_draft.take().map(ResumeDraft::into_inner);
    let media_chips = resumed.pending_media_chip_labels();
    let resume_error = resumed.resume_error.clone();
    resumed.resume_notice = None;
    if cancellation.is_cancelled() {
        return Ok(TuiSubmissionOutcome::RouteCancelled);
    }

    let mut current = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if current.running {
        return Err(CliError::runtime(HeadlessTurnError::State));
    }
    if *current != *expected || !cancellation.try_commit() {
        return Ok(TuiSubmissionOutcome::RouteCancelled);
    }
    persist_pending_agent_correction(bootstrap, &mut resumed);
    let (file_candidates, palette_entries) = on_commit(&resumed);
    *current = resumed;

    Ok(TuiSubmissionOutcome::SessionResumed {
        message,
        presentation,
        history,
        draft,
        media_chips,
        resume_error,
        file_candidates,
        palette_entries,
    })
}

const MAX_RESTORED_SUBAGENT_TOOL_USES: usize = 256;

/// The session's own messages, with the turns that belong to a subagent removed.
///
/// A completed subagent is persisted as an ordinary user/assistant/tool turn,
/// and [`resumed_subagent_cards`] already restores it as a card. Left in the
/// history as well, its task text opens a turn of its own and reads as though
/// the user had typed the instructions the runtime wrote for the subagent.
fn history_without_subagent_turns(messages: &[Message]) -> Vec<Message> {
    let mut kept = Vec::with_capacity(messages.len());
    let mut index = 0;

    while index < messages.len() {
        if messages
            .get(index..index + 3)
            .is_some_and(is_persisted_subagent_turn)
        {
            index += 3;
            continue;
        }
        kept.push(messages[index].clone());
        index += 1;
    }

    kept
}

/// Whether `window` is the three-message shape a completed subagent is stored as.
fn is_persisted_subagent_turn(window: &[Message]) -> bool {
    let [user, assistant, tool] = window else {
        return false;
    };
    if !matches!(user.parts.as_slice(), [MessagePart::Text(_)]) {
        return false;
    }
    let [MessagePart::ToolCall { id, .. }, MessagePart::Reasoning(_)] = assistant.parts.as_slice()
    else {
        return false;
    };
    let [MessagePart::ToolResult { tool_call_id, .. }] = tool.parts.as_slice() else {
        return false;
    };

    id.starts_with("subagent:") && tool_call_id == id
}

pub fn resumed_subagent_cards(messages: &[Message]) -> Vec<TuiRuntimeEvent> {
    let mut restored = Vec::new();
    let mut seen = BTreeSet::new();

    for window in messages.windows(3) {
        let [user, assistant, tool] = window else {
            continue;
        };
        let [MessagePart::Text(task)] = user.parts.as_slice() else {
            continue;
        };
        let [
            MessagePart::ToolCall { id, name, input },
            MessagePart::Reasoning(reasoning),
        ] = assistant.parts.as_slice()
        else {
            continue;
        };
        let [
            MessagePart::ToolResult {
                tool_call_id,
                content: final_result,
                is_error: false,
            },
        ] = tool.parts.as_slice()
        else {
            continue;
        };
        let Some(id) = id
            .strip_prefix("subagent:")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|id| *id > 0)
        else {
            continue;
        };
        // Sessions recorded before the wire name was corrected still carry the
        // dispatcher's `native::task`, and they must keep restoring their cards.
        let Some((agent, description)) = matches!(name.as_str(), "task" | "native::task")
            .then(|| serde_json::from_str::<serde_json::Value>(input).ok())
            .flatten()
            .and_then(|value| {
                Some((
                    value.get("agent")?.as_str()?.to_owned(),
                    value.get("description")?.as_str()?.to_owned(),
                ))
            })
        else {
            continue;
        };
        let Some(tool_uses) = reasoning
            .strip_suffix(" tool uses")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|tool_uses| *tool_uses <= MAX_RESTORED_SUBAGENT_TOOL_USES)
        else {
            continue;
        };
        if task.is_empty()
            || agent.is_empty()
            || description != *task
            || *tool_call_id != format!("subagent:{id}")
            || !seen.insert(id)
        {
            continue;
        }

        restored.push(TuiRuntimeEvent::RestoredCompletedSubagent {
            id,
            agent: sanitize_subagent_summary(&agent),
            task_summary: sanitize_subagent_summary(task),
            final_result: sanitize_subagent_summary(final_result),
            tool_uses,
        });
    }

    restored
}

/// Identifies the process's own current project, used to filter the session picker to sessions
/// belonging to it. This is a listing/grouping concern distinct from a session's own confinement
/// root, so it is one of the few sites allowed to read the process-wide discovered root.
pub fn tui_project_identifier(bootstrap: &Bootstrap) -> Result<String, CliError> {
    agens_bootstrap::session_root::SessionRoot::discover_for_new_session(bootstrap)
        .map(|root| root.path().display().to_string())
        .ok_or_else(|| CliError::configuration("TUI sessions require a project root"))
}

#[cfg(test)]
mod tests {
    use agens_core::{
        HeadlessTurnCancellation, MessagePart, PermissionDecision, PermissionMode,
        PermissionPattern, PermissionPolicy, PermissionRule, PermissionSession, Role,
        SessionAttemptStatus, SessionMetadata, TurnEvent,
    };
    use agens_tools::{ToolDispatchRequest, ToolEvaluationOutcome, ToolExecutionContext};
    use agens_tui::{Event, Key, Tui, TuiPermissionBridge};
    use rusqlite::Connection;

    use super::*;
    use crate::engine::ProductionTuiEngine;
    use crate::models::apply_tui_model;
    use crate::permission_prompt::{TuiPermissionPrompter, production_tui_permission_bridge};
    use crate::test_support::{
        bootstrap_from_a_different_working_directory, bootstrap_from_configuration,
        persist_tui_session, persist_tui_session_metadata, render_tui_test_backend,
        rotation_dispatcher, tui_project, tui_session_bootstrap,
        tui_session_bootstrap_for_provider, tui_session_bootstrap_with_global_bypass,
        tui_session_directory, tui_session_messages,
    };
    use agens_agents::ensure_active_agent_runtime;
    use agens_callcount::{Counts, counts as call_counts, reset as reset_call_counts};
    use agens_core::ask_user::UnavailableAskUserPort;
    use agens_headless::{HeadlessChatRequest, apply_session_to_request};
    use agens_session::attempt::attempt_failure_status;
    use agens_session::turns::{completed_session_turn_from_events, next_session_metadata};
    use agens_tool_runtime::runner::{TuiTaskControls, TuiTaskLifecycleBridge};
    use agens_tool_runtime::task::production_tui_task_runtime;

    fn bare_headless_request() -> HeadlessChatRequest {
        HeadlessChatRequest {
            prompt: "test".into(),
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
            media_ids: Vec::new(),
            media_mimes: Vec::new(),
        }
    }

    #[test]
    fn resuming_from_a_different_working_directory_confines_to_the_originally_recorded_root() {
        let origin = tui_session_directory("confinement-origin");
        let creation_bootstrap = tui_session_bootstrap(&origin, &[]);
        let mut store = SessionStore::open(creation_bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&origin), "origin");
        drop(store);

        let resume_bootstrap =
            bootstrap_from_a_different_working_directory(&origin, "confinement-elsewhere");
        assert_ne!(
            resume_bootstrap.paths().project_config,
            creation_bootstrap.paths().project_config
        );

        let resumed = resume_tui_session(
            &resume_bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        );
        assert!(
            resumed.is_ok(),
            "resuming from a different working directory must confine to the session's own \
             recorded root instead of being rejected: {resumed:?}"
        );
        let session = Arc::new(Mutex::new(resumed.unwrap().context));
        let resolved_root = agens_session::root::resolve_tui_session_root(
            &session.lock().unwrap(),
            &resume_bootstrap,
        )
        .unwrap();
        assert_eq!(resolved_root, origin.join("project"));
        let runtime = production_tui_task_runtime(
            &resume_bootstrap,
            &resolved_root,
            &SkillCatalog::default(),
            Box::new(TuiPermissionPrompter(
                production_tui_permission_bridge().0,
                None,
            )),
            TuiTaskLifecycleBridge::new(
                agens_tui::BridgeTx::bounded(8).0,
                TuiTaskControls::default(),
            ),
            agens_core::RequestConfig::default(),
            "confinement-check".to_owned(),
            false,
            Box::new(UnavailableAskUserPort),
        )
        .unwrap();
        ensure_active_agent_runtime(&resume_bootstrap, &session, &runtime.dispatcher).unwrap();

        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::read".into()),
                PermissionPattern::Any,
            )],
        );
        let outcome = runtime
            .dispatcher
            .lock()
            .unwrap()
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    tui_project(&origin),
                    "read",
                    serde_json::json!({"path": "marker.txt"}),
                ),
            )
            .unwrap();
        let ToolEvaluationOutcome::Authorized(handle) = outcome else {
            panic!("read should authorize under an allow rule");
        };
        std::fs::write(origin.join("project").join("marker.txt"), "origin-only").unwrap();
        let context = ToolExecutionContext::from_headless_adapter(
            HeadlessTurnCancellation::new().adapter_view(),
        );
        let output = runtime
            .dispatcher
            .lock()
            .unwrap()
            .execute(handle, &context)
            .unwrap();
        assert!(
            !output.is_error,
            "the confined read must find the file under the ORIGINAL root: {output:?}"
        );
        assert!(output.content.contains("origin-only"));

        std::fs::remove_dir_all(&origin).unwrap();
    }

    #[test]
    fn tui_session_list_filters_current_project_and_resume_preserves_typed_history() {
        let temporary = tui_session_directory("filter-resume");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let current = persist_tui_session(&mut store, &tui_project(&temporary), "current");
        persist_tui_session(
            &mut store,
            &temporary.join("other").display().to_string(),
            "other",
        );

        assert_eq!(list_tui_sessions(&bootstrap).unwrap(), "1\t1 event(s)");

        reset_call_counts();
        let resumed = resume_tui_session(
            &bootstrap,
            current.id,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(resumed.context.identifier, Some(current.id));
        assert_eq!(resumed.context.metadata, Some(current));
        assert_eq!(resumed.context.messages, tui_session_messages());
        assert!(resumed.context.active_agent.is_none());
        assert_eq!(resumed.history.len(), 1);
        assert_eq!(call_counts(), Counts(1, 1, 0, 0));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn failed_tui_resume_restores_exact_partial_provider_and_tool_history() {
        let temporary = tui_session_directory("failed-partial-resume");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = SessionMetadata {
            id: 42,
            project: tui_project(&temporary),
            title: "partial".into(),
            active_agent: "primary".into(),
            provider_id: Some("openai-api".into()),
            model_id: Some("test-model".into()),
            reasoning_effort: None,
            created_at: 1,
            updated_at: 1,
            completed_turn_count: 0,
            resumable: false,
        };
        let active = store
            .begin_session_attempt(&metadata, "inspect".into())
            .unwrap();
        let events = vec![
            TurnEvent::ProviderPart(MessagePart::Reasoning("checking".into())),
            TurnEvent::ProviderPart(MessagePart::ToolCall {
                id: "call-read".into(),
                name: "native::read".into(),
                input: r#"{"path":"Cargo.toml"}"#.into(),
            }),
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-read".into(),
                content: "[workspace]".into(),
                is_error: false,
            }),
            TurnEvent::ProviderPart(MessagePart::Text("partial answer".into())),
        ];
        let turn = completed_session_turn_from_events("inspect", &events, None).unwrap();
        let expected = turn.messages().to_vec();
        store
            .persist_partial_session_attempt(
                active.key(),
                &metadata,
                &turn,
                SessionAttemptStatus::ProviderError,
                2,
            )
            .unwrap();
        drop(store);

        let resumed = resume_tui_session(
            &bootstrap,
            active.key().session_id(),
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )
        .unwrap();

        assert_eq!(resumed.context.messages, expected);
        assert_eq!(resumed.history.len(), 1);
        assert_eq!(resumed.context.metadata.unwrap().completed_turn_count, 1);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    fn session_attempt_count(store: &SessionStore) -> i64 {
        Connection::open(store.database_path())
            .unwrap()
            .query_row("SELECT COUNT(*) FROM session_attempts", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn assert_restored_retry_draft_ui(outcome: TuiSubmissionOutcome, retry_prompt: &str) {
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        assert!(tui.begin_session_load());
        assert!(tui.apply_submission_outcome(outcome).is_none());
        let view = tui.view();
        assert_eq!(view.input, retry_prompt);
        assert_eq!(view.focus, agens_tui::TranscriptFocus::Composer);
        assert!(view.following_bottom);
        assert_eq!(
            view.status,
            Some("Recovered failed prompt · Enter retry · Esc discard")
        );
        assert!(view.completed_conversations.is_empty());
        assert!(!view.running);
        let rendered = render_tui_test_backend(&tui, 120, 24);
        assert!(rendered.contains(retry_prompt), "{rendered:?}");
        assert!(
            rendered.contains("Recovered failed prompt · Enter retry · Esc discard"),
            "{rendered:?}"
        );
    }

    #[test]
    fn zero_turn_failed_tui_resume_restores_draft_without_runtime_or_attempt_creation() {
        let temporary = tui_session_directory("failed-draft-resume");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = SessionMetadata {
            id: 0,
            project: tui_project(&temporary),
            title: "failed".into(),
            active_agent: "primary".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: 10,
            updated_at: 20,
            completed_turn_count: 0,
            resumable: false,
        };
        let retry_prompt = "retry exact café 🙂";
        let media = agens_store::ingest_media_bytes(
            bootstrap.data_directory(),
            b"retry-media",
            "image/png",
        )
        .unwrap();
        let attempt = store
            .begin_session_attempt_with_media(&metadata, retry_prompt.into(), vec![media.id])
            .unwrap();
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::Failed, 30)
            .unwrap();
        let attempt_count = session_attempt_count(&store);
        drop(store);

        reset_call_counts();
        let loaded = load_tui_session_for_resume(&bootstrap, attempt.key().session_id()).unwrap();
        assert_eq!(
            loaded.retry_boundary.as_ref().map(RetryBoundary::prompt),
            Some(retry_prompt)
        );
        assert_eq!(
            loaded
                .retry_boundary
                .as_ref()
                .map(RetryBoundary::media_ids)
                .unwrap(),
            &[media.id]
        );
        let prepared = prepare_loaded_tui_session_resume(
            &bootstrap,
            attempt.key().session_id(),
            loaded,
            &CredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(prepared.context.resume_draft.as_deref(), Some(retry_prompt));
        assert_eq!(prepared.context.pending_media_ids, vec![media.id]);
        assert_eq!(
            prepared.context.pending_media_mimes,
            vec!["image/png".to_owned()]
        );
        assert_eq!(
            prepared.context.pending_media_chip_labels(),
            vec!["[Image #1]".to_owned()]
        );
        assert!(!format!("{prepared:?}").contains(retry_prompt));
        assert_eq!(
            prepared.context.note(),
            "Recovered failed prompt · Enter retry · Esc discard"
        );
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let expected = session.lock().unwrap().clone();
        let outcome = commit_tui_session_resume(
            &bootstrap,
            &session,
            &expected,
            prepared,
            &TuiRouteCancellation::new(),
            |_| (Vec::new(), Vec::new()),
        )
        .unwrap();
        assert!(session.lock().unwrap().resume_draft.is_none());
        assert_eq!(session.lock().unwrap().pending_media_ids, vec![media.id]);
        assert_restored_retry_draft_ui(outcome.clone(), retry_prompt);
        let TuiSubmissionOutcome::SessionResumed {
            message,
            history,
            draft,
            media_chips,
            ..
        } = outcome
        else {
            panic!("expected resumed outcome");
        };
        assert_eq!(
            message,
            "Recovered failed prompt · Enter retry · Esc discard"
        );
        assert!(history.is_empty());
        assert_eq!(draft.as_deref(), Some(retry_prompt));
        assert_eq!(media_chips, vec!["[Image #1]".to_owned()]);
        assert_eq!(call_counts(), Counts(1, 1, 0, 0));

        let reopened = SessionStore::open(bootstrap.data_directory()).unwrap();
        let unchanged_attempt_count = session_attempt_count(&reopened);
        assert_eq!(unchanged_attempt_count, attempt_count);
        assert_eq!(
            reopened
                .load_session_for_resume(attempt.key().session_id())
                .unwrap()
                .latest_attempt
                .unwrap()
                .status(),
            SessionAttemptStatus::Failed
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn completed_history_resume_adds_failed_draft_without_duplicate_user_message() {
        let temporary = tui_session_directory("history-failed-draft");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "history");
        let retry_prompt = "failed next prompt";
        let attempt = store
            .begin_session_attempt(&metadata, retry_prompt.into())
            .unwrap();
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::ProviderError, 40)
            .unwrap();
        drop(store);

        let loaded = load_tui_session_for_resume(&bootstrap, metadata.id).unwrap();
        let prepared = prepare_loaded_tui_session_resume(
            &bootstrap,
            metadata.id,
            loaded,
            &CredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(prepared.context.messages, tui_session_messages());
        assert_eq!(prepared.history.len(), 1);
        assert_eq!(prepared.context.resume_draft.as_deref(), Some(retry_prompt));
        assert_eq!(
            prepared.context.note(),
            "Recovered failed prompt · Enter retry · Esc discard"
        );
        assert!(
            prepared
                .context
                .messages
                .iter()
                .all(|message| message.role != Role::User
                    || message.parts != [MessagePart::Text(retry_prompt.into())])
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn resumed_session_keeps_its_own_recorded_bypass_value_off_against_a_true_config() {
        let temporary = tui_session_directory("resume-bypass-off-over-config-on");
        let bootstrap = tui_session_bootstrap_with_global_bypass(&temporary, true);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "bypass-off");
        store
            .set_bypass_permission_prompts(metadata.id, false)
            .unwrap();
        drop(store);

        let prepared = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )
        .unwrap();

        assert!(!prepared.context.bypass_permissions);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn resumed_session_keeps_its_own_recorded_bypass_value_on_against_a_false_config() {
        let temporary = tui_session_directory("resume-bypass-on-over-config-off");
        let bootstrap = tui_session_bootstrap_with_global_bypass(&temporary, false);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "bypass-on");
        store
            .set_bypass_permission_prompts(metadata.id, true)
            .unwrap();
        drop(store);

        let prepared = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )
        .unwrap();

        assert!(prepared.context.bypass_permissions);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn resumed_session_with_no_recorded_bypass_value_falls_back_to_configuration() {
        let temporary = tui_session_directory("resume-bypass-none-falls-back");
        let bootstrap = tui_session_bootstrap_with_global_bypass(&temporary, true);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "bypass-none");
        drop(store);

        let prepared = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )
        .unwrap();

        assert!(prepared.context.bypass_permissions);

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn completed_resume_without_retry_draft_and_cancelled_timeout_taxonomy_stay_explicit() {
        let temporary = tui_session_directory("completed-no-draft");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "completed");
        drop(store);

        let loaded = load_tui_session_for_resume(&bootstrap, metadata.id).unwrap();
        assert!(loaded.retry_boundary.is_none());
        let prepared = prepare_loaded_tui_session_resume(
            &bootstrap,
            metadata.id,
            loaded,
            &CredentialResolver::production(),
        )
        .unwrap();
        assert!(prepared.context.resume_draft.is_none());
        assert!(prepared.context.note().starts_with("Resumed session"));
        assert_eq!(
            resume_retry_notice(SessionAttemptStatus::Cancelled),
            Some("Recovered failed prompt · Enter retry · Esc discard")
        );
        assert_eq!(
            attempt_failure_status(&CliError::runtime(HeadlessTurnError::TimedOut)),
            SessionAttemptStatus::Cancelled
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn tui_resume_commit_discards_cancelled_stale_and_invalid_preparations() {
        let temporary = tui_session_directory("atomic-resume");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "atomic");
        let attempt = store
            .begin_session_attempt(&metadata, "atomic preserved draft".into())
            .unwrap();
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::Failed, 30)
            .unwrap();
        drop(store);
        let credentials = CredentialResolver::production();
        let prepared = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &credentials,
        )
        .unwrap();
        assert_eq!(
            prepared.context.resume_draft.as_deref(),
            Some("atomic preserved draft")
        );
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let original = session.lock().unwrap().clone();

        let cancelled = TuiRouteCancellation::new();
        cancelled.cancel();
        assert_eq!(
            commit_tui_session_resume(
                &bootstrap,
                &session,
                &original,
                prepared.clone(),
                &cancelled,
                |_| (Vec::new(), Vec::new()),
            )
            .unwrap(),
            TuiSubmissionOutcome::RouteCancelled
        );
        assert_eq!(*session.lock().unwrap(), original);

        session.lock().unwrap().dangerous_mode = true;
        let newer = session.lock().unwrap().clone();
        assert_eq!(
            commit_tui_session_resume(
                &bootstrap,
                &session,
                &original,
                prepared.clone(),
                &TuiRouteCancellation::new(),
                |_| (Vec::new(), Vec::new()),
            )
            .unwrap(),
            TuiSubmissionOutcome::RouteCancelled
        );
        assert_eq!(*session.lock().unwrap(), newer);

        *session.lock().unwrap() = original.clone();
        let accepted = TuiRouteCancellation::new();
        assert!(matches!(
            commit_tui_session_resume(&bootstrap, &session, &original, prepared, &accepted, |_| {
                (Vec::new(), Vec::new())
            },)
            .unwrap(),
            TuiSubmissionOutcome::SessionResumed { .. }
        ));
        assert!(!accepted.cancel());
        let committed = session.lock().unwrap();
        assert_eq!(committed.identifier, Some(metadata.id));
        assert_eq!(committed.messages, tui_session_messages());
        drop(committed);

        let mut invalid = load_tui_session_for_resume(&bootstrap, metadata.id).unwrap();
        invalid.session.messages = vec![Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("orphan".into())],
        }];
        // Malformed history must still open the session (empty transcript + notice).
        let degraded = prepare_loaded_tui_session_resume(
            &bootstrap,
            metadata.id,
            invalid,
            &credentials,
        )
        .expect("malformed history must not block session entry");
        assert!(
            degraded.history.is_empty(),
            "unprojectable history opens with an empty transcript"
        );
        assert!(
            degraded
                .context
                .resume_notice
                .as_deref()
                .is_some_and(|notice| notice.contains("history")),
            "resume should surface a history-restoration notice: {:?}",
            degraded.context.resume_notice
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn first_runtime_materialization_after_resume_preserves_permission_denial() {
        let temporary = tui_session_directory("lazy-resume-runtime");
        let bootstrap = tui_session_bootstrap(&temporary, &[]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session(&mut store, &tui_project(&temporary), "lazy");
        drop(store);
        let skills = SkillCatalog::default();
        reset_call_counts();
        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &skills,
            &CredentialResolver::production(),
        )
        .unwrap()
        .context;
        assert_eq!(call_counts(), Counts(1, 1, 0, 0));
        let session = Arc::new(Mutex::new(resumed));
        let (permission_bridge, _) = TuiPermissionBridge::channel();
        let (events, _) = agens_tui::BridgeTx::bounded(8);
        let project_root =
            agens_session::root::resolve_tui_session_root(&session.lock().unwrap(), &bootstrap)
                .unwrap();
        let runtime = production_tui_task_runtime(
            &bootstrap,
            &project_root,
            &skills,
            Box::new(TuiPermissionPrompter(permission_bridge, None)),
            TuiTaskLifecycleBridge::new(events, TuiTaskControls::default()),
            agens_core::RequestConfig::default(),
            "abc12345".to_owned(),
            false,
            Box::new(UnavailableAskUserPort),
        )
        .unwrap();
        ensure_active_agent_runtime(&bootstrap, &session, &runtime.dispatcher).unwrap();
        assert_eq!(call_counts(), Counts(1, 1, 1, 0));
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .map(|agent| agent.name.as_str()),
            Some("primary")
        );

        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::Exact("native::task".into()),
                PermissionPattern::Any,
            )],
        );
        let outcome = runtime
            .dispatcher
            .lock()
            .unwrap()
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    tui_project(&temporary),
                    "task",
                    serde_json::json!({"agent":"explore","description":"inspect"}),
                ),
            )
            .unwrap();
        assert!(matches!(outcome, ToolEvaluationOutcome::Denied));
        assert_eq!(call_counts(), Counts(1, 1, 1, 0));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn model_switch_invalidates_and_rematerializes_inherited_primary_without_stale_model() {
        let temporary = tui_session_directory("active-agent-model-switch");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let dispatcher = Arc::new(Mutex::new(rotation_dispatcher()));
        ensure_active_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();
        assert_eq!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .and_then(|agent| agent.model.as_deref()),
            Some("gpt-5.5")
        );

        apply_tui_model(&bootstrap, "gpt-5.6-sol", &session).unwrap();
        assert!(session.lock().unwrap().active_agent.is_none());
        ensure_active_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();

        let context = session.lock().unwrap();
        assert_eq!(
            context
                .active_agent
                .as_ref()
                .and_then(|agent| agent.model.as_deref()),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            context
                .selection
                .as_ref()
                .unwrap()
                .reasoning_effort_default(),
            Some("medium")
        );
        drop(context);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn stale_persisted_agent_falls_back_to_primary_warns_and_persists_correction() {
        let temporary = tui_session_directory("stale-active-agent-fallback");
        let stale_definition = "---\nname: retired\ndescription: retired\nmode: primary\npermissions:\n  - allow native::read\n---\nRetired work.\n";
        let bootstrap = tui_session_bootstrap(&temporary, &[("retired", stale_definition)]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session_metadata(
            &mut store,
            &tui_project(&temporary),
            "stale",
            "retired",
            100,
        );
        drop(store);
        std::fs::remove_file(
            bootstrap
                .paths
                .global_config
                .with_file_name("agents")
                .join("retired.md"),
        )
        .unwrap();

        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )
        .unwrap();

        assert_eq!(
            resumed.context.note(),
            "Agent 'retired' is unavailable; resumed with primary."
        );
        assert_eq!(
            resumed.context.metadata.as_ref().unwrap().active_agent,
            "primary"
        );
        assert!(resumed.context.active_agent.is_none());
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "retired"
        );
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let expected = session.lock().unwrap().clone();
        let outcome = commit_tui_session_resume(
            &bootstrap,
            &session,
            &expected,
            resumed,
            &TuiRouteCancellation::new(),
            |_| (Vec::new(), Vec::new()),
        )
        .unwrap();
        assert!(matches!(
            outcome,
            TuiSubmissionOutcome::SessionResumed { message, .. }
                if message == "Agent 'retired' is unavailable; resumed with primary."
        ));
        ensure_active_agent_runtime(
            &bootstrap,
            &session,
            &Arc::new(Mutex::new(rotation_dispatcher())),
        )
        .unwrap();
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "primary"
        );
        assert!(!session.lock().unwrap().agent_correction_pending);
        assert!(
            session
                .lock()
                .unwrap()
                .active_agent
                .as_ref()
                .unwrap()
                .capabilities
                .descriptors()
                .is_empty()
        );
        let diagnostics = std::fs::read_to_string(
            bootstrap
                .data_directory()
                .join("diagnostics")
                .join(format!("agens-{}.jsonl", std::process::id())),
        )
        .unwrap();
        assert!(diagnostics.contains(r#""event":"agent_fallback""#));
        assert!(!diagnostics.contains("Retired work"));
        assert!(!diagnostics.contains(&tui_project(&temporary)));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn resumed_primary_uses_the_configured_profile_instead_of_the_saved_model() {
        let label = "resume-configured-primary-model";
        let bootstrap = bootstrap_from_configuration(
            label,
            Some(
                "[provider]\ntype = \"openai-chatgpt\"\n\
                 [agent]\ndefault_agent = \"primary\"\n\
                 [agents.primary]\nmodel = \"gpt-5.6-sol\"\neffort = \"high\"\n",
            ),
            None,
        );
        let temporary = std::env::temp_dir().join(format!("agens-{label}-{}", std::process::id()));
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let mut metadata = persist_tui_session_metadata(
            &mut store,
            &tui_project(&temporary),
            "configured primary",
            "primary",
            100,
        );
        metadata.provider_id = Some("openai-chatgpt".into());
        metadata.model_id = Some("gpt-5.5".into());
        metadata.reasoning_effort = Some(agens_core::ReasoningEffort::Low);
        store.update_session_selection(&metadata).unwrap();
        drop(store);

        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &SkillCatalog::default(),
            &CredentialResolver::production(),
        )
        .unwrap()
        .context;

        let selection = resumed
            .selection
            .as_ref()
            .expect("profile should select a model");
        assert_eq!(selection.model(), "gpt-5.6-sol");
        assert_eq!(selection.reasoning_effort(), Some("high"));
        assert_eq!(
            tui_session_presentation(&bootstrap, &resumed).model(),
            "gpt-5.6-sol"
        );

        let request = apply_session_to_request(&resumed, bare_headless_request());
        let model = request.model.clone().expect("request model should be set");
        let effort = request
            .session_reasoning_effort
            .or_else(|| request.request_config.reasoning_effort());
        assert_eq!(model, "gpt-5.6-sol");
        assert_eq!(effort, Some(agens_core::ReasoningEffort::High));
        let next = next_session_metadata(
            &bootstrap,
            "continued",
            request.session.as_ref(),
            request.active_agent.as_deref(),
            Some("openai-chatgpt".into()),
            model,
            effort,
        )
        .unwrap();
        let turn = completed_session_turn_from_events(
            "continued",
            &[TurnEvent::ProviderPart(MessagePart::Text("done".into()))],
            None,
        )
        .unwrap();
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let attempt = store
            .begin_session_attempt(&next, "continued".into())
            .unwrap();
        store
            .persist_completed_session_attempt(
                attempt.key(),
                &next,
                &turn,
                agens_session::context::current_session_timestamp(),
            )
            .unwrap();
        let persisted = store.load_session_for_resume(metadata.id).unwrap().metadata;
        assert_eq!(persisted.model_id.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            persisted.reasoning_effort,
            Some(agens_core::ReasoningEffort::High)
        );

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn explicit_unavailable_agent_model_and_ineligible_primary_are_hard_errors() {
        for (case, definition, active_agent, expected) in [
            (
                "explicit-model",
                "---\nname: reviewer\ndescription: reviewer\nmode: primary\nmodel: gpt-4o\npermissions: []\n---\nReview.\n",
                "reviewer",
                "agent model is unavailable",
            ),
            (
                "ineligible-primary",
                "---\nname: primary\ndescription: primary\nmode: subagent\npermissions: []\n---\nWrong mode.\n",
                "primary",
                "primary agent is unavailable",
            ),
        ] {
            let temporary = tui_session_directory(case);
            let bootstrap = tui_session_bootstrap_for_provider(
                &temporary,
                &[(active_agent, definition)],
                "openai-chatgpt",
                "gpt-5.5",
            );
            let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
            let metadata = persist_tui_session_metadata(
                &mut store,
                &tui_project(&temporary),
                case,
                active_agent,
                100,
            );
            drop(store);

            let error = resume_tui_session(
                &bootstrap,
                metadata.id,
                &SkillCatalog::default(),
                &CredentialResolver::production(),
            )
            .unwrap_err();
            assert_eq!(error.message, expected, "{case}");
            assert_eq!(
                SessionStore::open(bootstrap.data_directory())
                    .unwrap()
                    .load_session_for_resume(metadata.id)
                    .unwrap()
                    .metadata
                    .active_agent,
                active_agent,
                "{case}"
            );
            let diagnostics = std::fs::read_to_string(
                bootstrap
                    .data_directory()
                    .join("diagnostics")
                    .join(format!("agens-{}.jsonl", std::process::id())),
            )
            .unwrap();
            assert!(diagnostics.contains(r#""event":"agent_unavailable""#));
            assert!(!diagnostics.contains(definition));

            std::fs::remove_dir_all(temporary).unwrap();
        }
    }

    #[test]
    fn barrier_resume_loader_is_local_and_discards_its_late_cancelled_result() {
        let temporary = tui_session_directory("barrier-resume");
        let stale_definition = "---\nname: retired\ndescription: retired\nmode: primary\npermissions: []\n---\nRetired.\n";
        let bootstrap = tui_session_bootstrap(&temporary, &[("retired", stale_definition)]);
        let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
        let metadata = persist_tui_session_metadata(
            &mut store,
            &tui_project(&temporary),
            "barrier",
            "retired",
            100,
        );
        drop(store);
        std::fs::remove_file(
            bootstrap
                .paths
                .global_config
                .with_file_name("agents")
                .join("retired.md"),
        )
        .unwrap();
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let original = session.lock().unwrap().clone();
        let cancellation = TuiRouteCancellation::new();
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let bootstrap = bootstrap.clone();
            let session = Arc::clone(&session);
            let original = original.clone();
            let cancellation = cancellation.clone();
            move || {
                reset_call_counts();
                started_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
                let prepared = resume_tui_session(
                    &bootstrap,
                    metadata.id,
                    &SkillCatalog::default(),
                    &CredentialResolver::production(),
                )
                .unwrap();
                let outcome = commit_tui_session_resume(
                    &bootstrap,
                    &session,
                    &original,
                    prepared,
                    &cancellation,
                    |_| (Vec::new(), Vec::new()),
                )
                .unwrap();
                (outcome, call_counts())
            }
        });
        started_receiver.recv().unwrap();

        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });
        tui.set_presentation("old-provider", "old-model", "session #1");
        tui.begin_submission("old prompt");
        tui.finish_submission(Ok("old answer".into()));
        for character in "preserved draft".chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        assert!(tui.begin_session_load());
        assert!(tui.view().session_loading);
        assert!(!tui.view().running);
        assert_eq!(tui.view().conversation.unwrap().user, "old prompt");

        assert!(cancellation.cancel());
        tui.cancel_session_load();
        release_sender.send(()).unwrap();
        let (outcome, counters) = worker.join().unwrap();
        assert_eq!(outcome, TuiSubmissionOutcome::RouteCancelled);
        assert_eq!(counters, Counts(1, 1, 0, 0));
        assert_eq!(*session.lock().unwrap(), original);
        assert_eq!(
            SessionStore::open(bootstrap.data_directory())
                .unwrap()
                .load_session_for_resume(metadata.id)
                .unwrap()
                .metadata
                .active_agent,
            "retired"
        );
        assert_eq!(tui.view().provider_model, "old-provider / old-model");
        assert_eq!(tui.input(), "preserved draft");
        assert_eq!(tui.view().conversation.unwrap().user, "old prompt");

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn p1c2_resume_parser_restores_only_complete_standard_subagent_turns() {
        let messages = vec![
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("review the patch".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::ToolCall {
                        id: "subagent:42".into(),
                        name: "native::task".into(),
                        input: r#"{"agent":"reviewer","description":"review the patch"}"#.into(),
                    },
                    MessagePart::Reasoning("3 tool uses".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "subagent:42".into(),
                    content: "approved".into(),
                    is_error: false,
                }],
            },
        ];

        assert_eq!(
            resumed_subagent_cards(&messages),
            vec![TuiRuntimeEvent::RestoredCompletedSubagent {
                id: 42,
                agent: "reviewer".into(),
                task_summary: "review the patch".into(),
                final_result: "approved".into(),
                tool_uses: 3,
            }]
        );

        let mut duplicate = messages.clone();
        duplicate.extend(messages.clone());
        assert_eq!(resumed_subagent_cards(&duplicate).len(), 1);

        let mut failed = messages;
        failed[2].parts = vec![MessagePart::ToolResult {
            tool_call_id: "subagent:42".into(),
            content: "failed".into(),
            is_error: true,
        }];
        assert!(resumed_subagent_cards(&failed).is_empty());

        let mut malformed = duplicate[..3].to_vec();
        malformed[1].parts[0] = MessagePart::ToolCall {
            id: "subagent:43".into(),
            name: "native::task".into(),
            input: "not json".into(),
        };
        assert!(resumed_subagent_cards(&malformed).is_empty());

        let incomplete = duplicate[..2].to_vec();
        assert!(resumed_subagent_cards(&incomplete).is_empty());

        let mut transient = duplicate[..3].to_vec();
        transient[2].parts = vec![MessagePart::ToolResult {
            tool_call_id: "subagent:43".into(),
            content: "cancelled".into(),
            is_error: true,
        }];
        assert!(resumed_subagent_cards(&transient).is_empty());
    }

    /// A subagent's instructions are written by the runtime, not typed by the
    /// user, and the card is where they belong. Left in the restored history
    /// they open a turn that reads as the user's own prompt.
    #[test]
    fn a_restored_history_leaves_a_subagent_turn_to_its_card() {
        let messages = vec![
            Message {
                role: agens_core::Role::User,
                parts: vec![MessagePart::Text("lanza un subagente".into())],
            },
            Message {
                role: agens_core::Role::Assistant,
                parts: vec![MessagePart::Text("listo".into())],
            },
            Message {
                role: agens_core::Role::User,
                parts: vec![MessagePart::Text("explore the repository".into())],
            },
            Message {
                role: agens_core::Role::Assistant,
                parts: vec![
                    MessagePart::ToolCall {
                        id: "subagent:1".into(),
                        name: "task".into(),
                        input: r#"{"agent":"explore","description":"explore the repository"}"#
                            .into(),
                    },
                    MessagePart::Reasoning("3 tool uses".into()),
                ],
            },
            Message {
                role: agens_core::Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "subagent:1".into(),
                    content: "a map".into(),
                    is_error: false,
                }],
            },
        ];

        let kept = history_without_subagent_turns(&messages);

        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(
            !kept.iter().any(|message| message
                .parts
                .iter()
                .any(|part| matches!(part, MessagePart::Text(text)
                    if text == "explore the repository"))),
            "{kept:?}"
        );
        assert_eq!(
            resumed_subagent_cards(&messages).len(),
            1,
            "the turn is still restored, as a card"
        );
    }
}

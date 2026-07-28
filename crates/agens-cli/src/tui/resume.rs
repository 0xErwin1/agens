//! Resuming a persisted TUI session: loading it from the sessions store,
//! projecting it into a fresh [`SessionContext`], committing it into the
//! live session slot under a race guard, and reconstructing the restored
//! completed-subagent cards shown for its history. Also ensures a session
//! has an active agent runtime before it can accept native tool calls.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use agens_core::{HeadlessTurnError, Message, MessagePart, RetryBoundary, SessionAttemptStatus};
use agens_store::{SessionStore, StoredSession};
use agens_tools::SkillCatalog;
use agens_tui::{
    Conversation, PaletteEntry, TuiRouteCancellation, TuiRuntimeEvent, TuiSubmissionOutcome,
};

use crate::bootstrap::Bootstrap;
use crate::error::CliError;
use crate::model_registry::ModelSelection;
use crate::permissions::{ParseToolInput, SharedToolDispatcher};
use crate::session::context::{ActiveAgentRuntime, ResumeDraft, SessionContext};
use crate::session::provider::{CredentialResolver, ProviderKind};
use crate::tui::agents::{
    TuiAgentModelValidator, agent_rotation_error, persist_pending_agent_correction,
    reconcile_persisted_active_agent,
};
use crate::tui::session::resume_retry_notice;
use crate::tui::turn::{effective_tui_model, tui_session_presentation};
use crate::turns::sanitize_subagent_summary;

#[cfg(test)]
pub(crate) fn list_tui_sessions(bootstrap: &Bootstrap) -> Result<String, CliError> {
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

pub(crate) struct LoadedTuiSessionResume {
    pub(crate) session: StoredSession,
    pub(crate) retry_boundary: Option<RetryBoundary>,
    pub(crate) confinement_root: std::path::PathBuf,
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
pub(crate) struct ResumedTuiSession {
    pub(crate) context: SessionContext,
    pub(crate) history: Vec<Conversation>,
}

pub(crate) fn resume_tui_session(
    bootstrap: &Bootstrap,
    identifier: i64,
    _skills: &SkillCatalog,
    credentials: &CredentialResolver,
) -> Result<ResumedTuiSession, CliError> {
    let session = load_tui_session_for_resume(bootstrap, identifier)?;
    prepare_loaded_tui_session_resume(bootstrap, identifier, session, credentials)
}

pub(crate) fn load_tui_session_for_resume(
    bootstrap: &Bootstrap,
    identifier: i64,
) -> Result<LoadedTuiSessionResume, CliError> {
    #[cfg(test)]
    crate::test_support::note_tui_resume_load();

    let store = SessionStore::open(bootstrap.data_directory())
        .map_err(|_| CliError::storage("sessions database is unavailable"))?;
    let session = store
        .load_session_for_resume(identifier)
        .map_err(|_| CliError::storage("saved session is unavailable"))?;
    let retry_boundary = session
        .latest_attempt
        .as_ref()
        .filter(|attempt| resume_retry_notice(attempt.status()).is_some())
        .map(|attempt| {
            store
                .load_retry_boundary(attempt.key())
                .map_err(|_| CliError::storage("saved session is unavailable"))
        })
        .transpose()?
        .flatten();
    if session.metadata.completed_turn_count == 0
        && session
            .latest_attempt
            .as_ref()
            .is_none_or(|attempt| attempt.status() != SessionAttemptStatus::Running)
        && retry_boundary.is_none()
    {
        return Err(CliError::storage("saved session is unavailable"));
    }
    let confinement_root = store
        .confinement_root(identifier)
        .map_err(|_| CliError::storage("saved session is unavailable"))?;
    Ok(LoadedTuiSessionResume {
        session,
        retry_boundary,
        confinement_root,
    })
}

pub(crate) fn prepare_loaded_tui_session_resume(
    bootstrap: &Bootstrap,
    identifier: i64,
    loaded: LoadedTuiSessionResume,
    credentials: &CredentialResolver,
) -> Result<ResumedTuiSession, CliError> {
    let LoadedTuiSessionResume {
        session,
        retry_boundary,
        confinement_root,
    } = loaded;
    #[cfg(test)]
    crate::test_support::note_tui_resume_projection();
    let restored_history =
        Conversation::from_messages_with_parser(&session.messages, |name, input| {
            let bare = name
                .strip_prefix("native::")
                .or_else(|| name.strip_prefix("mcp::"))
                .unwrap_or(name);
            agens_core::ToolInput::parse(bare, input)
        })
        .map_err(|_| CliError::storage("saved session is unavailable"))?;
    let saved_provider = session.metadata.provider_id.as_deref();
    let provider = saved_provider.and_then(ProviderKind::parse);
    let selection_provider =
        provider.or_else(|| bootstrap.provider_type().and_then(ProviderKind::parse));
    let selection = match (session.metadata.model_id.as_deref(), selection_provider) {
        (Some(model), Some(provider)) => {
            let mut selector = ModelSelection::for_source(model, provider.source());
            if selector.apply_model(model).is_err() {
                selector
                    .apply_unverified_model(model)
                    .map_err(|_| CliError::storage("saved session selection is unavailable"))?;
            }
            if let Some(effort) = session.metadata.reasoning_effort {
                selector
                    .apply_reasoning_effort(effort.as_str())
                    .map_err(|_| CliError::storage("saved session selection is unavailable"))?;
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
    let mut context = SessionContext::restored(
        identifier,
        session.metadata,
        session.messages,
        confinement_root,
    );
    context.provider = provider;
    context.selection = selection;
    context.resume_error = resume_error;
    if let Some(boundary) = retry_boundary {
        let status = session
            .latest_attempt
            .as_ref()
            .map(agens_core::SessionAttemptSummary::status)
            .ok_or_else(|| CliError::storage("saved session is unavailable"))?;
        context.resume_notice = resume_retry_notice(status).map(str::to_owned);
        context.resume_draft = Some(ResumeDraft::new(boundary.prompt().to_owned()));
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
pub(crate) fn commit_tui_session_resume(
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
        resume_error,
        file_candidates,
        palette_entries,
    })
}

const MAX_RESTORED_SUBAGENT_TOOL_USES: usize = 256;

pub(crate) fn resumed_subagent_cards(messages: &[Message]) -> Vec<TuiRuntimeEvent> {
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
        let Some((agent, description)) = (name == "native::task")
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
pub(crate) fn tui_project_identifier(bootstrap: &Bootstrap) -> Result<String, CliError> {
    crate::session_root::SessionRoot::discover_for_new_session(bootstrap)
        .map(|root| root.path().display().to_string())
        .ok_or_else(|| CliError::configuration("TUI sessions require a project root"))
}

pub(crate) fn ensure_active_tui_agent_runtime(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<SessionContext>>,
    dispatcher: &SharedToolDispatcher,
) -> Result<(), CliError> {
    let dispatcher = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if context.active_agent.is_some() {
        return Ok(());
    }
    let project_root = crate::session_root::resolve_tui_session_root(&context, bootstrap)?;
    let agent = reconcile_persisted_active_agent(bootstrap, &mut context)?;
    let validator = TuiAgentModelValidator::for_context(bootstrap, &context)?;
    let inherited_model = effective_tui_model(bootstrap, &context);
    let active_agent = ActiveAgentRuntime::build(
        &agent,
        Some(&inherited_model),
        &project_root.display().to_string(),
        &dispatcher,
        &validator,
    )
    .map_err(agent_rotation_error)?;
    persist_pending_agent_correction(bootstrap, &mut context);
    context.active_agent = Some(active_agent);
    Ok(())
}

#[cfg(test)]
mod tests {
    use agens_core::{
        HeadlessTurnCancellation, PermissionDecision, PermissionMode, PermissionPattern,
        PermissionPolicy, PermissionRule, PermissionSession, Role, SessionMetadata,
    };
    use agens_tools::{ToolDispatchRequest, ToolEvaluationOutcome, ToolExecutionContext};
    use agens_tui::{Event, Key, Tui, TuiPermissionBridge};
    use rusqlite::Connection;

    use super::*;
    use crate::commands::chat::{chat_args_with_prompt, chat_request};
    use crate::permissions::prompt::production_tui_permission_bridge;
    use crate::session::attempt::attempt_failure_status;
    use crate::test_support::{
        bootstrap_from_a_different_working_directory, persist_tui_session,
        persist_tui_session_metadata, render_tui_test_backend, reset_tui_resume_test_counters,
        rotation_dispatcher, tui_project, tui_resume_test_counters, tui_session_bootstrap,
        tui_session_bootstrap_for_provider, tui_session_directory, tui_session_messages,
    };
    use crate::tools::runner::{TuiTaskControls, TuiTaskLifecycleBridge};
    use crate::tools::task::production_tui_task_runtime;
    use crate::tui::engine::ProductionTuiEngine;
    use crate::tui::models::apply_tui_model;

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
        let resolved_root = crate::session_root::resolve_tui_session_root(
            &session.lock().unwrap(),
            &resume_bootstrap,
        )
        .unwrap();
        assert_eq!(resolved_root, origin.join("project"));
        let runtime = production_tui_task_runtime(
            &resume_bootstrap,
            &resolved_root,
            &SkillCatalog::default(),
            production_tui_permission_bridge().0,
            TuiTaskLifecycleBridge::new(
                agens_tui::BridgeTx::bounded(8).0,
                TuiTaskControls::default(),
            ),
            agens_core::RequestConfig::default(),
            "confinement-check".to_owned(),
        )
        .unwrap();
        ensure_active_tui_agent_runtime(&resume_bootstrap, &session, &runtime.dispatcher).unwrap();

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

        reset_tui_resume_test_counters();
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
        assert_eq!(tui_resume_test_counters(), (1, 1, 0, 0));

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
        let attempt = store
            .begin_session_attempt(&metadata, retry_prompt.into())
            .unwrap();
        store
            .finish_session_attempt(attempt.key(), SessionAttemptStatus::Failed, 30)
            .unwrap();
        let attempt_count = session_attempt_count(&store);
        drop(store);

        reset_tui_resume_test_counters();
        let loaded = load_tui_session_for_resume(&bootstrap, attempt.key().session_id()).unwrap();
        assert_eq!(
            loaded.retry_boundary.as_ref().map(RetryBoundary::prompt),
            Some(retry_prompt)
        );
        let prepared = prepare_loaded_tui_session_resume(
            &bootstrap,
            attempt.key().session_id(),
            loaded,
            &CredentialResolver::production(),
        )
        .unwrap();
        assert_eq!(prepared.context.resume_draft.as_deref(), Some(retry_prompt));
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
        assert_restored_retry_draft_ui(outcome.clone(), retry_prompt);
        let TuiSubmissionOutcome::SessionResumed {
            message,
            history,
            draft,
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
        assert_eq!(tui_resume_test_counters(), (1, 1, 0, 0));

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
        let before_error = session.lock().unwrap().clone();
        assert!(
            prepare_loaded_tui_session_resume(&bootstrap, metadata.id, invalid, &credentials,)
                .is_err()
        );
        assert_eq!(*session.lock().unwrap(), before_error);

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
        reset_tui_resume_test_counters();
        let resumed = resume_tui_session(
            &bootstrap,
            metadata.id,
            &skills,
            &CredentialResolver::production(),
        )
        .unwrap()
        .context;
        assert_eq!(tui_resume_test_counters(), (1, 1, 0, 0));
        let session = Arc::new(Mutex::new(resumed));
        let (permission_bridge, _) = TuiPermissionBridge::channel();
        let (events, _) = agens_tui::BridgeTx::bounded(8);
        let project_root =
            crate::session_root::resolve_tui_session_root(&session.lock().unwrap(), &bootstrap)
                .unwrap();
        let runtime = production_tui_task_runtime(
            &bootstrap,
            &project_root,
            &skills,
            permission_bridge,
            TuiTaskLifecycleBridge::new(events, TuiTaskControls::default()),
            agens_core::RequestConfig::default(),
            "abc12345".to_owned(),
        )
        .unwrap();
        ensure_active_tui_agent_runtime(&bootstrap, &session, &runtime.dispatcher).unwrap();
        assert_eq!(tui_resume_test_counters(), (1, 1, 1, 0));
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
        assert_eq!(tui_resume_test_counters(), (1, 1, 1, 0));

        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn resumed_primary_inherits_every_effective_pinned_model_and_compatible_effort() {
        for provider in ["openai-api", "openai-chatgpt"] {
            for model in [
                "gpt-5.5",
                "gpt-5.6",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
            ] {
                let temporary =
                    tui_session_directory(&format!("resume-primary-{provider}-{model}"));
                let bootstrap =
                    tui_session_bootstrap_for_provider(&temporary, &[], provider, model);
                let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
                let mut metadata =
                    persist_tui_session(&mut store, &tui_project(&temporary), "inherited");
                metadata.provider_id = Some(provider.into());
                metadata.model_id = Some(model.into());
                metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
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
                assert!(resumed.active_agent.is_none());
                let session = Arc::new(Mutex::new(resumed));
                let dispatcher = Arc::new(Mutex::new(rotation_dispatcher()));

                ensure_active_tui_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();

                let context = session.lock().unwrap();
                let active = context.active_agent.as_ref().unwrap();
                assert_eq!(active.name, "primary", "{provider} {model}");
                assert_eq!(active.model.as_deref(), Some(model), "{provider} {model}");
                let request = context
                    .apply_to(chat_request(chat_args_with_prompt("first submission")).unwrap());
                assert_eq!(request.model.as_deref(), Some(model), "{provider} {model}");
                assert_eq!(
                    request.request_config.reasoning_effort(),
                    Some(agens_core::ReasoningEffort::High),
                    "{provider} {model}"
                );
                drop(context);

                std::fs::remove_dir_all(temporary).unwrap();
            }
        }
    }

    #[test]
    fn model_switch_invalidates_and_rematerializes_inherited_primary_without_stale_model() {
        let temporary = tui_session_directory("active-agent-model-switch");
        let bootstrap =
            tui_session_bootstrap_for_provider(&temporary, &[], "openai-api", "gpt-5.5");
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let dispatcher = Arc::new(Mutex::new(rotation_dispatcher()));
        ensure_active_tui_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();
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
        ensure_active_tui_agent_runtime(&bootstrap, &session, &dispatcher).unwrap();

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
        ensure_active_tui_agent_runtime(
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
    fn explicit_agent_models_use_the_provider_aware_effective_registry() {
        for (provider, model, expected_effort) in [
            ("openai-api", "gpt-4o", None),
            ("openai-chatgpt", "gpt-5.4", None),
            ("openai-api", "gpt-5.6-luna", None),
            ("openai-chatgpt", "gpt-5.6-luna", None),
            (
                "openai-api",
                "gpt-5.5",
                Some(agens_core::ReasoningEffort::High),
            ),
            (
                "openai-chatgpt",
                "gpt-5.5",
                Some(agens_core::ReasoningEffort::High),
            ),
        ] {
            let temporary = tui_session_directory(&format!("explicit-{provider}-{model}"));
            let definition = format!(
                "---\nname: reviewer\ndescription: reviewer\nmode: primary\nmodel: {model}\npermissions: []\n---\nReview.\n"
            );
            let bootstrap = tui_session_bootstrap_for_provider(
                &temporary,
                &[("reviewer", &definition)],
                provider,
                "gpt-5.5",
            );
            let mut store = SessionStore::open(bootstrap.data_directory()).unwrap();
            let mut metadata = persist_tui_session_metadata(
                &mut store,
                &tui_project(&temporary),
                "explicit",
                "reviewer",
                100,
            );
            metadata.provider_id = Some(provider.into());
            metadata.model_id = Some("gpt-5.5".into());
            metadata.reasoning_effort = Some(agens_core::ReasoningEffort::High);
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
            let session = Arc::new(Mutex::new(resumed));

            ensure_active_tui_agent_runtime(
                &bootstrap,
                &session,
                &Arc::new(Mutex::new(rotation_dispatcher())),
            )
            .unwrap();

            let context = session.lock().unwrap();
            assert_eq!(context.active_agent.as_ref().unwrap().name, "reviewer");
            assert_eq!(
                context.active_agent.as_ref().unwrap().model.as_deref(),
                Some(model),
                "{provider} {model}"
            );
            let request = context.apply_to(chat_request(chat_args_with_prompt("review")).unwrap());
            assert_eq!(request.model.as_deref(), Some(model), "{provider} {model}");
            assert_eq!(
                request.request_config.reasoning_effort(),
                expected_effort,
                "{provider} {model}"
            );
            drop(context);
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
                reset_tui_resume_test_counters();
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
                (outcome, tui_resume_test_counters())
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
        assert_eq!(counters, (1, 1, 0, 0));
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
}

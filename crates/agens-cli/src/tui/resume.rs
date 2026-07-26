//! Resuming a persisted TUI session: loading it from the sessions store,
//! projecting it into a fresh [`TuiSessionContext`], committing it into the
//! live session slot under a race guard, and reconstructing the restored
//! completed-subagent cards shown for its history. Also ensures a session
//! has an active agent runtime before it can accept native tool calls.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use agens_core::{HeadlessTurnError, Message, MessagePart, RetryBoundary, SessionAttemptStatus};
use agens_store::{SessionStore, StoredSession};
use agens_tools::SkillCatalog;
use agens_tui::{Conversation, TuiRouteCancellation, TuiRuntimeEvent, TuiSubmissionOutcome};

use crate::bootstrap::Bootstrap;
use crate::error::CliError;
use crate::model_registry::TuiModelSelector;
use crate::permissions::{ParseToolInput, SharedToolDispatcher};
use crate::tui::agents::{
    TuiAgentModelValidator, agent_rotation_error, persist_pending_agent_correction,
    reconcile_persisted_active_agent,
};
use crate::tui::provider::{TuiCredentialResolver, TuiProvider};
use crate::tui::session::{
    ActiveAgentRuntime, ResumeDraft, TuiSessionContext, resume_retry_notice,
};
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
}

impl std::ops::Deref for LoadedTuiSessionResume {
    type Target = StoredSession;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

pub(crate) fn resume_tui_session(
    bootstrap: &Bootstrap,
    identifier: i64,
    _skills: &SkillCatalog,
    credentials: &TuiCredentialResolver,
) -> Result<TuiSessionContext, CliError> {
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
    Ok(LoadedTuiSessionResume {
        session,
        retry_boundary,
    })
}

pub(crate) fn prepare_loaded_tui_session_resume(
    bootstrap: &Bootstrap,
    identifier: i64,
    loaded: LoadedTuiSessionResume,
    credentials: &TuiCredentialResolver,
) -> Result<TuiSessionContext, CliError> {
    let LoadedTuiSessionResume {
        session,
        retry_boundary,
    } = loaded;
    if session.metadata.project != tui_project_identifier(bootstrap)? {
        return Err(CliError::storage("saved session is unavailable"));
    }
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
    let provider = saved_provider.and_then(TuiProvider::parse);
    let selection_provider =
        provider.or_else(|| bootstrap.provider_type().and_then(TuiProvider::parse));
    let selection = match (session.metadata.model_id.as_deref(), selection_provider) {
        (Some(model), Some(provider)) => {
            let mut selector = TuiModelSelector::for_source(model, provider.source());
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
    let mut context = TuiSessionContext::restored(
        identifier,
        session.metadata,
        session.messages,
        restored_history,
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
    Ok(context)
}

pub(crate) fn commit_tui_session_resume(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<TuiSessionContext>>,
    expected: &TuiSessionContext,
    mut resumed: TuiSessionContext,
    cancellation: &TuiRouteCancellation,
) -> Result<TuiSubmissionOutcome, CliError> {
    let presentation = tui_session_presentation(bootstrap, &resumed);
    let message = resumed.note();
    let history = std::mem::take(&mut resumed.restored_history);
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
    *current = resumed;

    Ok(TuiSubmissionOutcome::SessionResumed {
        message,
        presentation,
        history,
        draft,
        resume_error,
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

pub(crate) fn tui_project_identifier(bootstrap: &Bootstrap) -> Result<String, CliError> {
    bootstrap
        .project_root()
        .map(|project| project.display().to_string())
        .ok_or_else(|| CliError::configuration("TUI sessions require a project root"))
}

pub(crate) fn ensure_active_tui_agent_runtime(
    bootstrap: &Bootstrap,
    session: &Arc<Mutex<TuiSessionContext>>,
    dispatcher: &SharedToolDispatcher,
) -> Result<(), CliError> {
    let project_root = bootstrap
        .project_root()
        .ok_or_else(|| CliError::configuration("native tools require a project root"))?;
    let dispatcher = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is unavailable"))?;
    let mut context = session
        .lock()
        .map_err(|_| CliError::storage("TUI session is unavailable"))?;
    if context.active_agent.is_some() {
        return Ok(());
    }
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

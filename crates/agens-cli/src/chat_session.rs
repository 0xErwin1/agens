//! What a chat the daemon hosts is made of.
//!
//! The seam is the same one `worker.rs` fills for a run: the control plane
//! decides when a turn runs and who hears about it, and everything a turn is
//! built from — the model, the prompt, the skills, the checkout, the session
//! row it persists against — is decided here, where that knowledge lives.
//!
//! It is the same turn machinery too. A hosted chat calls
//! `run_production_headless_chat_with_progress`, the entry a run's worker
//! drives, rather than a second runner that would have to be kept in step with
//! it.
//!
//! Two things separate a hosted chat from a worker, and both come from the same
//! fact: a chat runs in the user's own checkout, on work nobody scoped in
//! advance.
//!
//! - It does not widen the unmatched permission default. A worker may, because
//!   it runs unattended in a worktree of its own on a scope a person approved.
//!   A chat has neither, so a call the operator did not decide in advance is
//!   refused rather than allowed.
//! - It cannot ask. There is nobody at the other end of this session yet: the
//!   prompts a client would answer are the next unit of AGN-65, and until then
//!   a call that needs a decision is denied with the reason, not left hanging
//!   on a question nothing will answer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agens_bootstrap::Bootstrap;
use agens_core::{
    HeadlessTurnCancellation, HeadlessTurnPortError, Message, PermissionMode, RequestConfig,
    SessionMetadata, TurnProgressSink,
};
use agens_headless::{HeadlessChatRequest, run_production_headless_chat_with_progress};
use agens_permissions::{PermissionPromptAnswer, PermissionPromptContext, PermissionPrompter};
use agens_server::{
    ChatError, ChatHistorySource, ChatSession, ChatSessionFactory, ChatSessionRequest,
    ChatTurnOutcome, ChatTurns, SessionAdmission, SessionBudget, SessionId, SessionProvider,
    SessionRuntime,
};
use agens_store::SessionStore;

/// The factory `agens serve` gives the daemon: how a client's request becomes a
/// chat session.
#[must_use]
pub(crate) fn hosted_chat(bootstrap: &Bootstrap) -> ChatSessionFactory {
    let bootstrap = bootstrap.clone();

    Arc::new(move |request: &ChatSessionRequest| build_chat(&bootstrap, request))
}

/// Where the daemon reads a chat's conversation back from.
///
/// The store rather than the running chat's own memory: the turn owns that
/// memory while it runs, and a reader that waited for it would wait as long as
/// the answer takes.
#[must_use]
pub(crate) fn hosted_chat_history(bootstrap: &Bootstrap) -> ChatHistorySource {
    let data_directory = bootstrap.data_directory().to_path_buf();

    Arc::new(move |session: SessionId| {
        SessionStore::open(&data_directory)
            .and_then(|store| store.load_session_for_resume(session.value()))
            .map(|stored| stored.messages)
            .map_err(|error| ChatError::Unavailable(error.to_string()))
    })
}

fn build_chat(
    bootstrap: &Bootstrap,
    request: &ChatSessionRequest,
) -> Result<ChatSession, ChatError> {
    let checkout = checkout_of(&request.checkout)?;
    let bootstrap = chat_bootstrap(bootstrap, &checkout);
    let session = open_session(&bootstrap, request, &checkout)?;
    let model = model_of(&bootstrap)?;
    let history = resumed_history(&bootstrap, request, session.id);

    Ok(ChatSession {
        admission: SessionAdmission::new(
            SessionId::new(session.id),
            Box::new(ChatProvider {
                model: model.clone(),
            }),
            // A chat is a conversation a person is having. Capping how many
            // turns it may take would end it mid-sentence for a reason the
            // person never asked for; what bounds it is them closing it.
            SessionBudget::unlimited(),
        ),
        turns: Box::new(HostedChat {
            bootstrap,
            session,
            model,
            history,
        }),
    })
}

/// The checkout a chat's tools run in.
///
/// Canonicalized, so the confinement root a tool is held to is the one the
/// filesystem agrees on rather than whatever spelling reached the wire. A path
/// that does not resolve is refused: a chat rooted at a directory that is not
/// there has no project configuration, no AGENTS.md and nowhere to run.
fn checkout_of(checkout: &Path) -> Result<PathBuf, ChatError> {
    checkout.canonicalize().map_err(|error| {
        ChatError::Unavailable(format!(
            "the checkout {} cannot be opened: {error}",
            checkout.display()
        ))
    })
}

/// The configuration this chat runs under: its own MCP connections, and the
/// client's checkout as its project root.
///
/// The root is what makes every session-scoped decision this chat's own rather
/// than the daemon's — the confinement root, the permission grant scope, the
/// project configuration, the AGENTS.md the prompt carries and the directory
/// the tools start in are all derived from it.
fn chat_bootstrap(bootstrap: &Bootstrap, checkout: &Path) -> Bootstrap {
    let mut bootstrap = bootstrap.for_new_session();
    bootstrap.project_root = Some(checkout.to_path_buf());

    bootstrap
}

/// The model this chat speaks to.
fn model_of(bootstrap: &Bootstrap) -> Result<String, ChatError> {
    bootstrap.model().map(ToOwned::to_owned).ok_or_else(|| {
        ChatError::Unavailable("no model is configured for the daemon's chats".to_owned())
    })
}

/// The durable session row this chat persists against.
///
/// A resumed chat reads its row back rather than describing it again: how many
/// turns it has completed and whether it can be resumed are facts of the row,
/// and a second description of them would contradict it.
fn open_session(
    bootstrap: &Bootstrap,
    request: &ChatSessionRequest,
    checkout: &Path,
) -> Result<SessionMetadata, ChatError> {
    let mut store = SessionStore::open(bootstrap.data_directory())
        .map_err(|error| ChatError::Unavailable(error.to_string()))?;

    if let Some(session) = request.resume
        && let Ok(Some(stored)) = store.read_session(session)
    {
        return Ok(stored.metadata);
    }

    let now = now();
    let metadata = SessionMetadata {
        id: 0,
        project: checkout.display().to_string(),
        title: String::new(),
        active_agent: bootstrap
            .default_agent
            .clone()
            .unwrap_or_else(|| "primary".to_owned()),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: now,
        updated_at: now,
        completed_turn_count: 0,
        // Resumability is derived from having completed a turn, and this
        // session has not taken one yet.
        resumable: false,
        parent_session_id: None,
        fork_message_count: None,
    };

    let id = store
        .open_session(&metadata)
        .map_err(|error| ChatError::Unavailable(error.to_string()))?;

    Ok(SessionMetadata { id, ..metadata })
}

/// The transcript a resumed chat comes back to.
///
/// A history that cannot be read is treated as an empty one: the person is
/// coming back to this conversation either way, and refusing to open it over an
/// unreadable transcript would strand a session that is still perfectly usable.
fn resumed_history(
    bootstrap: &Bootstrap,
    request: &ChatSessionRequest,
    session_id: i64,
) -> Vec<Message> {
    if request.resume.is_none() {
        return Vec::new();
    }

    SessionStore::open(bootstrap.data_directory())
        .ok()
        .and_then(|store| store.load_session_for_resume(session_id).ok())
        .map(|stored| stored.messages)
        .unwrap_or_default()
}

/// The provider client the registry lists this session under.
///
/// The client itself is the turn's, built per request from the resolved
/// provider: the registry needs the model this session speaks to, and nothing
/// more of it.
struct ChatProvider {
    model: String,
}

impl SessionProvider for ChatProvider {
    fn model(&self) -> &str {
        &self.model
    }
}

/// One hosted chat, and the conversation it is carrying.
struct HostedChat {
    bootstrap: Bootstrap,
    /// The row this chat persists against, replaced by what each turn wrote so
    /// the next one continues the same session rather than opening another.
    session: SessionMetadata,
    model: String,
    history: Vec<Message>,
}

impl ChatTurns for HostedChat {
    fn run(
        &mut self,
        prompt: &str,
        _runtime: &SessionRuntime,
        cancellation: &HeadlessTurnCancellation,
        progress: &TurnProgressSink,
    ) -> ChatTurnOutcome {
        let request = match self.request_for(prompt) {
            Ok(request) => request,
            Err(error) => return ChatTurnOutcome::Failed(error),
        };

        let completion = run_production_headless_chat_with_progress(
            request,
            &self.bootstrap,
            cancellation,
            Some(progress),
            Box::new(UnattendedChat),
            None,
            None,
        );

        match completion {
            Ok(completion) => {
                self.adopt(completion.metadata, completion.messages);

                ChatTurnOutcome::Completed(completion.text)
            }
            Err(failure) => {
                // A turn that failed part-way still persisted what it did, and
                // the next prompt belongs to that same conversation. Adopting
                // it is what stops a failed turn from silently starting a new
                // one underneath the person.
                if let Some(partial) = failure.partial {
                    self.adopt(partial.metadata, partial.messages);
                }

                ChatTurnOutcome::Failed(failure.error.to_string())
            }
        }
    }
}

impl HostedChat {
    /// The turn one prompt becomes.
    fn request_for(&self, prompt: &str) -> Result<HeadlessChatRequest, String> {
        let project_root = self
            .bootstrap
            .project_root
            .clone()
            .ok_or_else(|| "the chat has no checkout to run in".to_owned())?;
        let skills = agens_bootstrap::discover_skill_catalog(&self.bootstrap, &project_root)
            .map_err(|error| error.to_string())?
            .catalog()
            .clone();

        Ok(HeadlessChatRequest {
            prompt: prompt.to_owned(),
            history: self.history.clone(),
            model: Some(self.model.clone()),
            // Left for the turn to resolve from the project root, which is how
            // a chat gets this project's AGENTS.md and the active agent's own
            // instructions rather than a prompt composed here.
            system_prompt: None,
            max_iterations: None,
            mode: PermissionMode::Edit,
            // Deliberately not widened. See this module's header: a chat runs
            // in the user's own checkout with no approved scope behind it, so
            // what the operator did not decide in advance is refused.
            dangerously_allow_all: false,
            dangerous_mode: false,
            request_config: RequestConfig::default(),
            session_reasoning_effort: None,
            session: Some(self.session.clone()),
            active_agent: None,
            effective_capabilities: None,
            pending_system_reminder: None,
            skills: Some(Arc::new(skills)),
            media_ids: Vec::new(),
            media_mimes: Vec::new(),
        })
    }

    /// Takes on what the turn persisted, so the next prompt continues it.
    ///
    /// The history is adopted whole rather than reconciled against turns
    /// persisted out of band, because a hosted chat runs with no task runtime:
    /// nothing delegates, so there is no subagent turn to come back and find
    /// missing. That changes when delegation reaches this path, and the
    /// reconciliation the terminal already performs is what it will need.
    fn adopt(&mut self, metadata: SessionMetadata, messages: Vec<Message>) {
        self.session = metadata;
        self.history = messages;
    }
}

/// What a permission question does in a chat nobody can answer yet.
///
/// Denied for this call alone, never parked and never allowed. The rules the
/// operator configured are what this chat runs under, and a prompt is by
/// definition something they did not decide in advance; with no client able to
/// answer one, letting the call through would authorize on their behalf.
struct UnattendedChat;

impl PermissionPrompter for UnattendedChat {
    fn prompt(
        &mut self,
        _context: &PermissionPromptContext,
        _cancellation: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
        Ok(PermissionPromptAnswer::DenyOnce)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

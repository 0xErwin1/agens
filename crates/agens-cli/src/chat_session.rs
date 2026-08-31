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
//! - It asks whoever is attached, and refuses when nobody is. A permission
//!   question reaches the clients watching the chat and the turn stops on it,
//!   the way it stops in a terminal. What it will not do is decide one on
//!   nobody's behalf: a question nobody can hear is denied for that call.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agens_agents::{agent_catalog_for_context, ensure_active_agent_runtime};
use agens_bootstrap::Bootstrap;
use agens_core::ask_user::{AskUserPort, AskUserReply, AskUserRequest};
use agens_core::{
    HeadlessTurnCancellation, HeadlessTurnPortError, Message, MessagePart, PermissionMode,
    RequestConfig, SessionMessage, SessionMetadata, TurnProgressSink,
};
use agens_headless::{
    HeadlessChatRequest, run_production_headless_chat_with_progress_and_ask_user,
};
use agens_models::{ModelSelection, QualifiedModel};
use agens_permissions::{PermissionPromptAnswer, PermissionPromptContext, PermissionPrompter};
use agens_server::{
    ChatAsks, ChatError, ChatHistorySource, ChatPermissionAnswer, ChatPermissionRequest,
    ChatPresentation, ChatSession, ChatSessionFactory, ChatSessionRequest, ChatTurnOutcome,
    ChatTurns, SessionAdmission, SessionBudget, SessionId, SessionProvider, SessionRuntime,
};
use agens_session::context::{ActiveAgentRuntime, SessionContext};
use agens_session::model::{current_provider, effective_model, model_source};
use agens_session::provider::{bootstrap_authentication, resolve_provider_for_model};
use agens_store::SessionStore;
use agens_tool_runtime::mcp::ProductionHostedMcp;
use agens_tool_runtime::rotation::rotate_agent;
use agens_tool_runtime::runner::{TuiTaskControls, TuiTaskLifecycleBridge};
use agens_tool_runtime::runtime::production_tool_runtime;
use agens_tool_runtime::task::{ProductionTuiTaskRuntime, production_tui_task_runtime};
use agens_tools::{TaskExecutionId, TaskExecutionRegistry, TaskMessageSource, TaskMessageTarget};

/// The factory `agens serve` gives the daemon: how a client's request becomes a
/// chat session.
#[must_use]
pub(crate) fn hosted_chat_with_tasks(
    bootstrap: &Bootstrap,
    tasks: HostedTaskCoordinator,
) -> ChatSessionFactory {
    let bootstrap = bootstrap.clone();
    Arc::new(move |request: &ChatSessionRequest| build_chat(&bootstrap, request, tasks.clone()))
}

#[cfg(test)]
#[must_use]
pub(crate) fn hosted_chat(bootstrap: &Bootstrap) -> ChatSessionFactory {
    let tasks = hosted_tasks(bootstrap).expect("hosted task journal is available");
    hosted_chat_with_tasks(bootstrap, tasks)
}

pub(crate) fn hosted_catalogs(
    bootstrap: &Bootstrap,
) -> Result<Arc<dyn agens_core::hosted::HostedCatalogs>, ChatError> {
    let root = bootstrap
        .project_root
        .as_deref()
        .unwrap_or_else(|| bootstrap.data_directory());
    let skills = agens_bootstrap::discover_skill_catalog(bootstrap, root)
        .map_err(|error| ChatError::Unavailable(error.to_string()))?;
    let entries = skills
        .catalog()
        .skills()
        .map(|skill| {
            agens_core::hosted::CatalogEntry::new(skill.name(), skill.description(), false)
        })
        .collect::<Vec<_>>();
    let mut hash = Sha256::new();
    for entry in &entries {
        hash.update(entry.name().as_bytes());
        hash.update([0]);
        hash.update(entry.description().as_bytes());
        hash.update([0]);
    }
    let skill =
        agens_core::hosted::CatalogSnapshot::new(format!("sha256:{:x}", hash.finalize()), entries);
    let commands = agens_tui_app::extensions::tui_hosted_builtin_entries();
    let mut command_hash = Sha256::new();
    for entry in &commands {
        command_hash.update(entry.name().as_bytes());
        command_hash.update([0]);
        command_hash.update(entry.description().as_bytes());
        command_hash.update([entry.built_in() as u8]);
    }
    let command = agens_core::hosted::CatalogSnapshot::new(
        format!("sha256:{:x}", command_hash.finalize()),
        commands,
    );
    Ok(Arc::new(agens_server::HostedCatalogSet::new(
        Some(command),
        Some(skill),
    )))
}

pub(crate) fn hosted_files(
    bootstrap: &Bootstrap,
) -> Arc<dyn agens_core::hosted::HostedWorkspaceFiles> {
    Arc::new(agens_server::ConfinedWorkspaceFiles::with_media_store(
        bootstrap.data_directory().to_path_buf(),
    ))
}

pub(crate) fn hosted_mcp(
    bootstrap: &Bootstrap,
) -> Result<Box<dyn agens_core::hosted::HostedMcpControl>, ChatError> {
    let root = bootstrap
        .project_root
        .as_deref()
        .unwrap_or_else(|| bootstrap.data_directory());
    ProductionHostedMcp::new(bootstrap, root)
        .map(|runtime| Box::new(runtime) as Box<dyn agens_core::hosted::HostedMcpControl>)
        .map_err(|error| ChatError::Unavailable(error.to_string()))
}

pub(crate) fn hosted_tasks(bootstrap: &Bootstrap) -> Result<HostedTaskCoordinator, ChatError> {
    agens_store::HostedTaskStore::open(bootstrap.data_directory())
        .map(HostedTaskCoordinator::new)
        .map_err(|error| ChatError::Unavailable(error.to_string()))
}

#[derive(Clone)]
pub(crate) struct HostedTaskCoordinator {
    store: Arc<Mutex<agens_store::HostedTaskStore>>,
    runtimes: Arc<Mutex<BTreeMap<i64, TaskExecutionRegistry>>>,
}

impl HostedTaskCoordinator {
    fn new(store: agens_store::HostedTaskStore) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            runtimes: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
    fn register(&self, session: i64, registry: TaskExecutionRegistry) {
        if let Ok(mut runtimes) = self.runtimes.lock() {
            runtimes.insert(session, registry);
        }
    }
    fn persist_runtime_event(&self, session: i64, event: &agens_core::TuiRuntimeEvent) -> bool {
        if let agens_core::TuiRuntimeEvent::SubagentExecution(event) = event
            && let agens_core::TuiSubagentUpdate::Terminal {
                status,
                final_result,
            } = &event.update
        {
            let payload = serde_json::json!({"status":format!("{status:?}"),"result":final_result})
                .to_string();
            return self
                .store
                .lock()
                .ok()
                .and_then(|mut store| {
                    store
                        .persist_completed_child_turn(session, &event.id.to_string(), 1, &payload)
                        .ok()
                })
                .is_some();
        }
        let agens_core::TuiRuntimeEvent::TaskExecution { agent, event } = event else {
            return true;
        };
        let (id, state, detail) = match event {
            agens_core::TuiExecutionEvent::ForegroundStarted { id } => (
                *id,
                agens_core::hosted::HostedTaskState::Running,
                "foreground",
            ),
            agens_core::TuiExecutionEvent::BackgroundStarted { id }
            | agens_core::TuiExecutionEvent::Backgrounded { id } => (
                *id,
                agens_core::hosted::HostedTaskState::Background,
                "background",
            ),
            agens_core::TuiExecutionEvent::CancellationRequested { .. } => return true,
            agens_core::TuiExecutionEvent::Completed { id } => (
                *id,
                agens_core::hosted::HostedTaskState::Completed,
                "completed",
            ),
            agens_core::TuiExecutionEvent::Failed { id } => {
                (*id, agens_core::hosted::HostedTaskState::Failed, "failed")
            }
            agens_core::TuiExecutionEvent::Cancelled { id } => (
                *id,
                agens_core::hosted::HostedTaskState::Cancelled,
                "cancelled",
            ),
        };
        self.store
            .lock()
            .ok()
            .and_then(|mut store| {
                store
                    .append_event(
                        session,
                        &id.to_string(),
                        state,
                        &format!("{detail}:{agent}"),
                    )
                    .ok()
            })
            .is_some()
    }
}

impl agens_core::hosted::HostedTaskJournal for HostedTaskCoordinator {
    fn append_event(
        &mut self,
        session_id: i64,
        task_id: &str,
        state: agens_core::hosted::HostedTaskState,
        payload: &str,
    ) -> Result<agens_core::hosted::HostedTaskEvent, agens_core::hosted::TaskControlError> {
        self.store
            .lock()
            .map_err(|_| agens_core::hosted::TaskControlError::Storage)?
            .append_event(session_id, task_id, state, payload)
            .map_err(|_| agens_core::hosted::TaskControlError::Storage)
    }
    fn persist_completed_child_turn(
        &mut self,
        session_id: i64,
        task_id: &str,
        sequence: u64,
        payload: &str,
    ) -> Result<(), agens_core::hosted::TaskControlError> {
        self.store
            .lock()
            .map_err(|_| agens_core::hosted::TaskControlError::Storage)?
            .persist_completed_child_turn(session_id, task_id, sequence, payload)
            .map_err(|_| agens_core::hosted::TaskControlError::Storage)
    }
    fn completed_child_turns(
        &self,
        session_id: i64,
    ) -> Result<Vec<agens_core::hosted::HostedChildTurn>, agens_core::hosted::TaskControlError>
    {
        self.store
            .lock()
            .map_err(|_| agens_core::hosted::TaskControlError::Storage)?
            .completed_child_turns(session_id)
            .map_err(|_| agens_core::hosted::TaskControlError::Storage)
    }
    fn snapshot_tail(
        &self,
        session_id: i64,
    ) -> Result<agens_core::hosted::HostedTaskReplay, agens_core::hosted::TaskControlError> {
        self.store
            .lock()
            .map_err(|_| agens_core::hosted::TaskControlError::Storage)?
            .snapshot_tail(session_id)
            .map_err(|_| agens_core::hosted::TaskControlError::Storage)
    }
    fn replay_after(
        &self,
        session_id: i64,
        after_cursor: u64,
    ) -> Result<agens_core::hosted::HostedTaskReplay, agens_core::hosted::TaskControlError> {
        self.store
            .lock()
            .map_err(|_| agens_core::hosted::TaskControlError::Storage)?
            .replay_after(session_id, after_cursor)
            .map_err(|_| agens_core::hosted::TaskControlError::Storage)
    }
    fn apply_control(
        &mut self,
        command: &agens_core::hosted::HostedControlCommand,
    ) -> Result<agens_core::hosted::HostedControlResult, agens_core::hosted::TaskControlError> {
        self.store
            .lock()
            .map_err(|_| agens_core::hosted::TaskControlError::Storage)?
            .apply_control_with(command, || {
                let registry = self
                    .runtimes
                    .lock()
                    .ok()
                    .and_then(|r| r.get(&command.session_id()).cloned())
                    .ok_or(agens_core::hosted::TaskControlError::WrongSession)?;
                let id = command
                    .task_id()
                    .and_then(|id| id.parse::<u64>().ok())
                    .map(TaskExecutionId::from_value);
                let applied = match command.kind() {
                    agens_core::hosted::HostedControlKind::Background => {
                        id.is_some_and(|id| registry.transition_to_background(id))
                    }
                    agens_core::hosted::HostedControlKind::Cancel => {
                        id.is_some_and(|id| registry.cancel(id))
                    }
                    agens_core::hosted::HostedControlKind::CancelAll => {
                        registry.cancel_all();
                        true
                    }
                    agens_core::hosted::HostedControlKind::Message(message) => {
                        id.is_some_and(|id| {
                            registry
                                .send_message(
                                    TaskMessageSource::User,
                                    TaskMessageTarget::Execution(id),
                                    message.clone(),
                                )
                                .is_ok()
                        })
                    }
                };
                if !applied {
                    return Err(agens_core::hosted::TaskControlError::InvalidTransition);
                }
                Ok(())
            })
            .map_err(|error| error.kind())
    }
}

/// Where the daemon reads a chat's conversation back from.
///
/// The store rather than the running chat's own memory: the turn owns that
/// memory while it runs, and a reader that waited for it would wait as long as
/// the answer takes.
///
/// Read without the resumability gate: a chat's row exists before its first
/// turn completes, and a client rejoining it before then is owed the empty
/// thread, not a refusal for a session the daemon just opened for it.
#[must_use]
pub(crate) fn hosted_chat_history(bootstrap: &Bootstrap) -> ChatHistorySource {
    let data_directory = bootstrap.data_directory().to_path_buf();

    Arc::new(move |session: SessionId| {
        SessionStore::open(&data_directory)
            .and_then(|store| store.load_session_thread(session.value()))
            .map(|stored| stored.messages)
            .map_err(|error| ChatError::Unavailable(error.to_string()))
    })
}

fn build_chat(
    bootstrap: &Bootstrap,
    request: &ChatSessionRequest,
    tasks: HostedTaskCoordinator,
) -> Result<ChatSession, ChatError> {
    let checkout = checkout_of(&request.checkout)?;
    let bootstrap = chat_bootstrap(bootstrap, &checkout);
    let session = open_session(&bootstrap, request, &checkout)?;
    let (model, request_config) = match restored_selection(&bootstrap, &session)? {
        Some(selection) => selection,
        None => (model_of(&bootstrap)?, RequestConfig::default()),
    };

    // A fresh chat's history is known empty. A resumed one is read on the
    // first turn that needs it rather than here: `open` is what an attaching
    // client waits on, and the client reads the same thread through the
    // history RPC anyway, so an eager read here would charge every attach a
    // second full-thread load that grows with the conversation.
    let history = request.resume.is_none().then(Vec::new);

    let active_agent = active_agent_for(&bootstrap, &session)?;

    // The session's own recorded value wins over configuration; the configured
    // default only seeds a session that never recorded one — the same rule a
    // local resume applies, so hosted and local agree about the same session.
    let persisted_bypass = SessionStore::open(bootstrap.data_directory())
        .and_then(|store| store.bypass_permission_prompts(session.id))
        .unwrap_or_default();
    let configured_bypass = agens_bootstrap::session_config::SessionConfig::resolve(
        &agens_bootstrap::session_root::SessionRoot::confined_to(checkout.clone()),
        &bootstrap,
    )
    .ok()
    .map(|config| config.bypass_permission_prompts());
    let bypass_permissions = persisted_bypass.or(configured_bypass).unwrap_or(false);

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
        presentation: presentation_of(&bootstrap, &session, bypass_permissions),
        turns: Box::new(HostedChat {
            bootstrap,
            session,
            model,
            history,
            request_config,
            active_agent,
            tasks,
            task_runtime: None,
            bypass_permissions,
            dangerous_mode: false,
        }),
    })
}

/// How this session presents itself to an attaching surface.
///
/// Computed the way `tui_session_presentation` computes it for a local launch,
/// from the same persisted metadata a resumed local session reads, so an
/// attached footer and a local one describe the same session identically.
fn presentation_of(
    bootstrap: &Bootstrap,
    session: &SessionMetadata,
    bypass_permissions: bool,
) -> ChatPresentation {
    let context = hosted_context(bootstrap, session, None);
    let model = effective_model(bootstrap, &context);

    let provider = session
        .provider_id
        .clone()
        .or_else(|| current_provider(bootstrap, &context).map(|kind| kind.identifier().to_owned()));

    let reasoning_effort = session
        .reasoning_effort
        .map(|effort| effort.as_str().to_owned())
        .or_else(|| {
            ModelSelection::for_source(&model, model_source(bootstrap, &context))
                .reasoning_effort_default()
                .map(str::to_owned)
        });

    ChatPresentation {
        context_window: agens_models::context_window_for(&model),
        provider,
        model: Some(model),
        reasoning_effort,
        bypass_permissions,
        // A freshly built session object always starts with dangerous mode
        // off; the toggle reply, not the arrival, is what turns the footer on.
        dangerous_mode: false,
    }
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

fn restored_selection(
    bootstrap: &Bootstrap,
    session: &SessionMetadata,
) -> Result<Option<(String, RequestConfig)>, ChatError> {
    let (provider, model) = match (&session.provider_id, &session.model_id) {
        (None, None) => return Ok(None),
        (Some(provider), Some(model)) => (provider, model),
        _ => {
            return Err(ChatError::Unavailable(
                "the persisted session model selection is incomplete".to_owned(),
            ));
        }
    };
    let qualified = format!("{provider}/{model}");
    let resolved =
        resolve_provider_for_model(Some(&qualified), &bootstrap_authentication(bootstrap))
            .map_err(|_| {
                ChatError::Unavailable(
                    "the persisted session model selection is unavailable".to_owned(),
                )
            })?;
    let mut selection = ModelSelection::for_source(&resolved.model, resolved.provider.source());
    selection.apply_model(&resolved.model).map_err(|error| {
        ChatError::Unavailable(format!(
            "the persisted session model selection is stale: {error}"
        ))
    })?;

    if let Some(effort) = session.reasoning_effort {
        selection
            .apply_reasoning_effort(effort.as_str())
            .map_err(|error| {
                ChatError::Unavailable(format!(
                    "the persisted session reasoning effort is stale: {error}"
                ))
            })?;
    }

    Ok(Some((qualified, selection.request_config().clone())))
}

fn active_agent_for(
    bootstrap: &Bootstrap,
    session: &SessionMetadata,
) -> Result<ActiveAgentRuntime, ChatError> {
    let context = Arc::new(std::sync::Mutex::new(hosted_context(
        bootstrap, session, None,
    )));
    let project_root = bootstrap
        .project_root
        .as_deref()
        .ok_or_else(|| ChatError::Unavailable("the chat has no checkout to run in".to_owned()))?;
    let skills = agens_bootstrap::discover_skill_catalog(bootstrap, project_root)
        .map_err(|error| ChatError::Unavailable(error.to_string()))?
        .catalog()
        .clone();
    let (_, dispatcher) = production_tool_runtime(bootstrap, project_root, Some(&skills))
        .map_err(|error| ChatError::Unavailable(error.to_string()))?;
    ensure_active_agent_runtime(bootstrap, &context, &dispatcher)
        .map_err(|error| ChatError::Unavailable(error.to_string()))?;

    context
        .lock()
        .map_err(|_| ChatError::Unavailable("hosted agent state is unavailable".to_owned()))?
        .active_agent
        .clone()
        .ok_or_else(|| ChatError::Unavailable("the active agent is unavailable".to_owned()))
}

fn hosted_context(
    bootstrap: &Bootstrap,
    session: &SessionMetadata,
    active_agent: Option<ActiveAgentRuntime>,
) -> SessionContext {
    SessionContext {
        identifier: Some(session.id),
        metadata: Some(session.clone()),
        confinement_root: bootstrap.project_root.clone(),
        active_agent,
        ..SessionContext::fresh()
    }
}

/// The transcript a resumed chat comes back to.
///
/// A history that cannot be read is treated as an empty one: the person is
/// coming back to this conversation either way, and refusing to open it over an
/// unreadable transcript would strand a session that is still perfectly usable.
fn stored_thread(bootstrap: &Bootstrap, session_id: i64) -> Vec<Message> {
    SessionStore::open(bootstrap.data_directory())
        .ok()
        .and_then(|store| store.load_session_thread(session_id).ok())
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
    /// The conversation so far. `None` for a resumed chat until the first turn
    /// reads the stored thread, so opening a chat never pays for a transcript
    /// no turn has asked for yet.
    history: Option<Vec<Message>>,
    request_config: RequestConfig,
    active_agent: ActiveAgentRuntime,
    tasks: HostedTaskCoordinator,
    task_runtime: Option<ProductionTuiTaskRuntime>,
    /// Whether this session bypasses Ask permission prompts. Mirrors the
    /// persisted per-session flag; toggled by the hosted `/bypass` command.
    bypass_permissions: bool,
    /// Whether this session runs in dangerous mode. Ephemeral, like the local
    /// toggle: it lives only as long as this hosted session object.
    dangerous_mode: bool,
}

impl ChatTurns for HostedChat {
    /// How this chat is configured right now, computed from the same session
    /// metadata the open answer describes, so a footer redressed after a
    /// command reads identically to one dressed by attaching afresh. Dangerous
    /// mode is this object's own ephemeral flag rather than anything persisted.
    fn presentation(&self) -> Option<ChatPresentation> {
        let mut presentation =
            presentation_of(&self.bootstrap, &self.session, self.bypass_permissions);
        presentation.dangerous_mode = self.dangerous_mode;

        Some(presentation)
    }

    fn command(&mut self, command: &str) -> Result<String, ChatError> {
        if command == "/agents" {
            return self.list_agents();
        }
        if let Some(agent) = command.strip_prefix("/agent ") {
            return self.select_agent(agent.trim());
        }
        if let Some(model) = command.strip_prefix("/model ") {
            return self.select_model(model.trim());
        }
        if command == "/bypass" {
            return self.toggle_bypass();
        }
        if command == "/dangerous" {
            self.dangerous_mode = !self.dangerous_mode;
            return Ok(if self.dangerous_mode {
                agens_core::hosted::DANGEROUS_ON_REPLY.to_owned()
            } else {
                agens_core::hosted::DANGEROUS_OFF_REPLY.to_owned()
            });
        }

        let effort = command
            .strip_prefix("/effort ")
            .ok_or_else(|| ChatError::Unavailable("unsupported hosted command".to_owned()))?
            .trim();
        let resolved = resolve_provider_for_model(
            Some(&self.model),
            &bootstrap_authentication(&self.bootstrap),
        )
        .map_err(|_| ChatError::Unavailable("the configured provider is unavailable".to_owned()))?;
        let model = QualifiedModel::parse(&self.model)
            .map_err(|error| ChatError::Unavailable(error.to_string()))?;
        let mut selection = ModelSelection::for_source(model.model(), resolved.provider.source());
        selection
            .apply_reasoning_effort(effort)
            .map_err(ChatError::Unavailable)?;

        self.session.provider_id = Some(resolved.provider.identifier().to_owned());
        self.session.model_id = Some(model.model().to_owned());
        self.session.reasoning_effort = selection.reasoning_effort_value();
        self.request_config = selection.request_config().clone();
        SessionStore::open(self.bootstrap.data_directory())
            .and_then(|mut store| store.update_session_selection(&self.session))
            .map_err(|_| {
                ChatError::Unavailable("session selection could not be saved".to_owned())
            })?;

        Ok(format!("Reasoning effort: {effort}."))
    }

    fn run(
        &mut self,
        message: &SessionMessage,
        _runtime: &SessionRuntime,
        cancellation: &HeadlessTurnCancellation,
        asks: &Arc<dyn ChatAsks>,
        progress: &TurnProgressSink,
    ) -> ChatTurnOutcome {
        let mut request = match self.request_for(message) {
            Ok(request) => request,
            Err(error) => return ChatTurnOutcome::Failed(error),
        };

        if self.task_runtime.is_none() {
            let project_root = match self.bootstrap.project_root.as_deref() {
                Some(root) => root,
                None => {
                    return ChatTurnOutcome::Failed("the chat has no checkout to run in".into());
                }
            };
            let skills =
                match agens_bootstrap::discover_skill_catalog(&self.bootstrap, project_root) {
                    Ok(skills) => skills.catalog().clone(),
                    Err(error) => return ChatTurnOutcome::Failed(error.to_string()),
                };
            let registry = TaskExecutionRegistry::default();
            let (events, receiver) = agens_tui::BridgeTx::bounded(256);
            std::thread::spawn(move || while receiver.recv().is_ok() {});
            let session_id = self.session.id;
            let event_tasks = self.tasks.clone();
            let lifecycle = TuiTaskLifecycleBridge::new(events, TuiTaskControls(registry.clone()))
                .with_event_writer(Arc::new(move |event| {
                    event_tasks.persist_runtime_event(session_id, event)
                }));
            let runtime = production_tui_task_runtime(
                &self.bootstrap,
                project_root,
                &skills,
                Box::new(AttachedPrompter {
                    asks: Arc::clone(asks),
                }),
                lifecycle,
                self.request_config.clone(),
                format!("hosted-task-{session_id}"),
                false,
                Box::new(AttachedAskUserPort {
                    asks: Arc::clone(asks),
                }),
            );
            match runtime {
                Ok(runtime) => {
                    self.tasks
                        .register(session_id, runtime.task_registry.clone());
                    self.task_runtime = Some(runtime);
                }
                Err(error) => return ChatTurnOutcome::Failed(error.to_string()),
            }
        }

        if let Some(project_root) = self.bootstrap.project_root.as_deref() {
            request.system_prompt = match agens_headless::headless_turn_own_system_prompt(
                &self.bootstrap,
                project_root,
                request.system_prompt.take(),
            ) {
                Ok(prompt) => Some(prompt),
                Err(error) => return ChatTurnOutcome::Failed(error.to_string()),
            };
        }

        let completion = run_production_headless_chat_with_progress_and_ask_user(
            request,
            &self.bootstrap,
            cancellation,
            Some(progress),
            Box::new({
                let asks = Arc::clone(asks);
                move |_| {
                    Box::new(AttachedPrompter {
                        asks: Arc::clone(&asks),
                    }) as Box<dyn PermissionPrompter>
                }
            }),
            self.task_runtime.as_ref(),
            None,
            Some(Box::new(AttachedAskUserPort {
                asks: Arc::clone(asks),
            })),
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
    /// Flips this session's permission-bypass state and persists it, so the
    /// next turn and the next attach both see the same decision.
    fn toggle_bypass(&mut self) -> Result<String, ChatError> {
        let enabled = !self.bypass_permissions;
        SessionStore::open(self.bootstrap.data_directory())
            .and_then(|mut store| store.set_bypass_permission_prompts(self.session.id, enabled))
            .map_err(|_| {
                ChatError::Unavailable("permission bypass state could not be saved".to_owned())
            })?;
        self.bypass_permissions = enabled;

        Ok(if enabled {
            agens_core::hosted::BYPASS_ON_REPLY.to_owned()
        } else {
            agens_core::hosted::BYPASS_OFF_REPLY.to_owned()
        })
    }

    fn list_agents(&self) -> Result<String, ChatError> {
        let context = hosted_context(
            &self.bootstrap,
            &self.session,
            Some(self.active_agent.clone()),
        );
        let catalog = agent_catalog_for_context(&self.bootstrap, &context)
            .map_err(|error| ChatError::Unavailable(error.to_string()))?;
        let current = self.active_agent.name.as_str();
        let entries = catalog.primary_or_all().map(|agent| {
            if agent.name == current {
                format!("{} (current)", agent.name)
            } else {
                agent.name.clone()
            }
        });

        Ok(std::iter::once("Eligible primary agents:".to_owned())
            .chain(entries)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn select_agent(&mut self, requested: &str) -> Result<String, ChatError> {
        if requested == "praetor" {
            return Err(ChatError::Unavailable(
                "praetor is team mode, not an agent profile; use /team".to_owned(),
            ));
        }

        let project_root = self.bootstrap.project_root.as_deref().ok_or_else(|| {
            ChatError::Unavailable("the chat has no checkout to run in".to_owned())
        })?;
        let skills = agens_bootstrap::discover_skill_catalog(&self.bootstrap, project_root)
            .map_err(|error| ChatError::Unavailable(error.to_string()))?
            .catalog()
            .clone();
        let context = Arc::new(std::sync::Mutex::new(hosted_context(
            &self.bootstrap,
            &self.session,
            Some(self.active_agent.clone()),
        )));
        let message = rotate_agent(&self.bootstrap, requested, &context, &skills)
            .map_err(|error| ChatError::Unavailable(error.to_string()))?;
        let context = context
            .lock()
            .map_err(|_| ChatError::Unavailable("hosted agent state is unavailable".to_owned()))?;
        self.session = context.metadata.clone().ok_or_else(|| {
            ChatError::Unavailable("hosted session state is unavailable".to_owned())
        })?;
        self.active_agent = context
            .active_agent
            .clone()
            .ok_or_else(|| ChatError::Unavailable("the active agent is unavailable".to_owned()))?;

        Ok(message)
    }

    fn select_model(&mut self, requested: &str) -> Result<String, ChatError> {
        let parsed = QualifiedModel::parse(requested)
            .map_err(|error| ChatError::Unavailable(error.to_string()))?;
        if parsed.source().is_none() {
            return Err(ChatError::Unavailable(
                "hosted model commands require provider/model".to_owned(),
            ));
        }

        let authentication = bootstrap_authentication(&self.bootstrap);
        let current = resolve_provider_for_model(Some(&self.model), &authentication)
            .map_err(|_| ChatError::Unavailable("the configured provider is unavailable".into()))?;
        let resolved = resolve_provider_for_model(Some(requested), &authentication)
            .map_err(|_| ChatError::Unavailable("the requested provider is unavailable".into()))?;
        let mut selection = ModelSelection::for_source(&resolved.model, resolved.provider.source());
        selection
            .apply_model(&resolved.model)
            .map_err(ChatError::Unavailable)?;

        let reset_effort = self
            .session
            .reasoning_effort
            .filter(|effort| selection.apply_reasoning_effort(effort.as_str()).is_err());
        let model = format!("{}/{}", resolved.provider.identifier(), resolved.model);
        let mut session = self.session.clone();
        session.provider_id = Some(resolved.provider.identifier().to_owned());
        session.model_id = Some(resolved.model.clone());
        session.reasoning_effort = selection.reasoning_effort_value();
        let active_agent = active_agent_for(&self.bootstrap, &session)?;
        SessionStore::open(self.bootstrap.data_directory())
            .and_then(|mut store| store.update_session_selection(&session))
            .map_err(|_| {
                ChatError::Unavailable("session selection could not be saved".to_owned())
            })?;

        self.session = session;
        self.model = model;
        self.request_config = selection.request_config().clone();
        self.active_agent = active_agent;

        let selected = reset_effort.map_or_else(
            || format!("Model: {}.", resolved.model),
            |effort| {
                format!(
                    "Model: {}. Reasoning effort reset to Default because {} is unsupported.",
                    resolved.model,
                    effort.as_str()
                )
            },
        );
        Ok(if current.provider == resolved.provider {
            selected
        } else {
            format!("Provider: {}. {selected}", resolved.provider.label())
        })
    }

    /// The conversation this chat continues from, read lazily for a resumed
    /// chat so `open` never blocks a client on a thread load.
    fn stored_history(&mut self) -> &[Message] {
        if self.history.is_none() {
            self.history = Some(stored_thread(&self.bootstrap, self.session.id));
        }

        self.history.as_deref().unwrap_or_default()
    }

    /// The turn one prompt becomes.
    fn request_for(&mut self, message: &SessionMessage) -> Result<HeadlessChatRequest, String> {
        let project_root = self
            .bootstrap
            .project_root
            .clone()
            .ok_or_else(|| "the chat has no checkout to run in".to_owned())?;
        let skills = agens_bootstrap::discover_skill_catalog(&self.bootstrap, &project_root)
            .map_err(|error| error.to_string())?
            .catalog()
            .clone();

        let prompt = message
            .as_message()
            .parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let (media_ids, media_mimes): (Vec<_>, Vec<_>) = message
            .as_message()
            .parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Media { media_id, mime } => Some((*media_id, mime.clone())),
                _ => None,
            })
            .unzip();

        let history = self.stored_history().to_vec();
        let project_prompt = agens_bootstrap::session_config::SessionInstructions::resolve(
            &agens_bootstrap::session_root::SessionRoot::confined_to(project_root),
            &self.bootstrap,
        );
        let system_prompt = project_prompt
            .text()
            .filter(|instructions| !instructions.is_empty())
            .and_then(|instructions| {
                self.active_agent
                    .system_prompt
                    .strip_suffix(&format!("\n\n{instructions}"))
            })
            .unwrap_or(&self.active_agent.system_prompt)
            .to_owned();
        let active_model = self.active_agent.model.as_deref();
        let selected_model = QualifiedModel::parse(&self.model)
            .map(|model| model.model().to_owned())
            .unwrap_or_else(|_| self.model.clone());
        let overrides_selection = active_model.is_some_and(|model| model != selected_model);

        Ok(HeadlessChatRequest {
            prompt,
            user_message: Some(message.clone()),
            history,
            model: active_model
                .map(ToOwned::to_owned)
                .or_else(|| Some(self.model.clone())),
            system_prompt: Some(system_prompt),
            max_iterations: None,
            mode: PermissionMode::Edit,
            // Widened only by the operator's own persisted `/bypass` toggle:
            // a chat runs in the user's checkout with no approved scope behind
            // it, so what the operator did not decide is still refused.
            dangerously_allow_all: self.bypass_permissions,
            dangerous_mode: self.dangerous_mode,
            request_config: if overrides_selection {
                RequestConfig::default()
            } else {
                self.request_config.clone()
            },
            session_reasoning_effort: (!overrides_selection)
                .then_some(self.session.reasoning_effort)
                .flatten(),
            session: Some(self.session.clone()),
            active_agent: Some(self.active_agent.name.clone()),
            effective_capabilities: Some(self.active_agent.capabilities.clone()),
            pending_system_reminder: None,
            skills: Some(Arc::new(skills)),
            media_ids,
            media_mimes,
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
        self.history = Some(messages);
    }
}

/// A permission question, asked of whoever is watching this chat.
///
/// The turn stops on it, the way it stops in a terminal. What this will not do
/// is decide one on nobody's behalf: a question nobody can hear is denied for
/// that call, because the rules the operator configured are what this chat runs
/// under and a prompt is by definition something they did not decide in
/// advance.
struct AttachedAskUserPort {
    asks: Arc<dyn ChatAsks>,
}

impl AskUserPort for AttachedAskUserPort {
    fn ask(
        &self,
        request: &AskUserRequest,
        _cancellation: &HeadlessTurnCancellation,
    ) -> AskUserReply {
        self.asks.ask_user(request)
    }
}

struct AttachedPrompter {
    asks: Arc<dyn ChatAsks>,
}

impl PermissionPrompter for AttachedPrompter {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
        _cancellation: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
        let request = ChatPermissionRequest {
            // Reduced from the dispatcher's own identity, which equals no
            // spelling a person writes.
            tool: agens_core::bare_tool_name(&context.tool_identity).into_owned(),
            target: agens_permissions::sanitize_permission_target(
                &context.tool_identity,
                &context.target_identifier,
            ),
            // The same spelling the terminal's own prompt shows, so a question
            // reads identically whether the turn ran here or in this process.
            access: format!("{:?}", context.access),
            reason: context.reason.clone(),
        };

        Ok(match self.asks.permission(&request) {
            ChatPermissionAnswer::AllowOnce => PermissionPromptAnswer::AllowOnce,
            ChatPermissionAnswer::AllowAlways => PermissionPromptAnswer::AllowAlways,
            ChatPermissionAnswer::DenyAlways => PermissionPromptAnswer::DenyAlways,
            ChatPermissionAnswer::DenyOnce | ChatPermissionAnswer::Unheard => {
                PermissionPromptAnswer::DenyOnce
            }
        })
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

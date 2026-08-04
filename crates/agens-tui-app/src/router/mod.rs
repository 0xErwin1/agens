//! The router: what the terminal asks the runtime to do.
//!
//! One type, `TuiRuntimeRouter`, with its methods grouped by the question they
//! answer — routing a submission, opening a dialog, rebuilding the extensions a
//! session sees, resolving a slash command, changing provider. They sit in
//! separate files because one 1100-line `impl` block is not navigable, not
//! because they are separate concepts.

mod dialogs;
mod extensions;
mod provider;
mod resolve;
mod routing;

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::profiles::{AgentProfileStore, ProfileEditor};
use agens_core::HeadlessTurnCancellation;
use agens_tools::{
    CommandBusyPolicy, CommandCatalog, McpStatusHandle, SkillCatalog, ToolDispatcher,
};
use agens_tui::{PaletteEntry, TuiProviderOutcome, TuiSubmissionOutcome};

use crate::extensions::resolved_tui_palette;
use agens_agents::subagent_catalog;
use agens_auth::ChatGptAuthCoordinator;
use agens_bootstrap::Bootstrap;
use agens_error::CliError;
use agens_session::context::SessionContext;
use agens_session::context::current_session_timestamp;
use agens_session::provider::{
    ChatGptCredentialSnapshot, CredentialResolver, restore_chatgpt_credentials,
};
use agens_tool_runtime::mcp::{ProductionMcpRuntime, load_configured_mcp_registry};

pub const TUI_ERROR_ACTION: &str = "Correct the command or runtime condition, then retry.";

/// The only busy-session policy for a resolved input route.
///
/// This lives with the router because route classification must happen before
/// the TUI mutates its prompt queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BusyPolicy {
    Local,
    Queue,
    Reject,
    Quit,
    Invalid,
}

impl BusyPolicy {
    fn from_catalog_policy(policy: CommandBusyPolicy) -> Self {
        match policy {
            CommandBusyPolicy::Local => Self::Local,
            CommandBusyPolicy::ProviderTurn => Self::Queue,
            CommandBusyPolicy::IdleOnly => Self::Reject,
            CommandBusyPolicy::Quit => Self::Quit,
            CommandBusyPolicy::Invalid => Self::Invalid,
        }
    }
}

#[derive(Clone)]
pub struct TuiRuntimeRouter {
    bootstrap: Arc<Mutex<Bootstrap>>,
    pub session: Arc<Mutex<SessionContext>>,
    cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
    auth: ChatGptAuthCoordinator,
    credentials: CredentialResolver,
    /// Commands, skills, and the derived command palette, bundled behind one lock so a
    /// post-startup resume can swap all three atomically to the resumed session's own root
    /// instead of leaving them pinned to whatever root the router was constructed with.
    extensions: Arc<Mutex<RouterExtensions>>,
    pub mcp_status: McpStatusHandle,
    /// The router's own long-lived MCP runtime: it registers every configured
    /// server's descriptor so `/mcp` can show it, but it never connects on
    /// its own. A connect attempt only happens when the user explicitly
    /// reconnects (`r` in the `/mcp` overlay), which reuses this same
    /// registry rather than building a throwaway one.
    mcp_runtime: Arc<Mutex<ProductionMcpRuntime>>,
    clock: fn() -> i64,
    credential_restorer: Arc<CredentialRestorer>,
    profile_editor: Arc<Mutex<Option<ProfileEditor>>>,
    profile_focus: Arc<Mutex<Option<String>>>,
    profile_store: Option<Arc<dyn AgentProfileStore>>,
}

struct RouterExtensions {
    commands: Arc<CommandCatalog>,
    skills: Arc<SkillCatalog>,
    palette: Vec<PaletteEntry>,
}

type CredentialRestorer =
    dyn Fn(&Path, ChatGptCredentialSnapshot) -> Result<(), CliError> + Send + Sync;

impl TuiRuntimeRouter {
    pub fn new(
        bootstrap: Bootstrap,
        session: Arc<Mutex<SessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
    ) -> Self {
        Self::with_auth_coordinator(
            bootstrap,
            session,
            cancellation,
            commands,
            skills,
            ChatGptAuthCoordinator::production(),
        )
    }

    pub fn with_auth_coordinator(
        mut bootstrap: Bootstrap,
        session: Arc<Mutex<SessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
        auth: ChatGptAuthCoordinator,
    ) -> Self {
        let has_subagents = session.lock().is_ok_and(|context| {
            subagent_catalog(&bootstrap, &context).is_ok_and(|mut agents| agents.next().is_some())
        });
        let palette = resolved_tui_palette(&commands, &skills, has_subagents);
        let project_root = bootstrap.project_root.as_deref().unwrap_or(Path::new("."));

        // The handle has to exist before the first registry is built. Building the
        // registry first gave it a private handle, and every later registry — the tool
        // runtime's, which is the one that actually discovers — reported into a handle
        // nobody rendered, so discovery failures never reached the `/mcp` overlay.
        let mcp_status = McpStatusHandle::default();
        bootstrap.mcp_status = mcp_status.clone();

        let registry = load_configured_mcp_registry(&bootstrap, project_root);
        let mcp_runtime = Arc::new(Mutex::new(ProductionMcpRuntime {
            registry: Arc::new(Mutex::new(registry)),
            dispatcher: Arc::new(Mutex::new(ToolDispatcher::new())),
        }));
        Self {
            bootstrap: Arc::new(Mutex::new(bootstrap)),
            session,
            cancellation,
            auth,
            credentials: CredentialResolver::production(),
            extensions: Arc::new(Mutex::new(RouterExtensions {
                commands,
                skills,
                palette,
            })),
            mcp_status,
            mcp_runtime,
            clock: current_session_timestamp,
            credential_restorer: Arc::new(restore_chatgpt_credentials),
            profile_editor: Arc::new(Mutex::new(None)),
            profile_focus: Arc::new(Mutex::new(None)),
            profile_store: None,
        }
    }

    pub fn with_profile_store(mut self, store: Arc<dyn AgentProfileStore>) -> Self {
        self.profile_store = Some(store);
        self
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_credential_restorer(
        mut self,
        restore: impl Fn(&Path, ChatGptCredentialSnapshot) -> Result<(), CliError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.credential_restorer = Arc::new(restore);
        self
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_credential_resolver(
        bootstrap: Bootstrap,
        session: Arc<Mutex<SessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
        credentials: CredentialResolver,
    ) -> Self {
        let mut router = Self::new(bootstrap, session, cancellation, commands, skills);
        router.credentials = credentials;
        router
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_clock(
        bootstrap: Bootstrap,
        session: Arc<Mutex<SessionContext>>,
        cancellation: Arc<Mutex<Option<HeadlessTurnCancellation>>>,
        commands: Arc<CommandCatalog>,
        skills: Arc<SkillCatalog>,
        clock: fn() -> i64,
    ) -> Self {
        let mut router = Self::new(bootstrap, session, cancellation, commands, skills);
        router.clock = clock;
        router
    }
}

pub enum AuthRouteError {
    Auth(agens_auth::ChatGptAuthError),
    Runtime(CliError),
}

pub fn auth_route_outcome(result: Result<String, AuthRouteError>) -> TuiSubmissionOutcome {
    match result {
        Ok(message) => TuiSubmissionOutcome::LocalInfo(message),
        Err(AuthRouteError::Auth(error)) => TuiSubmissionOutcome::LocalActionableError {
            message: error.message().into(),
            action: error.action().into(),
        },
        Err(AuthRouteError::Runtime(error)) => TuiSubmissionOutcome::LocalActionableError {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        },
    }
}

fn has_error_message(error: &CliError, expected: &str) -> bool {
    if error.message == expected {
        return true;
    }
    let Some(reference) = error
        .message
        .strip_prefix(expected)
        .and_then(|suffix| suffix.strip_prefix(" [ref: "))
        .and_then(|suffix| suffix.strip_suffix(']'))
    else {
        return false;
    };
    reference.len() == 8
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn tui_provider_outcome(result: Result<String, CliError>) -> TuiProviderOutcome {
    match result {
        Ok(output) => TuiProviderOutcome::Completed(output),
        Err(error) if error.category == "cancelled" => TuiProviderOutcome::Cancelled {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        },
        Err(error) if has_error_message(&error, "request exceeds the model context window") => {
            TuiProviderOutcome::Failed {
                message: error.to_string(),
                action: "Start a new session or shorten the prompt, then retry.".into(),
            }
        }
        Err(error) if has_error_message(&error, "outgrew what one request can replay") => {
            TuiProviderOutcome::Failed {
                message: error.to_string(),
                action: "Start a new session; this one's history cannot be replayed further."
                    .into(),
            }
        }
        Err(error) if has_error_message(&error, "tool-continuation rounds") => {
            TuiProviderOutcome::Failed {
                message: error.to_string(),
                action: "Ask for a narrower step; the turn kept calling tools without finishing."
                    .into(),
            }
        }
        Err(error) if has_error_message(&error, "network request failed") => {
            TuiProviderOutcome::Failed {
                message: error.to_string(),
                action: "Check the network connection, then retry.".into(),
            }
        }
        Err(error) if has_error_message(&error, "ChatGPT response protocol failed") => {
            TuiProviderOutcome::Failed {
                message: error.to_string(),
                action: "Open /diagnostics for the referenced event, then retry.".into(),
            }
        }
        Err(error) => TuiProviderOutcome::Failed {
            message: error.to_string(),
            action: TUI_ERROR_ACTION.into(),
        },
    }
}

#[cfg(test)]
mod tests;

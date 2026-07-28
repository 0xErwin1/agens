//! Provider identity and credential state.
//!
//! None of this is a user interface: it is which provider a session speaks to
//! and whether its credentials are usable. It lived under `tui/` and carried a
//! `Tui` prefix, which is why the engine appeared to depend on the terminal.

//! Provider identity and ChatGPT credential-file bookkeeping for the TUI:
//! [`ProviderKind`] enumerates the supported API/subscription providers,
//! [`CredentialResolver`] resolves each provider's [`CredentialStatus`]
//! against the on-disk credentials file, and the snapshot/restore pair
//! backs ChatGPT login rollback on failure.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use agens_providers::chatgpt_login::{remove_provider_entry, upsert_provider_entry};
use agens_providers::{ChatGptAuthState, load_chatgpt_auth_state};

use agens_bootstrap::openai_api_key;
use agens_error::CliError;
use agens_models::ModelSource;

#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAiApi,
    OpenAiChatGpt,
}

impl ProviderKind {
    pub const ALL: [Self; 2] = [Self::OpenAiChatGpt, Self::OpenAiApi];

    pub const fn identifier(self) -> &'static str {
        ["openai-api", "openai-chatgpt"][self as usize]
    }

    pub const fn label(self) -> &'static str {
        ["OpenAI API", "ChatGPT subscription"][self as usize]
    }

    pub const fn source(self) -> ModelSource {
        [ModelSource::OpenAiApi, ModelSource::ChatGptSubscription][self as usize]
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.identifier() == value)
    }
}

#[repr(usize)]
#[derive(Clone, Copy)]
pub enum CredentialStatus {
    Ready,
    RefreshRequired,
    ConnectRequired,
    CredentialRequired,
}

impl CredentialStatus {
    pub const fn label(self) -> &'static str {
        [
            "ready",
            "refresh required",
            "connect required",
            "credential required",
        ][self as usize]
    }

    pub const fn available(self) -> bool {
        matches!(self, Self::Ready | Self::RefreshRequired)
    }
}

#[derive(Clone)]
pub struct CredentialResolver {
    pub environment: Arc<dyn Fn() -> BTreeMap<String, String> + Send + Sync>,
}

impl CredentialResolver {
    pub fn production() -> Self {
        Self {
            environment: Arc::new(|| std::env::vars().collect()),
        }
    }

    /// A resolver whose environment is fixed. Not test-gated: the tests that use
    /// it live in another crate now, and a constructor that only exists under
    /// `cfg(test)` cannot cross a crate boundary.
    pub fn with_environment(environment: BTreeMap<String, String>) -> Self {
        Self::with_environment_resolver(move || environment.clone())
    }

    pub fn with_environment_resolver(
        resolve: impl Fn() -> BTreeMap<String, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            environment: Arc::new(resolve),
        }
    }

    pub fn api_key(&self, path: &Path) -> Option<String> {
        let credentials = fs::read_to_string(path).ok();
        openai_api_key(credentials.as_deref(), &(self.environment)())
    }

    pub fn status(&self, path: &Path, provider: ProviderKind) -> CredentialStatus {
        match provider {
            ProviderKind::OpenAiChatGpt => {
                match load_chatgpt_auth_state(path, std::time::SystemTime::now()) {
                    Ok(ChatGptAuthState::Ready) => CredentialStatus::Ready,
                    Ok(ChatGptAuthState::RefreshRequired) => CredentialStatus::RefreshRequired,
                    Err(_) => CredentialStatus::ConnectRequired,
                }
            }
            ProviderKind::OpenAiApi => {
                if self.api_key(path).is_some() {
                    CredentialStatus::Ready
                } else {
                    CredentialStatus::CredentialRequired
                }
            }
        }
    }
}

pub enum ChatGptCredentialSnapshot {
    Absent,
    Present(serde_json::Value),
}

pub fn snapshot_chatgpt_credentials(path: &Path) -> Result<ChatGptCredentialSnapshot, CliError> {
    match fs::read_to_string(path) {
        Ok(credentials) => serde_json::from_str::<serde_json::Value>(&credentials)
            .ok()
            .and_then(|root| root.get("openai-chatgpt").cloned())
            .map_or(Ok(ChatGptCredentialSnapshot::Absent), |entry| {
                Ok(ChatGptCredentialSnapshot::Present(entry))
            }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound || path.is_dir() => {
            Ok(ChatGptCredentialSnapshot::Absent)
        }
        Err(_) => Err(CliError::storage("ChatGPT credentials could not be read")),
    }
}

pub fn restore_chatgpt_credentials(
    path: &Path,
    snapshot: ChatGptCredentialSnapshot,
) -> Result<(), CliError> {
    remove_provider_entry(path, "openai-chatgpt")
        .map_err(|_| CliError::storage("ChatGPT credential recovery failed"))?;

    if let ChatGptCredentialSnapshot::Present(entry) = snapshot {
        upsert_provider_entry(path, "openai-chatgpt", entry)
            .map_err(|_| CliError::storage("ChatGPT credential recovery failed"))?;
    }

    Ok(())
}

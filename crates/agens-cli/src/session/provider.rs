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

use crate::bootstrap::openai_api_key;
use agens_error::CliError;
use agens_models::ModelSource;

#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderKind {
    OpenAiApi,
    OpenAiChatGpt,
}

impl ProviderKind {
    pub(crate) const ALL: [Self; 2] = [Self::OpenAiChatGpt, Self::OpenAiApi];

    pub(crate) const fn identifier(self) -> &'static str {
        ["openai-api", "openai-chatgpt"][self as usize]
    }

    pub(crate) const fn label(self) -> &'static str {
        ["OpenAI API", "ChatGPT subscription"][self as usize]
    }

    pub(crate) const fn source(self) -> ModelSource {
        [ModelSource::OpenAiApi, ModelSource::ChatGptSubscription][self as usize]
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.identifier() == value)
    }
}

#[repr(usize)]
#[derive(Clone, Copy)]
pub(crate) enum CredentialStatus {
    Ready,
    RefreshRequired,
    ConnectRequired,
    CredentialRequired,
}

impl CredentialStatus {
    pub(crate) const fn label(self) -> &'static str {
        [
            "ready",
            "refresh required",
            "connect required",
            "credential required",
        ][self as usize]
    }

    pub(crate) const fn available(self) -> bool {
        matches!(self, Self::Ready | Self::RefreshRequired)
    }
}

#[derive(Clone)]
pub(crate) struct CredentialResolver {
    pub(crate) environment: Arc<dyn Fn() -> BTreeMap<String, String> + Send + Sync>,
}

impl CredentialResolver {
    pub(crate) fn production() -> Self {
        Self {
            environment: Arc::new(|| std::env::vars().collect()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_environment(environment: BTreeMap<String, String>) -> Self {
        Self::with_environment_resolver(move || environment.clone())
    }

    #[cfg(test)]
    pub(crate) fn with_environment_resolver(
        resolve: impl Fn() -> BTreeMap<String, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            environment: Arc::new(resolve),
        }
    }

    pub(crate) fn api_key(&self, path: &Path) -> Option<String> {
        let credentials = fs::read_to_string(path).ok();
        openai_api_key(credentials.as_deref(), &(self.environment)())
    }

    pub(crate) fn status(&self, path: &Path, provider: ProviderKind) -> CredentialStatus {
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

pub(crate) enum ChatGptCredentialSnapshot {
    Absent,
    Present(serde_json::Value),
}

pub(crate) fn snapshot_chatgpt_credentials(
    path: &Path,
) -> Result<ChatGptCredentialSnapshot, CliError> {
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

pub(crate) fn restore_chatgpt_credentials(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tui_session_directory;

    #[test]
    fn tui_provider_availability_uses_complete_current_credentials_without_exposing_them() {
        let temporary = tui_session_directory("provider-status");
        let credentials = temporary.join("auth.json");
        std::fs::write(
            &credentials,
            r#"{"openai-chatgpt":{"access_token":"access","refresh_token":"refresh","account_id":"account","expires_at":"2099-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let resolver = CredentialResolver::with_environment(BTreeMap::new());

        let statuses =
            ProviderKind::ALL.map(|provider| resolver.status(&credentials, provider).label());
        assert_eq!(statuses, ["ready", "credential required"]);
        std::fs::remove_dir_all(temporary).unwrap();
    }
}

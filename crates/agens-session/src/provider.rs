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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAiApi,
    OpenAiChatGpt,
    Moonshot,
}

impl ProviderKind {
    pub const ALL: [Self; 3] = [Self::OpenAiChatGpt, Self::OpenAiApi, Self::Moonshot];

    pub const fn identifier(self) -> &'static str {
        match self {
            Self::OpenAiApi => "openai-api",
            Self::OpenAiChatGpt => "openai-chatgpt",
            Self::Moonshot => "moonshotai",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenAiApi => "OpenAI API",
            Self::OpenAiChatGpt => "ChatGPT subscription",
            Self::Moonshot => "Moonshot AI",
        }
    }

    pub const fn source(self) -> ModelSource {
        match self {
            Self::OpenAiApi => ModelSource::OpenAiApi,
            Self::OpenAiChatGpt => ModelSource::ChatGptSubscription,
            Self::Moonshot => ModelSource::MoonshotApi,
        }
    }

    /// The model a run falls back to for this provider. Total where
    /// [`agens_models::default_model`] is partial: a `ProviderKind` has already
    /// been validated, so there is no unknown provider left to reject.
    pub const fn default_model(self) -> &'static str {
        match self {
            Self::OpenAiApi => "gpt-4.1",
            Self::OpenAiChatGpt => "gpt-5.5",
            Self::Moonshot => "kimi-k3",
        }
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

    pub fn moonshot_api_key(&self, path: &Path) -> Option<String> {
        let credentials = fs::read_to_string(path).ok();
        moonshot_api_key(credentials.as_deref(), &(self.environment)())
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
            ProviderKind::Moonshot => {
                if self.moonshot_api_key(path).is_some() {
                    CredentialStatus::Ready
                } else {
                    CredentialStatus::CredentialRequired
                }
            }
        }
    }
}

/// Resolves the Moonshot API key with the same env-over-stored precedence as
/// [`openai_api_key`]: the `MOONSHOT_API_KEY` environment variable wins over a
/// stored `moonshotai.api_key` credential entry.
fn moonshot_api_key(
    credentials: Option<&str>,
    environment: &BTreeMap<String, String>,
) -> Option<String> {
    environment
        .get("MOONSHOT_API_KEY")
        .filter(|key| !key.is_empty())
        .cloned()
        .or_else(|| {
            credentials
                .and_then(|contents| serde_json::from_str::<serde_json::Value>(contents).ok())
                .and_then(|credentials| {
                    credentials
                        .get("moonshotai")?
                        .get("api_key")?
                        .as_str()
                        .filter(|key| !key.is_empty())
                        .map(ToOwned::to_owned)
                })
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_three_provider_kinds_map_correctly() {
        assert_eq!(ProviderKind::ALL.len(), 3);
        assert!(ProviderKind::ALL.contains(&ProviderKind::Moonshot));

        assert_eq!(ProviderKind::Moonshot.identifier(), "moonshotai");
        assert_eq!(ProviderKind::Moonshot.label(), "Moonshot AI");
        assert_eq!(ProviderKind::Moonshot.source(), ModelSource::MoonshotApi);
        assert_eq!(
            ProviderKind::parse("moonshotai"),
            Some(ProviderKind::Moonshot)
        );
    }

    #[test]
    fn credential_status_reports_a_moonshot_row() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-provider-status-moonshot-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temporary).expect("fixture directory should be created");
        let credentials_path = temporary.join("credentials.json");

        let missing = CredentialResolver::with_environment(BTreeMap::new());
        assert!(matches!(
            missing.status(&credentials_path, ProviderKind::Moonshot),
            CredentialStatus::CredentialRequired
        ));

        let present = CredentialResolver::with_environment(BTreeMap::from([(
            "MOONSHOT_API_KEY".to_owned(),
            "sk-test-moonshot".to_owned(),
        )]));
        assert!(matches!(
            present.status(&credentials_path, ProviderKind::Moonshot),
            CredentialStatus::Ready
        ));

        std::fs::remove_dir_all(&temporary).ok();
    }

    #[test]
    fn typed_and_string_default_model_lookups_agree() {
        for provider in ProviderKind::ALL {
            assert_eq!(
                Some(provider.default_model()),
                agens_models::default_model(Some(provider.identifier())),
                "default model disagrees for {}",
                provider.identifier()
            );
        }
    }
}

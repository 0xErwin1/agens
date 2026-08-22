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

use agens_bootstrap::provider_api_key;
use agens_error::CliError;
use agens_models::{ModelSource, QualifiedModel};

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

    pub fn for_source(source: ModelSource) -> Self {
        match source {
            ModelSource::OpenAiApi => Self::OpenAiApi,
            ModelSource::ChatGptSubscription => Self::OpenAiChatGpt,
            ModelSource::MoonshotApi => Self::Moonshot,
        }
    }
}

/// Splits a `provider/model` identifier into the provider it names and the
/// bare identifier that provider's API accepts.
///
/// The parse is [`QualifiedModel`]'s, so what counts as a provider prefix is
/// answered once rather than re-implemented wherever a qualified identifier
/// arrives. `None` covers a bare identifier and one whose prefix names no
/// provider alike: both stay with the provider the session already resolved,
/// and the model resolution that follows is what reports an identifier nothing
/// serves.
pub fn split_qualified_model(value: &str) -> Option<(ProviderKind, String)> {
    let qualified = QualifiedModel::parse(value).ok()?;

    Some((
        ProviderKind::for_source(qualified.source()?),
        qualified.model().to_owned(),
    ))
}

/// A model identifier resolved to the provider that will serve it, and the
/// bare identifier that provider's API accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub provider: ProviderKind,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderResolutionError {
    /// Several authenticated providers serve this identifier, so nothing in it
    /// says where the request goes.
    Ambiguous {
        model: String,
        candidates: Vec<ProviderKind>,
    },
    /// No model was named and more than one provider could answer.
    AmbiguousDefault {
        candidates: Vec<ProviderKind>,
    },
    /// The provider that serves this model has no usable credentials.
    Unauthenticated(ProviderKind),
    UnknownModel(String),
    NoProvider,
}

impl ProviderResolutionError {
    pub fn message(&self) -> String {
        match self {
            Self::Ambiguous { model, candidates } => format!(
                "model \"{model}\" is served by {}; qualify it as \"{}/{model}\"",
                names(candidates),
                candidates
                    .first()
                    .map_or("provider", |provider| provider.identifier())
            ),
            Self::AmbiguousDefault { candidates } => format!(
                "no model is configured and {} are authenticated; name one as \"provider/model\"",
                names(candidates)
            ),
            Self::Unauthenticated(provider) => format!(
                "provider \"{}\" serves this model but has no usable credentials",
                provider.identifier()
            ),
            Self::UnknownModel(model) => {
                format!("model \"{model}\" is not served by any known provider")
            }
            Self::NoProvider => "no provider has usable credentials".to_owned(),
        }
    }
}

fn names(candidates: &[ProviderKind]) -> String {
    candidates
        .iter()
        .map(|provider| format!("\"{}\"", provider.identifier()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether this run can authenticate a given provider.
///
/// API-key providers are resolved through the bootstrap so an injected host
/// answers the same way it answered at configuration time; the ChatGPT
/// subscription is resolved from its credentials file, which is where its
/// provider reads it too.
pub fn bootstrap_authentication(
    bootstrap: &agens_bootstrap::Bootstrap,
) -> impl Fn(ProviderKind) -> bool + use<'_> {
    let credentials = bootstrap.paths.credentials.clone();
    let resolver = CredentialResolver::with_environment(bootstrap.credential_environment());

    move |provider| match provider {
        ProviderKind::OpenAiChatGpt => resolver.status(&credentials, provider).available(),
        ProviderKind::OpenAiApi | ProviderKind::Moonshot => {
            bootstrap.api_key_for(provider.identifier()).is_some()
        }
    }
}

/// Every provider this run can authenticate, as model sources.
pub fn authenticated_sources(authenticated: &dyn Fn(ProviderKind) -> bool) -> Vec<ModelSource> {
    ModelSource::ALL
        .into_iter()
        .filter(|source| authenticated(ProviderKind::for_source(*source)))
        .collect()
}

/// [`resolve_provider_for_model`] against an on-disk credentials file.
pub fn resolve_provider_for_model_with_credentials(
    model: Option<&str>,
    credentials: &Path,
    resolver: &CredentialResolver,
) -> Result<ResolvedProvider, ProviderResolutionError> {
    resolve_provider_for_model(model, &|provider| {
        resolver.status(credentials, provider).available()
    })
}

/// Resolves which provider serves a model, from the identifier alone.
///
/// A `provider/model` prefix is the whole answer. A bare identifier resolves
/// only when exactly one authenticated provider serves it: with two reachable
/// providers offering the same name, picking one would send the request, and
/// its spend, somewhere the user never named.
pub fn resolve_provider_for_model(
    model: Option<&str>,
    authenticated: &dyn Fn(ProviderKind) -> bool,
) -> Result<ResolvedProvider, ProviderResolutionError> {
    let Some(model) = model else {
        let candidates: Vec<ProviderKind> = ModelSource::ALL
            .into_iter()
            .map(ProviderKind::for_source)
            .filter(|provider| authenticated(*provider))
            .collect();

        return match candidates.as_slice() {
            [] => Err(ProviderResolutionError::NoProvider),
            [only] => Ok(ResolvedProvider {
                provider: *only,
                model: only.default_model().to_owned(),
            }),
            _ => Err(ProviderResolutionError::AmbiguousDefault { candidates }),
        };
    };

    let parsed = QualifiedModel::parse(model)
        .map_err(|_| ProviderResolutionError::UnknownModel(model.to_owned()))?;
    let serving: Vec<ProviderKind> = agens_models::sources_serving(parsed.model())
        .into_iter()
        .map(ProviderKind::for_source)
        .collect();

    // A prefix names the provider outright, so nothing has to be inferred from
    // the catalog. Requiring catalog membership here would make the bundled
    // snapshot authoritative over which models exist, and a model newer than
    // the snapshot, or served by a proxy, unusable.
    if let Some(named) = parsed.source().map(ProviderKind::for_source) {
        return authenticated(named)
            .then(|| ResolvedProvider {
                provider: named,
                model: parsed.model().to_owned(),
            })
            .ok_or(ProviderResolutionError::Unauthenticated(named));
    }

    let candidates: Vec<ProviderKind> = serving
        .iter()
        .copied()
        .filter(|provider| authenticated(*provider))
        .collect();

    match (candidates.as_slice(), serving.as_slice()) {
        ([only], _) => Ok(ResolvedProvider {
            provider: *only,
            model: parsed.model().to_owned(),
        }),
        ([], []) => Err(ProviderResolutionError::UnknownModel(
            parsed.model().to_owned(),
        )),
        ([], [unreachable, ..]) => Err(ProviderResolutionError::Unauthenticated(*unreachable)),
        _ => Err(ProviderResolutionError::Ambiguous {
            model: parsed.model().to_owned(),
            candidates,
        }),
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

    /// The stored or environment-provided API key for one provider. Delegates to
    /// [`provider_api_key`] so precedence stays defined in exactly one place.
    pub fn provider_api_key(&self, path: &Path, provider: ProviderKind) -> Option<String> {
        let credentials = fs::read_to_string(path).ok();
        provider_api_key(
            provider.identifier(),
            credentials.as_deref(),
            &(self.environment)(),
        )
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
            ProviderKind::OpenAiApi | ProviderKind::Moonshot => {
                if self.provider_api_key(path, provider).is_some() {
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

    /// Both routers used to split the prefix themselves, which is one parser
    /// per caller and one place each for the set of provider names to drift.
    #[test]
    fn a_qualified_identifier_splits_only_on_a_provider_this_build_serves() {
        for provider in ProviderKind::ALL {
            assert_eq!(
                split_qualified_model(&format!("{}/kimi-k3", provider.identifier())),
                Some((provider, "kimi-k3".to_owned()))
            );
        }

        assert_eq!(split_qualified_model("kimi-k3"), None);
        assert_eq!(split_qualified_model("not-a-provider/kimi-k3"), None);
        assert_eq!(split_qualified_model("openai-api/"), None);
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

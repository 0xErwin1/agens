use std::collections::BTreeMap;
use std::path::PathBuf;

use agens_session::provider::{
    CredentialResolver, ProviderKind, ProviderResolutionError,
    resolve_provider_for_model_with_credentials,
};

struct Credentials {
    directory: PathBuf,
}

impl Credentials {
    fn new(label: &str, contents: &str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("agens-resolve-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("credential fixture directory");
        std::fs::write(directory.join("auth.json"), contents).expect("credential fixture");
        Self { directory }
    }

    fn path(&self) -> PathBuf {
        self.directory.join("auth.json")
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

const CHATGPT_ENTRY: &str = r#""openai-chatgpt": {"access_token": "a", "refresh_token": "r", "account_id": "i", "expires_at": "2099-01-01T00:00:00Z"}"#;
const MOONSHOT_ENTRY: &str = r#""moonshotai": {"api_key": "k"}"#;
const OPENAI_ENTRY: &str = r#""openai-api": {"api_key": "k"}"#;

fn resolver() -> CredentialResolver {
    CredentialResolver::with_environment(BTreeMap::new())
}

fn resolve(
    label: &str,
    entries: &[&str],
    model: Option<&str>,
) -> Result<(ProviderKind, String), ProviderResolutionError> {
    let credentials = Credentials::new(label, &format!("{{{}}}", entries.join(", ")));

    resolve_provider_for_model_with_credentials(model, &credentials.path(), &resolver())
        .map(|resolved| (resolved.provider, resolved.model))
}

/// The prefix is the whole answer: no other authenticated provider, and no
/// global setting, gets a say.
#[test]
fn a_qualified_model_selects_the_provider_it_names() {
    assert_eq!(
        resolve(
            "qualified",
            &[CHATGPT_ENTRY, MOONSHOT_ENTRY],
            Some("moonshotai/kimi-k3")
        ),
        Ok((ProviderKind::Moonshot, "kimi-k3".to_owned()))
    );
    assert_eq!(
        resolve(
            "qualified-chatgpt",
            &[CHATGPT_ENTRY, MOONSHOT_ENTRY],
            Some("openai-chatgpt/gpt-5.5")
        ),
        Ok((ProviderKind::OpenAiChatGpt, "gpt-5.5".to_owned()))
    );
}

#[test]
fn a_bare_model_only_one_authenticated_provider_serves_needs_no_prefix() {
    assert_eq!(
        resolve("bare-single", &[MOONSHOT_ENTRY], Some("kimi-k3")),
        Ok((ProviderKind::Moonshot, "kimi-k3".to_owned()))
    );
}

/// Both OpenAI dialects serve `gpt-5.5` and both are authenticated, so nothing
/// in the identifier says where the request goes. Guessing here spends money
/// against a provider the user did not choose.
#[test]
fn a_bare_model_two_authenticated_providers_serve_is_refused_with_its_candidates() {
    let error = resolve(
        "bare-ambiguous",
        &[CHATGPT_ENTRY, OPENAI_ENTRY],
        Some("gpt-5.5"),
    )
    .expect_err("both dialects serve it");

    assert_eq!(
        error,
        ProviderResolutionError::Ambiguous {
            model: "gpt-5.5".to_owned(),
            candidates: vec![ProviderKind::OpenAiApi, ProviderKind::OpenAiChatGpt],
        }
    );
}

/// The same identifier stops being ambiguous once only one of the two
/// providers can actually be reached.
#[test]
fn an_otherwise_ambiguous_model_resolves_when_only_one_provider_is_authenticated() {
    assert_eq!(
        resolve("bare-one-credential", &[OPENAI_ENTRY], Some("gpt-5.5")),
        Ok((ProviderKind::OpenAiApi, "gpt-5.5".to_owned()))
    );
}

#[test]
fn a_model_whose_provider_has_no_credentials_names_that_provider() {
    assert_eq!(
        resolve(
            "unauthenticated",
            &[CHATGPT_ENTRY],
            Some("moonshotai/kimi-k3")
        ),
        Err(ProviderResolutionError::Unauthenticated(
            ProviderKind::Moonshot
        ))
    );
    assert_eq!(
        resolve("unauthenticated-bare", &[CHATGPT_ENTRY], Some("kimi-k3")),
        Err(ProviderResolutionError::Unauthenticated(
            ProviderKind::Moonshot
        ))
    );
}

#[test]
fn a_model_no_provider_serves_is_reported_as_unknown() {
    assert_eq!(
        resolve("unknown", &[MOONSHOT_ENTRY], Some("no-such-model")),
        Err(ProviderResolutionError::UnknownModel(
            "no-such-model".to_owned()
        ))
    );
}

/// The prefix names the provider, so a model the bundled snapshot has never
/// heard of is still routable: the provider itself rejects it if it is wrong.
#[test]
fn a_qualified_model_outside_the_bundled_catalog_still_resolves() {
    assert_eq!(
        resolve(
            "unknown-qualified",
            &[MOONSHOT_ENTRY],
            Some("moonshotai/nope")
        ),
        Ok((ProviderKind::Moonshot, "nope".to_owned()))
    );
}

/// With no model configured the single authenticated provider still answers,
/// through its own default.
#[test]
fn an_absent_model_falls_back_to_the_only_authenticated_provider() {
    assert_eq!(
        resolve("absent-single", &[MOONSHOT_ENTRY], None),
        Ok((ProviderKind::Moonshot, "kimi-k3".to_owned()))
    );
}

#[test]
fn an_absent_model_with_several_providers_asks_for_one() {
    let error = resolve("absent-many", &[CHATGPT_ENTRY, MOONSHOT_ENTRY], None)
        .expect_err("two providers, no model");

    assert_eq!(
        error,
        ProviderResolutionError::AmbiguousDefault {
            candidates: vec![ProviderKind::OpenAiChatGpt, ProviderKind::Moonshot],
        }
    );
}

#[test]
fn no_credentials_at_all_is_its_own_failure() {
    assert_eq!(
        resolve("none", &[], None),
        Err(ProviderResolutionError::NoProvider)
    );
}

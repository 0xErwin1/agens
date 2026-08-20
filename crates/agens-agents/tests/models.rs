use agens_agents::AgentModelCompatibility;
use agens_models::ModelSource;
use agens_tools::{AgentModelValidationError, AgentModelValidator};

fn validator(authenticated: &[ModelSource]) -> AgentModelCompatibility {
    AgentModelCompatibility::for_authenticated(authenticated.to_vec())
        .expect("the bundled catalog is available")
}

/// The point of the qualified form: one catalog may mix providers, and each
/// agent's identifier says which one it means.
#[test]
fn agents_on_different_providers_both_validate_in_the_same_run() {
    let both = validator(&[ModelSource::MoonshotApi, ModelSource::ChatGptSubscription]);

    assert_eq!(both.validate_model("moonshotai/kimi-k3"), Ok(()));
    assert_eq!(both.validate_model("openai-chatgpt/gpt-5.5"), Ok(()));
}

#[test]
fn a_bare_identifier_only_one_authenticated_provider_serves_is_accepted() {
    let both = validator(&[ModelSource::MoonshotApi, ModelSource::ChatGptSubscription]);

    assert_eq!(both.validate_model("kimi-k3"), Ok(()));
}

/// `gpt-5.5` exists under both OpenAI dialects, so a bare identifier does not
/// say which one an agent meant.
#[test]
fn a_bare_identifier_two_authenticated_providers_serve_is_refused_with_its_candidates() {
    let error = validator(&[ModelSource::OpenAiApi, ModelSource::ChatGptSubscription])
        .validate_model("gpt-5.5")
        .expect_err("both dialects serve it");

    assert_eq!(
        error,
        AgentModelValidationError::Ambiguous {
            candidates: "\"openai-api\", \"openai-chatgpt\"".to_owned(),
        }
    );

    let message = error.message("gpt-5.5");

    assert!(message.contains("openai-api"), "{message}");
    assert!(message.contains("openai-chatgpt"), "{message}");
}

#[test]
fn a_model_whose_provider_is_not_authenticated_names_that_provider() {
    let error = validator(&[ModelSource::ChatGptSubscription])
        .validate_model("moonshotai/kimi-k3")
        .expect_err("Moonshot has no credentials here");

    assert_eq!(
        error,
        AgentModelValidationError::Unreachable {
            provider: "moonshotai",
        }
    );
    assert!(
        error.message("moonshotai/kimi-k3").contains("credentials"),
        "the message points at authentication"
    );
}

#[test]
fn a_model_no_provider_serves_stays_plainly_unavailable() {
    let moonshot = validator(&[ModelSource::MoonshotApi]);

    assert_eq!(
        moonshot.validate_model("no-such-model"),
        Err(AgentModelValidationError::Unavailable)
    );
    assert_eq!(
        moonshot.validate_model("openai/gpt-5.5"),
        Err(AgentModelValidationError::Unavailable)
    );
}

/// The prefix names the provider, so an identifier the bundled snapshot does
/// not list is still accepted: the provider itself rejects it if it is wrong.
#[test]
fn a_qualified_identifier_outside_the_bundled_catalog_is_accepted() {
    assert_eq!(
        validator(&[ModelSource::MoonshotApi]).validate_model("moonshotai/kimi-k9"),
        Ok(())
    );
}

use agens_agents::AgentModelCompatibility;
use agens_models::ModelSource;
use agens_tools::{AgentModelValidationError, AgentModelValidator};

fn validator(source: ModelSource) -> AgentModelCompatibility {
    AgentModelCompatibility::for_source(source).expect("the bundled catalog is available")
}

#[test]
fn a_model_the_session_provider_serves_is_accepted_bare_or_qualified() {
    let moonshot = validator(ModelSource::MoonshotApi);

    assert_eq!(moonshot.validate_model("kimi-k3"), Ok(()));
    assert_eq!(moonshot.validate_model("moonshotai/kimi-k3"), Ok(()));
}

/// The case this diagnostic exists for: the model is real and its credentials
/// may well be present, but the session resolved another provider.
#[test]
fn a_model_served_only_by_another_provider_names_both_providers() {
    let error = validator(ModelSource::ChatGptSubscription)
        .validate_model("kimi-k3")
        .expect_err("the ChatGPT catalog has no Moonshot model");

    assert_eq!(
        error,
        AgentModelValidationError::ProviderMismatch {
            requested: "moonshotai",
            active: "openai-chatgpt",
        }
    );

    let message = error.message("kimi-k3");

    assert!(message.contains("kimi-k3"), "{message}");
    assert!(message.contains("moonshotai"), "{message}");
    assert!(message.contains("openai-chatgpt"), "{message}");
}

#[test]
fn an_explicit_provider_prefix_is_rejected_against_a_different_session_provider() {
    let error = validator(ModelSource::MoonshotApi)
        .validate_model("openai-chatgpt/gpt-5.5")
        .expect_err("the session speaks to Moonshot");

    assert_eq!(
        error,
        AgentModelValidationError::ProviderMismatch {
            requested: "openai-chatgpt",
            active: "moonshotai",
        }
    );
}

/// Both OpenAI dialects serve `gpt-5.5`, so only the prefix distinguishes them.
#[test]
fn an_overlapping_identifier_is_resolved_by_its_prefix_not_by_the_session() {
    let api = validator(ModelSource::OpenAiApi);

    assert_eq!(api.validate_model("openai-api/gpt-5.5"), Ok(()));
    assert_eq!(
        api.validate_model("openai-chatgpt/gpt-5.5"),
        Err(AgentModelValidationError::ProviderMismatch {
            requested: "openai-chatgpt",
            active: "openai-api",
        })
    );
}

#[test]
fn a_model_no_provider_serves_stays_plainly_unavailable() {
    let chatgpt = validator(ModelSource::ChatGptSubscription);

    assert_eq!(
        chatgpt.validate_model("no-such-model"),
        Err(AgentModelValidationError::Unavailable)
    );
    assert_eq!(
        chatgpt.validate_model("moonshotai/no-such-model"),
        Err(AgentModelValidationError::Unavailable)
    );
}

#[test]
fn an_unrecognized_provider_prefix_is_unavailable_rather_than_a_mismatch() {
    assert_eq!(
        validator(ModelSource::MoonshotApi).validate_model("openai/gpt-5.5"),
        Err(AgentModelValidationError::Unavailable)
    );
}

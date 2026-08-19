use agens_core::{AgentDefinition, AgentMode};
use agens_session::context::{ActiveAgentRuntime, AgentRotationError};
use agens_tools::{AgentModelValidationError, AgentModelValidator, ToolDispatcher};

struct AcceptEveryModel;

impl AgentModelValidator for AcceptEveryModel {
    fn validate_model(&self, _: &str) -> Result<(), AgentModelValidationError> {
        Ok(())
    }
}

struct RejectEveryModel;

impl AgentModelValidator for RejectEveryModel {
    fn validate_model(&self, _: &str) -> Result<(), AgentModelValidationError> {
        Err(AgentModelValidationError::ProviderMismatch {
            requested: "moonshotai",
            active: "openai-chatgpt",
        })
    }
}

fn agent(model: Option<&str>) -> AgentDefinition {
    AgentDefinition {
        name: "primary".into(),
        description: "primary".into(),
        mode: AgentMode::Primary,
        model: model.map(ToOwned::to_owned),
        model_source: None,
        reasoning_effort: None,
        system_prompt: "Work.".into(),
        permission_rules: Vec::new(),
        skills: Vec::new(),
    }
}

fn build(
    definition: &AgentDefinition,
    validator: &dyn AgentModelValidator,
) -> Result<ActiveAgentRuntime, AgentRotationError> {
    ActiveAgentRuntime::build(
        definition,
        Some("gpt-5.5"),
        "project",
        &ToolDispatcher::new(),
        validator,
    )
}

/// The provider prefix selects a provider; it is not part of the identifier the
/// provider's API is asked for.
#[test]
fn a_qualified_agent_model_reaches_the_runtime_bare() {
    let runtime =
        build(&agent(Some("moonshotai/kimi-k3")), &AcceptEveryModel).expect("the model validates");

    assert_eq!(runtime.model.as_deref(), Some("kimi-k3"));
}

#[test]
fn an_unqualified_agent_model_and_an_inherited_one_are_unchanged() {
    assert_eq!(
        build(&agent(Some("kimi-k3")), &AcceptEveryModel)
            .expect("the model validates")
            .model
            .as_deref(),
        Some("kimi-k3")
    );
    assert_eq!(
        build(&agent(None), &AcceptEveryModel)
            .expect("an absent model inherits")
            .model
            .as_deref(),
        Some("gpt-5.5")
    );
}

#[test]
fn a_rejected_model_carries_its_identifier_and_verdict() {
    let error = build(&agent(Some("moonshotai/kimi-k3")), &RejectEveryModel)
        .expect_err("the validator rejects every model");

    assert_eq!(
        error,
        AgentRotationError::ModelUnavailable {
            model: "moonshotai/kimi-k3".into(),
            error: AgentModelValidationError::ProviderMismatch {
                requested: "moonshotai",
                active: "openai-chatgpt",
            },
        }
    );
}

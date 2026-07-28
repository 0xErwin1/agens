//! Which model and provider a session is effectively speaking through.
//!
//! Derived from the session and the resolved configuration, with no terminal in
//! the answer: a headless turn and a rendered header ask the same question.

use agens_bootstrap::Bootstrap;
use agens_models::{ModelSelection, ModelSource, default_model};

use crate::context::SessionContext;
use crate::provider::ProviderKind;

pub fn current_provider(bootstrap: &Bootstrap, context: &SessionContext) -> Option<ProviderKind> {
    if context.chatgpt_unavailable {
        return None;
    }
    if context.resume_error.is_some()
        && context
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.provider_id.is_some())
        && context.provider.is_none()
    {
        return None;
    }
    context
        .provider
        .or_else(|| bootstrap.provider_type().and_then(ProviderKind::parse))
}

pub fn effective_model(bootstrap: &Bootstrap, context: &SessionContext) -> String {
    context
        .selection
        .as_ref()
        .map(ModelSelection::model)
        .or_else(|| {
            context
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.model_id.as_deref())
        })
        .or_else(|| bootstrap.model())
        .unwrap_or_else(|| default_model(bootstrap.provider_type()))
        .to_owned()
}

pub fn model_source(bootstrap: &Bootstrap, context: &SessionContext) -> ModelSource {
    current_provider(bootstrap, context)
        .unwrap_or(ProviderKind::OpenAiApi)
        .source()
}

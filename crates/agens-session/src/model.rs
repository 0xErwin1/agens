//! Which model and provider a session is effectively speaking through.
//!
//! Derived from the session and the resolved configuration, with no terminal in
//! the answer: a headless turn and a rendered header ask the same question.

use agens_bootstrap::Bootstrap;
use agens_models::{ModelSelection, ModelSource, QualifiedModel};

use crate::context::SessionContext;
use crate::provider::{ProviderKind, bootstrap_authentication, resolve_provider_for_model};

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
    context.provider.or_else(|| configured_provider(bootstrap))
}

/// The provider the run's own configured model resolves to, for a session that
/// has not named one of its own yet.
///
/// Nothing declares a provider any more, so this is the only thing left that can
/// say which one a fresh session starts on.
pub fn configured_provider(bootstrap: &Bootstrap) -> Option<ProviderKind> {
    resolve_provider_for_model(bootstrap.model(), &bootstrap_authentication(bootstrap))
        .ok()
        .map(|resolved| resolved.provider)
}

/// The provider a session falls back to once [`current_provider`] declines to
/// name one. Shared so the effective model and the effective source can never
/// disagree about which provider they are describing.
pub fn resolved_provider(bootstrap: &Bootstrap, context: &SessionContext) -> ProviderKind {
    current_provider(bootstrap, context).unwrap_or(ProviderKind::OpenAiApi)
}

/// The bare model identifier this session speaks through.
///
/// Bare, not qualified: the provider is answered by [`current_provider`], and
/// everything downstream — the provider's own API, a catalog lookup, a rendered
/// header — wants the identifier without a prefix in front of it.
pub fn effective_model(bootstrap: &Bootstrap, context: &SessionContext) -> String {
    let configured = bootstrap
        .model()
        .and_then(|model| QualifiedModel::parse(model).ok());

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
        .or_else(|| configured.as_ref().map(QualifiedModel::model))
        .unwrap_or_else(|| resolved_provider(bootstrap, context).default_model())
        .to_owned()
}

pub fn model_source(bootstrap: &Bootstrap, context: &SessionContext) -> ModelSource {
    resolved_provider(bootstrap, context).source()
}

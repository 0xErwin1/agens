use agens_agents::{AgentProfileResolver, ProfileOrigin};
use agens_bootstrap::session_config::ScopedAgentProfiles;
use agens_config::AgentProfile;
use agens_core::ReasoningEffort;

fn profiles(global: Option<AgentProfile>, project: Option<AgentProfile>) -> ScopedAgentProfiles {
    ScopedAgentProfiles::new(
        global
            .into_iter()
            .map(|profile| ("research".to_owned(), profile))
            .collect(),
        project
            .into_iter()
            .map(|profile| ("research".to_owned(), profile))
            .collect(),
    )
}

#[test]
fn project_and_global_profiles_win_independently_per_field() {
    let profiles = profiles(
        Some(AgentProfile {
            model: Some("global-model".to_owned()),
            effort: Some("low".to_owned()),
        }),
        Some(AgentProfile {
            model: None,
            effort: Some("high".to_owned()),
        }),
    );

    let resolved = AgentProfileResolver::new(&profiles).resolve(
        "research",
        Some("frontmatter-model"),
        Some(ReasoningEffort::Minimal),
        "session-model",
        Some(ReasoningEffort::Medium),
    );

    assert_eq!(resolved.model.value, "global-model");
    assert_eq!(resolved.model.origin, ProfileOrigin::GlobalProfile);
    assert_eq!(resolved.effort.value, Some(ReasoningEffort::High));
    assert_eq!(resolved.effort.origin, ProfileOrigin::ProjectProfile);
}

#[test]
fn pinned_model_without_explicit_effort_uses_the_model_default() {
    let profiles = profiles(
        Some(AgentProfile {
            model: Some("pinned-model".to_owned()),
            effort: None,
        }),
        None,
    );

    let resolved = AgentProfileResolver::new(&profiles).resolve(
        "research",
        None,
        None,
        "session-model",
        Some(ReasoningEffort::High),
    );

    assert_eq!(resolved.model.value, "pinned-model");
    assert_eq!(resolved.effort.value, None);
    assert_eq!(resolved.effort.origin, ProfileOrigin::SessionInherited);
}

#[test]
fn frontmatter_then_session_values_are_used_without_profiles() {
    let resolved = AgentProfileResolver::new(&ScopedAgentProfiles::default()).resolve(
        "research",
        Some("frontmatter-model"),
        Some(ReasoningEffort::Low),
        "session-model",
        Some(ReasoningEffort::Medium),
    );

    assert_eq!(resolved.model.value, "frontmatter-model");
    assert_eq!(resolved.model.origin, ProfileOrigin::Frontmatter);
    assert_eq!(resolved.effort.value, Some(ReasoningEffort::Low));
    assert_eq!(resolved.effort.origin, ProfileOrigin::Frontmatter);

    let inherited = AgentProfileResolver::new(&ScopedAgentProfiles::default()).resolve(
        "research",
        None,
        None,
        "session-model",
        Some(ReasoningEffort::Medium),
    );
    assert_eq!(inherited.model.value, "session-model");
    assert_eq!(inherited.model.origin, ProfileOrigin::SessionInherited);
    assert_eq!(inherited.effort.value, Some(ReasoningEffort::Medium));
    assert_eq!(inherited.effort.origin, ProfileOrigin::SessionInherited);
}

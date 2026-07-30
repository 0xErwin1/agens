use agens_bootstrap::session_config::ScopedAgentProfiles;
use agens_core::{ReasoningEffort, RequestConfig};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileOrigin {
    ProjectProfile,
    GlobalProfile,
    Frontmatter,
    SessionInherited,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedProfileValue<T> {
    pub value: T,
    pub origin: ProfileOrigin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAgentProfile {
    pub model: ResolvedProfileValue<String>,
    pub effort: ResolvedProfileValue<Option<ReasoningEffort>>,
}

pub struct AgentProfileResolver<'a> {
    profiles: &'a ScopedAgentProfiles,
}

impl<'a> AgentProfileResolver<'a> {
    pub const fn new(profiles: &'a ScopedAgentProfiles) -> Self {
        Self { profiles }
    }

    pub fn resolve(
        &self,
        agent_name: &str,
        frontmatter_model: Option<&str>,
        frontmatter_effort: Option<ReasoningEffort>,
        session_model: &str,
        session_effort: Option<ReasoningEffort>,
    ) -> ResolvedAgentProfile {
        let model = resolve_value(
            self.profiles
                .project()
                .get(agent_name)
                .and_then(|profile| profile.model.clone()),
            self.profiles
                .global()
                .get(agent_name)
                .and_then(|profile| profile.model.clone()),
            frontmatter_model.map(str::to_owned),
            session_model.to_owned(),
        );
        let session_effort = (model.value == session_model)
            .then_some(session_effort)
            .flatten();

        ResolvedAgentProfile {
            effort: resolve_optional_value(
                self.profiles
                    .project()
                    .get(agent_name)
                    .and_then(|profile| profile.effort.as_deref())
                    .and_then(parse_effort),
                self.profiles
                    .global()
                    .get(agent_name)
                    .and_then(|profile| profile.effort.as_deref())
                    .and_then(parse_effort),
                frontmatter_effort,
                session_effort,
            ),
            model,
        }
    }
}

fn resolve_value<T: Clone>(
    project: Option<T>,
    global: Option<T>,
    frontmatter: Option<T>,
    session: T,
) -> ResolvedProfileValue<T> {
    match project {
        Some(value) => ResolvedProfileValue {
            value,
            origin: ProfileOrigin::ProjectProfile,
        },
        None => match global {
            Some(value) => ResolvedProfileValue {
                value,
                origin: ProfileOrigin::GlobalProfile,
            },
            None => match frontmatter {
                Some(value) => ResolvedProfileValue {
                    value,
                    origin: ProfileOrigin::Frontmatter,
                },
                None => ResolvedProfileValue {
                    value: session,
                    origin: ProfileOrigin::SessionInherited,
                },
            },
        },
    }
}

fn resolve_optional_value<T>(
    project: Option<T>,
    global: Option<T>,
    frontmatter: Option<T>,
    session: Option<T>,
) -> ResolvedProfileValue<Option<T>> {
    match project {
        Some(value) => ResolvedProfileValue {
            value: Some(value),
            origin: ProfileOrigin::ProjectProfile,
        },
        None => match global {
            Some(value) => ResolvedProfileValue {
                value: Some(value),
                origin: ProfileOrigin::GlobalProfile,
            },
            None => match frontmatter {
                Some(value) => ResolvedProfileValue {
                    value: Some(value),
                    origin: ProfileOrigin::Frontmatter,
                },
                None => ResolvedProfileValue {
                    value: session,
                    origin: ProfileOrigin::SessionInherited,
                },
            },
        },
    }
}

fn parse_effort(value: &str) -> Option<ReasoningEffort> {
    RequestConfig::with_reasoning_effort(value)
        .ok()
        .and_then(|config| config.reasoning_effort())
}

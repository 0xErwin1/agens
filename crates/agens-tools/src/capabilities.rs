use std::collections::{BTreeMap, BTreeSet};

use agens_core::{
    AgentDefinition, PermissionDecision, PermissionPattern, PermissionRule, PermissionScope,
    PermissionTargetKind, permission_target_kind_for_tool,
};

use crate::ToolDispatcher;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveCapabilitySet {
    descriptors: Vec<EffectiveCapabilityDescriptor>,
}

impl EffectiveCapabilitySet {
    pub fn from_agent(agent: &AgentDefinition, project: &str, dispatcher: &ToolDispatcher) -> Self {
        let snapshot = dispatcher.capability_snapshot();
        let mut normalized = BTreeMap::new();

        for rule in &agent.permission_rules {
            if !rule_applies_to_project(rule, project) {
                continue;
            }
            let Some(selector) = selector(&rule.tool, &snapshot) else {
                continue;
            };
            let target = target(&rule.target);
            let kind = target_kind(&rule.tool);
            normalized.insert((selector, target), (rule.decision, kind));
        }

        let mut descriptors = normalized
            .into_iter()
            .map(
                |((selector, target), (decision, kind))| EffectiveCapabilityDescriptor {
                    selector,
                    target,
                    decision,
                    kind,
                },
            )
            .collect::<Vec<_>>();
        descriptors.sort_by_key(EffectiveCapabilityDescriptor::key);
        descriptors.dedup();
        Self { descriptors }
    }

    pub fn descriptors(&self) -> &[EffectiveCapabilityDescriptor] {
        &self.descriptors
    }

    pub fn permission_rules(&self) -> Vec<PermissionRule> {
        self.descriptors
            .iter()
            .map(EffectiveCapabilityDescriptor::permission_rule)
            .collect()
    }

    pub fn is_expansion_from(&self, prior: &Self) -> bool {
        let prior_decisions = prior.decisions();
        let candidate_decisions = self.decisions();

        self.descriptors.iter().any(|descriptor| {
            descriptor.decision == PermissionDecision::Allow
                && prior_decisions.get(&descriptor.key()) != Some(&PermissionDecision::Allow)
        }) || prior.descriptors.iter().any(|descriptor| {
            descriptor.decision == PermissionDecision::Deny
                && candidate_decisions.get(&descriptor.key()) != Some(&PermissionDecision::Deny)
        })
    }

    fn decisions(&self) -> BTreeMap<DescriptorKey, PermissionDecision> {
        self.descriptors
            .iter()
            .map(|descriptor| (descriptor.key(), descriptor.decision))
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveCapabilityDescriptor {
    selector: ToolSelector,
    target: Option<String>,
    decision: PermissionDecision,
    kind: PermissionTargetKind,
}

impl EffectiveCapabilityDescriptor {
    pub fn decision(&self) -> PermissionDecision {
        self.decision
    }

    fn key(&self) -> DescriptorKey {
        (self.selector.clone(), self.target.clone())
    }

    pub fn matches_identity(&self, identity: &str) -> bool {
        match &self.selector {
            ToolSelector::Exact(candidate) => candidate == identity,
            ToolSelector::Pattern { identities, .. } => {
                identities.iter().any(|candidate| candidate == identity)
            }
        }
    }

    fn permission_rule(&self) -> PermissionRule {
        PermissionRule::global(
            self.decision,
            self.selector.permission_pattern(),
            self.target
                .as_ref()
                .map(|pattern| {
                    PermissionPattern::glob_for_target_kind(pattern.clone(), self.kind)
                        .expect("stored target is validated")
                })
                .unwrap_or(PermissionPattern::Any),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ToolSelector {
    Exact(String),
    Pattern {
        source: String,
        identities: Vec<String>,
    },
}

impl ToolSelector {
    fn permission_pattern(&self) -> PermissionPattern {
        match self {
            Self::Exact(identity) => PermissionPattern::Exact(identity.clone()),
            Self::Pattern { source, .. } => {
                PermissionPattern::glob(source.clone()).expect("stored selector is validated")
            }
        }
    }
}

type DescriptorKey = (ToolSelector, Option<String>);

/// Classifies a declared rule's target kind from its `tool` pattern before
/// that pattern is resolved into the dispatcher's internal identity space
/// (`ToolSelector`'s `native:<len>:<name>` form), which no longer carries a
/// recognizable tool name. An unresolvable pattern (`Any`, or a glob whose
/// source is not a plain tool name) defaults to `Path`, the conservative
/// choice that keeps segment discipline.
fn target_kind(pattern: &PermissionPattern) -> PermissionTargetKind {
    let source = match pattern {
        PermissionPattern::Exact(value) => Some(value.as_str()),
        PermissionPattern::Glob(_) => pattern.glob_source(),
        PermissionPattern::Any => None,
    };

    source
        .map(permission_target_kind_for_tool)
        .unwrap_or(PermissionTargetKind::Path)
}

fn rule_applies_to_project(rule: &PermissionRule, project: &str) -> bool {
    rule.scope == PermissionScope::Global || rule.project.as_deref() == Some(project)
}

fn selector(pattern: &PermissionPattern, snapshot: &CapabilitySnapshot) -> Option<ToolSelector> {
    match pattern {
        PermissionPattern::Exact(value) => exact_selector(value, snapshot),
        PermissionPattern::Glob(_) if pattern.glob_source().is_some_and(is_literal_glob) => {
            exact_selector(pattern.glob_source().unwrap(), snapshot)
        }
        PermissionPattern::Any | PermissionPattern::Glob(_) => {
            let source = pattern.glob_source().unwrap_or("*").to_owned();
            let identities = snapshot
                .identities
                .iter()
                .filter(|identity| pattern.matches(identity))
                .cloned()
                .collect::<Vec<_>>();
            (!identities.is_empty()).then_some(ToolSelector::Pattern { source, identities })
        }
    }
}

fn exact_selector(value: &str, snapshot: &CapabilitySnapshot) -> Option<ToolSelector> {
    let identity = snapshot
        .aliases
        .get(value)
        .cloned()
        .unwrap_or_else(|| value.into());
    snapshot
        .identities
        .contains(&identity)
        .then_some(ToolSelector::Exact(identity))
}

fn is_literal_glob(pattern: &str) -> bool {
    !pattern.contains(['*', '?', '[', ']', '{', '}'])
}

fn target(pattern: &PermissionPattern) -> Option<String> {
    match pattern {
        PermissionPattern::Any => None,
        PermissionPattern::Exact(value) => Some(value.clone()),
        PermissionPattern::Glob(_) => pattern.glob_source().map(ToOwned::to_owned),
    }
}

pub(crate) struct CapabilitySnapshot {
    pub(crate) identities: BTreeSet<String>,
    pub(crate) aliases: BTreeMap<String, String>,
}

use std::collections::{BTreeMap, BTreeSet};

use agens_core::{
    AgentDefinition, PermissionDecision, PermissionPattern, PermissionRule, PermissionScope,
    PermissionTargetKind, permission_target_kind_for_tool, prevailing_decision,
};

use crate::ToolDispatcher;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveCapabilitySet {
    descriptors: Vec<EffectiveCapabilityDescriptor>,
}

impl EffectiveCapabilitySet {
    /// Resolves an agent's declarations against the tools a dispatcher
    /// actually holds, in declaration order.
    ///
    /// Declaration order is preserved only so the descriptors read back the way
    /// they were written, for the surfaces that display them. It decides
    /// nothing: [`agens_core::prevailing_rule_decision`] reads precedence off
    /// what each rule selects, so the same definition answers the same whatever
    /// order it was authored in.
    ///
    /// Two rules selecting exactly the same calls collapse into one descriptor,
    /// carrying the decision [`agens_core::prevailing_decision`] gives them —
    /// the same answer precedence would have reached had both survived, so
    /// collapsing them cannot change what the definition means.
    pub fn from_agent(agent: &AgentDefinition, project: &str, dispatcher: &ToolDispatcher) -> Self {
        let snapshot = dispatcher.capability_snapshot();
        let mut descriptors: Vec<EffectiveCapabilityDescriptor> = Vec::new();

        for rule in &agent.permission_rules {
            if !rule_applies_to_project(rule, project) {
                continue;
            }
            let target = target(&rule.target);
            for (selector, kind) in resolved_selectors(rule, &snapshot) {
                let mut descriptor = EffectiveCapabilityDescriptor {
                    selector,
                    target: target.clone(),
                    decision: rule.decision,
                    kind,
                };
                if let Some(existing) = descriptors
                    .iter()
                    .find(|existing| existing.key() == descriptor.key())
                {
                    descriptor.decision =
                        prevailing_decision(existing.decision, descriptor.decision);
                }
                descriptors.retain(|existing| existing.key() != descriptor.key());
                descriptors.push(descriptor);
            }
        }

        Self { descriptors }
    }

    pub fn descriptors(&self) -> &[EffectiveCapabilityDescriptor] {
        &self.descriptors
    }

    pub fn permission_rules(&self) -> Vec<PermissionRule> {
        self.descriptors
            .iter()
            .flat_map(EffectiveCapabilityDescriptor::permission_rules)
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
            ToolSelector::Unmatched(_) => false,
        }
    }

    /// Reconstructs one concrete policy rule per identity this descriptor
    /// covers. A `Pattern` selector's declared `source` (for example `bas*`)
    /// is never reused directly here: it was matched against tool names, not
    /// against the dispatcher's internal identity strings, so a glob built
    /// from it would compare false against every real call. Each matched
    /// identity therefore gets its own `Exact` rule instead.
    fn permission_rules(&self) -> Vec<PermissionRule> {
        let target = self
            .target
            .as_ref()
            .map(|pattern| {
                PermissionPattern::glob_for_target_kind(pattern.clone(), self.kind)
                    .expect("stored target is validated")
            })
            .unwrap_or(PermissionPattern::Any);

        if let ToolSelector::Unmatched(source) = &self.selector {
            return vec![PermissionRule::global(
                self.decision,
                PermissionPattern::glob(source.clone()).expect("stored tool pattern is validated"),
                target,
            )];
        }

        self.selector
            .identities()
            .map(|identity| {
                PermissionRule::global(
                    self.decision,
                    PermissionPattern::Exact(identity.to_owned()),
                    target.clone(),
                )
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ToolSelector {
    Exact(String),
    Pattern {
        source: String,
        identities: Vec<String>,
    },
    /// A declared tool pattern the dispatcher holds nothing for, kept with the
    /// pattern exactly as written. See [`resolved_selectors`].
    Unmatched(String),
}

impl ToolSelector {
    fn identities(&self) -> impl Iterator<Item = &str> {
        match self {
            Self::Exact(identity) => std::slice::from_ref(identity),
            Self::Pattern { identities, .. } => identities.as_slice(),
            Self::Unmatched(_) => &[],
        }
        .iter()
        .map(String::as_str)
    }
}

type DescriptorKey = (ToolSelector, Option<String>);

fn rule_applies_to_project(rule: &PermissionRule, project: &str) -> bool {
    rule.scope == PermissionScope::Global || rule.project.as_deref() == Some(project)
}

/// Resolves a declaration's tool pattern, keeping a rule that matches nothing
/// instead of letting it vanish.
///
/// A definition's `permission_rules` are shared with the delegated-child path,
/// so a rule may legitimately name a tool one surface holds and the other does
/// not — an MCP tool here, a native tool there. Dropping it silently would
/// make the same definition mean different things depending on how it was
/// launched, and would hide a typo behind a rule that appears to be in force.
/// The retained rule is compared against dispatcher identities at evaluation
/// time and matches none of them, so it decides nothing until the surface
/// grows a tool it names.
///
/// An unmatched `allow` is dropped rather than retained: it could only ever
/// widen, and a grant for a tool the dispatcher does not hold grants nothing.
fn resolved_selectors(
    rule: &PermissionRule,
    snapshot: &CapabilitySnapshot,
) -> Vec<(ToolSelector, PermissionTargetKind)> {
    let resolved = selectors(&rule.tool, snapshot);
    if !resolved.is_empty() || rule.decision == PermissionDecision::Allow {
        return resolved;
    }

    let Some(source) = tool_pattern_source(&rule.tool) else {
        return Vec::new();
    };
    let kind = permission_target_kind_for_tool(&source);
    vec![(ToolSelector::Unmatched(source), kind)]
}

fn tool_pattern_source(pattern: &PermissionPattern) -> Option<String> {
    match pattern {
        PermissionPattern::Any => None,
        PermissionPattern::Exact(value) => Some(value.clone()),
        PermissionPattern::Glob(_) => pattern.glob_source().map(ToOwned::to_owned),
    }
}

/// Resolves a declared `tool` pattern into the selector(s) it applies to,
/// each paired with the target kind its own matched tool implies.
///
/// A literal (non-wildcard) pattern names exactly one tool, so its kind is
/// classified directly from that name. A wildcard pattern can match several
/// tools, and those tools are not guaranteed to share a target kind (a
/// pattern spanning `bash` and a path-shaped tool, for instance) — so
/// matched identities are grouped by their own kind rather than by the
/// pattern that found them, and each group becomes its own selector. This
/// keeps a `bash` match's target free-form even when a sibling match under
/// the same wildcard is path-shaped, and vice versa.
fn selectors(
    pattern: &PermissionPattern,
    snapshot: &CapabilitySnapshot,
) -> Vec<(ToolSelector, PermissionTargetKind)> {
    match pattern {
        PermissionPattern::Exact(value) => exact_selector(value, snapshot)
            .map(|selector| (selector, permission_target_kind_for_tool(value)))
            .into_iter()
            .collect(),
        PermissionPattern::Glob(_) if pattern.glob_source().is_some_and(is_literal_glob) => {
            let source = pattern.glob_source().unwrap();
            exact_selector(source, snapshot)
                .map(|selector| (selector, permission_target_kind_for_tool(source)))
                .into_iter()
                .collect()
        }
        PermissionPattern::Any | PermissionPattern::Glob(_) => {
            let source = pattern.glob_source().unwrap_or("*").to_owned();
            let mut groups: Vec<(PermissionTargetKind, Vec<String>)> = Vec::new();

            for (identity, kind) in matched_identities_with_kind(pattern, snapshot) {
                match groups.iter_mut().find(|(existing, _)| *existing == kind) {
                    Some((_, identities)) => identities.push(identity),
                    None => groups.push((kind, vec![identity])),
                }
            }

            groups
                .into_iter()
                .map(|(kind, identities)| {
                    (
                        ToolSelector::Pattern {
                            source: source.clone(),
                            identities,
                        },
                        kind,
                    )
                })
                .collect()
        }
    }
}

/// Matches a wildcard tool pattern both against the dispatcher's internal
/// identity strings directly (`native:4:bash`) and against every alias
/// (bare and qualified tool names) those identities are known by, so a
/// pattern written in either shape resolves the same matched tools. The
/// alias form is preferred for classifying a match's target kind, since it
/// still carries a recognizable tool name; the raw identity form does not.
fn matched_identities_with_kind(
    pattern: &PermissionPattern,
    snapshot: &CapabilitySnapshot,
) -> Vec<(String, PermissionTargetKind)> {
    let mut kinds: BTreeMap<String, PermissionTargetKind> = BTreeMap::new();

    for identity in &snapshot.identities {
        if pattern.matches(identity) {
            kinds
                .entry(identity.clone())
                .or_insert(PermissionTargetKind::Path);
        }
    }
    for (alias, identity) in &snapshot.aliases {
        if pattern.matches(alias) {
            kinds.insert(identity.clone(), permission_target_kind_for_tool(alias));
        }
    }

    kinds.into_iter().collect()
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

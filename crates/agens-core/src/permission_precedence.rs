//! The single owner of permission-rule precedence.
//!
//! Every surface that turns declarations into a [`PermissionPolicy`] — a
//! delegated child's tool surface, a primary agent's capability set, and the
//! configured `[permissions]` block — asks this module which rule prevails, so
//! one rule set can only ever produce one decision.
//!
//! [`PermissionPolicy`]: crate::PermissionPolicy

use crate::{PermissionDecision, PermissionRequest, PermissionRule, PermissionScope};

/// Reports the decision `rules` give `request`, or `None` when none of them
/// selects it.
///
/// Precedence is read off what a rule DENOTES, never off how it was spelled or
/// where it was written:
///
/// 1. A rule whose selected calls are a strict subset of another's outranks it,
///    so `deny bash rm*` decides `rm -rf x` even beside `allow bash *`, and
///    `allow write src/generated/**` decides its own subtree even beside
///    `deny write src/**`. "Selects everything" is one breadth rather than
///    several: an absent target, `*` on a free-form target and `**` are the
///    same rule written three ways.
///    A rule scoped to one project also selects strictly fewer calls than the
///    same rule written globally.
/// 2. Where neither rule's selection contains the other's — or where the
///    containment cannot be established, which [`crate::PermissionPattern::covers`]
///    reports conservatively — the rules tie, and the more RESTRICTIVE decision
///    wins: `deny` beats `ask`, and `ask` beats `allow`. `ask` sits between the
///    two because it withholds authority pending a human, which grants strictly
///    less than `allow` and strictly more than `deny`; inside a delegated child,
///    where the prompt is unreachable, it resolves to a denial anyway.
///
/// Authoring order is never consulted, so appending a rule can never silently
/// revoke one already written.
pub fn prevailing_rule_decision(
    rules: &[PermissionRule],
    request: &PermissionRequest,
) -> Option<PermissionDecision> {
    let matching = rules
        .iter()
        .filter(|rule| rule.matches(request))
        .collect::<Vec<_>>();

    narrowest_decision(&matching)
}

/// Reports whether these rules, taken as a whole, leave no call to `tool` that
/// could be authorized or prompted for.
///
/// A delegated child enforces that case by omitting the tool from its catalog
/// rather than by a policy rule, so this has to answer exactly what
/// [`prevailing_rule_decision`] would for every target at once. It does that by
/// asking that same question about each region the rules themselves carve out,
/// rather than by re-deriving the precedence contract in a second place:
///
/// - unless some rule selects every target, a target no rule names reaches the
///   policy's unmatched fallback, which is not a denial;
/// - otherwise every rule's own region has to net `Deny`, where the rules
///   deciding a region are those whose selection contains it.
///
/// A rule that overlaps a region without containing it cannot make the region
/// unreachable, because the part it does not cover is still a call the region's
/// own rules decide — so leaving those rules out is what makes this exact
/// rather than approximate.
///
/// An `ask` counts as reachable on purpose: a child resolves it to a denial
/// that states the prompt could not be reached, which an omitted tool could
/// never distinguish itself from.
pub fn declarations_deny_every_target(rules: &[PermissionRule], tool: &str) -> bool {
    let matching = rules
        .iter()
        .filter(|rule| rule.tool.matches(tool))
        .collect::<Vec<_>>();

    matching.iter().any(|rule| rule.target.denotes_everything())
        && matching.iter().all(|region| {
            let deciding = matching
                .iter()
                .copied()
                .filter(|rule| covers(rule, region))
                .collect::<Vec<_>>();

            narrowest_decision(&deciding) == Some(PermissionDecision::Deny)
        })
}

/// Resolves two decisions that select the same calls, by the tie-break above.
///
/// Collapsing two such rules into one is the same question precedence answers,
/// so it is answered here rather than beside the caller that needs it.
pub const fn prevailing_decision(
    first: PermissionDecision,
    second: PermissionDecision,
) -> PermissionDecision {
    if restrictiveness(second) >= restrictiveness(first) {
        second
    } else {
        first
    }
}

/// Keeps only the rules no other rule strictly narrows, then lets the most
/// restrictive of those decide.
fn narrowest_decision(matching: &[&PermissionRule]) -> Option<PermissionDecision> {
    matching
        .iter()
        .filter(|rule| {
            !matching
                .iter()
                .any(|other| covers(rule, other) && !covers(other, rule))
        })
        .map(|rule| rule.decision)
        .reduce(prevailing_decision)
}

/// Reports whether every call `narrower` selects is also selected by
/// `broader`, across all three axes a rule selects on: the project it is scoped
/// to, the tool, and the target. A rule scoped to one project selects strictly
/// fewer calls than the same rule written globally, which is what lets a
/// project-scoped exception outrank a global default.
fn covers(broader: &PermissionRule, narrower: &PermissionRule) -> bool {
    scope_covers(broader, narrower)
        && broader.tool.covers(&narrower.tool)
        && broader.target.covers(&narrower.target)
}

fn scope_covers(broader: &PermissionRule, narrower: &PermissionRule) -> bool {
    match (broader.scope, narrower.scope) {
        (PermissionScope::Global, _) => true,
        (PermissionScope::Project, PermissionScope::Global) => false,
        (PermissionScope::Project, PermissionScope::Project) => broader.project == narrower.project,
    }
}

/// Ranks decisions from the most permissive to the most restrictive.
const fn restrictiveness(decision: PermissionDecision) -> u8 {
    match decision {
        PermissionDecision::Allow => 0,
        PermissionDecision::Ask => 1,
        PermissionDecision::Deny => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        PermissionDecision, PermissionPattern, ToolAccess, permission_target_kind_for_tool,
    };

    /// Builds a rule the way both production paths build one: the target's
    /// `/`-crossing behavior comes from the tool it belongs to, so `*` is the
    /// whole space for `bash` and stops at a separator for `write`.
    fn rule(decision: PermissionDecision, tool: &str, target: Option<&str>) -> PermissionRule {
        PermissionRule::global(
            decision,
            PermissionPattern::glob(tool).unwrap(),
            match target {
                Some(target) => PermissionPattern::glob_for_target_kind(
                    target,
                    permission_target_kind_for_tool(tool),
                )
                .unwrap(),
                None => PermissionPattern::Any,
            },
        )
    }

    fn decision(rules: &[PermissionRule], tool: &str, target: &str) -> Option<PermissionDecision> {
        prevailing_rule_decision(
            rules,
            &PermissionRequest::new("project", tool, target, ToolAccess::Write),
        )
    }

    /// Both orders, because authoring order is exactly what must not decide.
    fn both_orders(first: PermissionRule, second: PermissionRule) -> [Vec<PermissionRule>; 2] {
        [vec![first.clone(), second.clone()], vec![second, first]]
    }

    #[test]
    fn a_narrower_target_outranks_a_broader_one() {
        for rules in both_orders(
            rule(PermissionDecision::Deny, "bash", Some("rm*")),
            rule(PermissionDecision::Allow, "bash", Some("*")),
        ) {
            assert_eq!(
                decision(&rules, "bash", "rm -rf victim"),
                Some(PermissionDecision::Deny)
            );
            assert_eq!(
                decision(&rules, "bash", "echo hi"),
                Some(PermissionDecision::Allow)
            );
        }
    }

    #[test]
    fn a_nested_path_target_outranks_the_subtree_that_contains_it() {
        for rules in both_orders(
            rule(PermissionDecision::Deny, "write", Some("src/secret/**")),
            rule(PermissionDecision::Allow, "write", Some("src/**")),
        ) {
            assert_eq!(
                decision(&rules, "write", "src/secret/key.txt"),
                Some(PermissionDecision::Deny)
            );
            assert_eq!(
                decision(&rules, "write", "src/main.rs"),
                Some(PermissionDecision::Allow)
            );
        }
    }

    /// The shape that reopened a closed tool: an explicitly spelled `*` names
    /// exactly the calls an absent target names, so it can never outrank it.
    #[test]
    fn an_explicit_wildcard_target_ties_with_an_absent_one() {
        for rules in both_orders(
            rule(PermissionDecision::Deny, "bash", None),
            rule(PermissionDecision::Allow, "bash", Some("*")),
        ) {
            assert_eq!(
                decision(&rules, "bash", "rm -rf victim"),
                Some(PermissionDecision::Deny)
            );
        }

        for rules in both_orders(
            rule(PermissionDecision::Deny, "write", None),
            rule(PermissionDecision::Allow, "write", Some("**")),
        ) {
            assert_eq!(
                decision(&rules, "write", ".env"),
                Some(PermissionDecision::Deny)
            );
        }
    }

    #[test]
    fn incomparable_targets_tie_and_the_more_restrictive_decision_wins() {
        for rules in both_orders(
            rule(PermissionDecision::Deny, "bash", Some("*victim*")),
            rule(PermissionDecision::Allow, "bash", Some("rm*")),
        ) {
            assert_eq!(
                decision(&rules, "bash", "rm -rf victim"),
                Some(PermissionDecision::Deny)
            );
        }
    }

    #[test]
    fn ask_sits_between_allow_and_deny_on_a_tie() {
        for rules in both_orders(
            rule(PermissionDecision::Ask, "bash", None),
            rule(PermissionDecision::Allow, "bash", None),
        ) {
            assert_eq!(
                decision(&rules, "bash", "echo hi"),
                Some(PermissionDecision::Ask)
            );
        }

        for rules in both_orders(
            rule(PermissionDecision::Ask, "bash", None),
            rule(PermissionDecision::Deny, "bash", None),
        ) {
            assert_eq!(
                decision(&rules, "bash", "echo hi"),
                Some(PermissionDecision::Deny)
            );
        }
    }

    #[test]
    fn a_narrower_tool_pattern_outranks_a_broader_one_on_an_equal_target() {
        for rules in both_orders(
            PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::Exact("native::bash".into()),
                PermissionPattern::Any,
            ),
            rule(PermissionDecision::Allow, "*", None),
        ) {
            assert_eq!(
                decision(&rules, "native::bash", "echo hi"),
                Some(PermissionDecision::Deny)
            );
        }
    }

    #[test]
    fn an_untargeted_allow_cannot_reopen_a_tool_an_untargeted_deny_closed() {
        for rules in both_orders(
            rule(PermissionDecision::Deny, "bash", None),
            rule(PermissionDecision::Allow, "bash", None),
        ) {
            assert!(declarations_deny_every_target(&rules, "bash"));
        }
    }

    /// The catalog and the policy have to answer one question: an explicit
    /// wildcard allow that the policy ties into a denial must not leave the
    /// tool reachable, and vice versa.
    #[test]
    fn omission_agrees_with_the_policy_on_an_explicit_wildcard_pairing() {
        for rules in both_orders(
            rule(PermissionDecision::Allow, "bash", None),
            rule(PermissionDecision::Deny, "bash", Some("*")),
        ) {
            assert_eq!(
                decision(&rules, "bash", "echo hi"),
                Some(PermissionDecision::Deny)
            );
            assert!(declarations_deny_every_target(&rules, "bash"));
        }
    }

    #[test]
    fn a_targeted_deny_never_closes_a_tool_outright() {
        assert!(!declarations_deny_every_target(
            &[rule(PermissionDecision::Deny, "bash", Some("rm*"))],
            "bash"
        ));
    }

    /// Omission has to answer exactly what precedence answers: a targeted
    /// `allow` outranks an untargeted `deny`, so the tool is still reachable
    /// for the calls that rule covers and must stay in the catalog.
    #[test]
    fn a_targeted_allow_keeps_a_tool_an_untargeted_deny_would_have_closed() {
        for rules in both_orders(
            rule(PermissionDecision::Deny, "bash", None),
            rule(PermissionDecision::Allow, "bash", Some("git*")),
        ) {
            assert!(!declarations_deny_every_target(&rules, "bash"));
            assert_eq!(
                decision(&rules, "bash", "git status"),
                Some(PermissionDecision::Allow)
            );
        }
    }

    /// A child resolves `ask` to a denial that says the prompt was unreachable,
    /// which an omitted tool could never distinguish itself from.
    #[test]
    fn a_targeted_ask_keeps_a_tool_an_untargeted_deny_would_have_closed() {
        assert!(!declarations_deny_every_target(
            &[
                rule(PermissionDecision::Deny, "bash", None),
                rule(PermissionDecision::Ask, "bash", Some("git*")),
            ],
            "bash"
        ));
    }

    #[test]
    fn a_targeted_deny_beside_an_untargeted_deny_still_closes_the_tool() {
        assert!(declarations_deny_every_target(
            &[
                rule(PermissionDecision::Deny, "bash", None),
                rule(PermissionDecision::Deny, "bash", Some("rm*")),
            ],
            "bash"
        ));
    }

    #[test]
    fn a_path_wildcard_that_stops_at_a_separator_closes_no_tool() {
        assert!(!declarations_deny_every_target(
            &[rule(PermissionDecision::Deny, "write", Some("*"))],
            "write"
        ));
        assert!(declarations_deny_every_target(
            &[rule(PermissionDecision::Deny, "write", Some("**"))],
            "write"
        ));
    }
}

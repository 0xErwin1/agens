//! The single owner of permission-rule precedence.
//!
//! Every surface that turns declarations into a [`PermissionPolicy`] — a
//! delegated child's tool surface, a primary agent's capability set, and the
//! configured `[permissions]` block — orders them here, so one declaration set
//! can only ever produce one decision.
//!
//! [`PermissionPolicy`]: crate::PermissionPolicy

use crate::{PermissionPattern, PermissionRule};

/// Orders declarations so that [`PermissionPolicy`]'s last-match-wins
/// resolution of static rules realizes the precedence contract below.
///
/// 1. The declaration with the most specific TARGET wins. An exact target
///    outranks a glob, and a glob outranks [`PermissionPattern::Any`]. This is
///    what makes the natural authoring shape `deny bash rm*` followed by
///    `allow bash` — "bash, except these" — deny `rm` instead of being
///    overtaken by the broad allow that trails it.
/// 2. On an equally specific target, the declaration with the most specific
///    TOOL pattern wins, so `allow *` followed by `deny bash` denies `bash`.
/// 3. On an equally specific target and tool, the LAST declaration wins, which
///    keeps authoring order meaningful where nothing else separates two rules.
///
/// The sort is stable, so rule 3 needs no key of its own.
///
/// [`PermissionPolicy`]: crate::PermissionPolicy
pub fn ordered_permission_rules(mut rules: Vec<PermissionRule>) -> Vec<PermissionRule> {
    rules.sort_by_key(|rule| (specificity(&rule.target), specificity(&rule.tool)));
    rules
}

/// Reports whether these declarations, taken as a whole, deny `tool` for every
/// target it could ever be called with.
///
/// A delegated child enforces that case by omitting the tool from its catalog
/// rather than by a policy rule, so the two enforcement mechanisms have to
/// agree on when it applies. Only untargeted declarations are consulted: a
/// declaration naming a target speaks about some calls, never about all of
/// them, and can never justify removing the tool the narrower rule still needs
/// to act on.
pub fn declarations_deny_every_target(rules: &[PermissionRule], tool: &str) -> bool {
    ordered_permission_rules(rules.to_vec())
        .iter()
        .rfind(|rule| rule.target == PermissionPattern::Any && rule.tool.matches(tool))
        .is_some_and(|rule| rule.decision == crate::PermissionDecision::Deny)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PatternSpecificity {
    Any,
    Glob,
    Exact,
}

fn specificity(pattern: &PermissionPattern) -> PatternSpecificity {
    match pattern {
        PermissionPattern::Any => PatternSpecificity::Any,
        PermissionPattern::Glob(_) => PatternSpecificity::Glob,
        PermissionPattern::Exact(_) => PatternSpecificity::Exact,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PermissionDecision;

    fn rule(decision: PermissionDecision, tool: &str, target: Option<&str>) -> PermissionRule {
        PermissionRule::global(
            decision,
            PermissionPattern::glob(tool).unwrap(),
            match target {
                Some(target) => PermissionPattern::glob(target).unwrap(),
                None => PermissionPattern::Any,
            },
        )
    }

    #[test]
    fn a_targeted_declaration_outranks_an_untargeted_one_declared_after_it() {
        let ordered = ordered_permission_rules(vec![
            rule(PermissionDecision::Deny, "bash", Some("rm*")),
            rule(PermissionDecision::Allow, "bash", None),
        ]);

        assert_eq!(ordered.last().unwrap().decision, PermissionDecision::Deny);
    }

    #[test]
    fn an_exact_tool_outranks_a_wildcard_tool_on_an_equal_target() {
        let ordered = ordered_permission_rules(vec![
            PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::bash".into()),
                PermissionPattern::Any,
            ),
            rule(PermissionDecision::Deny, "bas*", None),
        ]);

        assert_eq!(ordered.last().unwrap().decision, PermissionDecision::Allow);
    }

    #[test]
    fn equally_specific_declarations_keep_their_authoring_order() {
        let ordered = ordered_permission_rules(vec![
            rule(PermissionDecision::Deny, "bash", None),
            rule(PermissionDecision::Allow, "bash", None),
        ]);

        assert_eq!(ordered.last().unwrap().decision, PermissionDecision::Allow);
    }

    #[test]
    fn a_trailing_untargeted_allow_reopens_a_tool_an_earlier_untargeted_deny_closed() {
        assert!(declarations_deny_every_target(
            &[rule(PermissionDecision::Deny, "bash", None)],
            "bash"
        ));
        assert!(!declarations_deny_every_target(
            &[
                rule(PermissionDecision::Deny, "bash", None),
                rule(PermissionDecision::Allow, "bash", None),
            ],
            "bash"
        ));
    }

    #[test]
    fn a_targeted_deny_never_closes_a_tool_outright() {
        assert!(!declarations_deny_every_target(
            &[rule(PermissionDecision::Deny, "bash", Some("rm*"))],
            "bash"
        ));
    }
}

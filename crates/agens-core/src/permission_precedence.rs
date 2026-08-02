//! The single owner of permission-rule precedence.
//!
//! Every surface that turns declarations into a [`PermissionPolicy`] — a
//! delegated child's tool surface, a primary agent's capability set, and the
//! configured `[permissions]` block — orders them here, so one declaration set
//! can only ever produce one decision.
//!
//! [`PermissionPolicy`]: crate::PermissionPolicy

use crate::{PermissionDecision, PermissionPattern, PermissionRule};

/// Orders rules so that [`PermissionPolicy`]'s last-match-wins resolution of
/// static rules realizes the precedence contract below.
///
/// 1. The rule with the narrower TARGET wins. Naming any target at all
///    outranks [`PermissionPattern::Any`], which is what makes the natural
///    authoring shape `deny bash rm*` followed by `allow bash` — "bash, except
///    these" — deny `rm` instead of being overtaken by the broad allow that
///    trails it. Two *named* targets are treated as equally specific: `rm*` and
///    `*` are both globs, and there is no total order on glob breadth to
///    separate them by.
/// 2. On an equally specific target, the rule with the narrower TOOL pattern
///    wins, so `allow *` followed by `deny bash` denies `bash`.
/// 3. On an equally specific target and tool, the more RESTRICTIVE decision
///    wins: `deny` beats `ask`, and `ask` beats `allow`. Authoring order never
///    decides safety, so `deny bash rm*` and `allow bash *` deny `rm` in either
///    order, and a `deny` cannot be silently revoked by appending an allow.
///    `ask` sits between the two because it withholds authority pending a
///    human, which grants strictly less than `allow` and strictly more than
///    `deny`; inside a delegated child, where the prompt is unreachable, it
///    resolves to a denial anyway.
///
/// The consequence to be aware of is that a *narrower* `allow` cannot carve an
/// exception out of a *broader* `deny` when both name a target, because rule 1
/// already ranks them equal and rule 3 then hands the tie to the deny. An
/// exception has to be carved the other way round — a broad `allow` with the
/// exceptions denied — or by leaving the broader rule untargeted, which rule 1
/// does separate.
///
/// The sort is stable, so two rules identical under all three rules keep their
/// authoring order, which is the only case where authoring order is consulted
/// at all.
///
/// [`PermissionPolicy`]: crate::PermissionPolicy
pub fn ordered_permission_rules(mut rules: Vec<PermissionRule>) -> Vec<PermissionRule> {
    rules.sort_by_key(|rule| {
        (
            specificity(&rule.target),
            specificity(&rule.tool),
            restrictiveness(rule.decision),
        )
    });
    rules
}

/// Reports whether these rules, taken as a whole, leave no call to `tool` that
/// could be authorized or prompted for.
///
/// A delegated child enforces that case by omitting the tool from its catalog
/// rather than by a policy rule, so this has to answer exactly what
/// [`ordered_permission_rules`] would: a tool is omitted if and only if the
/// ordering leaves no reachable non-`Deny` decision for it.
///
/// That reduces to two questions against the ordered rules, because rule 1
/// places every target-naming rule after every untargeted one, so an untargeted
/// rule can never outrank a targeted one:
///
/// 1. does the winning UNTARGETED rule for the tool deny it, and
/// 2. is every target-naming rule for the tool also a deny?
///
/// If a target-naming rule allows or asks, that rule outranks the blanket deny
/// for the calls it covers, so those calls are still reachable and the tool has
/// to stay in the catalog for the rule to act on. An `ask` counts as reachable
/// on purpose: a child resolves it to a denial that states the prompt could not
/// be reached, which an omitted tool could never distinguish itself from.
pub fn declarations_deny_every_target(rules: &[PermissionRule], tool: &str) -> bool {
    let ordered = ordered_permission_rules(rules.to_vec());
    let matching = || ordered.iter().filter(|rule| rule.tool.matches(tool));

    let denied_untargeted = matching()
        .rfind(|rule| rule.target == PermissionPattern::Any)
        .is_some_and(|rule| rule.decision == PermissionDecision::Deny);

    denied_untargeted
        && matching().all(|rule| {
            rule.target == PermissionPattern::Any || rule.decision == PermissionDecision::Deny
        })
}

/// Resolves two decisions that rules 1 and 2 above rank equal, by rule 3.
///
/// Collapsing two rules that select exactly the same calls into one is the same
/// question the ordering answers, so it is answered here rather than beside the
/// caller that needs it.
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

/// How narrowly a pattern selects, for ordering only.
///
/// There are exactly two rungs because there is no defensible total order on
/// glob breadth: `rm*` matches strictly less than `*`, but `a*` and `*b` are
/// incomparable, and a rule that decides safety must not depend on a heuristic
/// that answers differently for equally reasonable authoring. Rules that tie
/// here are separated by [`restrictiveness`] instead, which is total.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PatternSpecificity {
    Any,
    Named,
}

const fn specificity(pattern: &PermissionPattern) -> PatternSpecificity {
    match pattern {
        PermissionPattern::Any => PatternSpecificity::Any,
        PermissionPattern::Glob(_) | PermissionPattern::Exact(_) => PatternSpecificity::Named,
    }
}

/// Ranks decisions from the most permissive to the most restrictive, so that a
/// stable ascending sort under last-match-wins resolution lets the most
/// restrictive one win a tie.
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
    fn a_narrower_tool_pattern_outranks_a_broader_one_on_an_equal_target() {
        let ordered = ordered_permission_rules(vec![
            rule(PermissionDecision::Deny, "bash", None),
            PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Any,
                PermissionPattern::Any,
            ),
        ]);

        assert_eq!(ordered.last().unwrap().decision, PermissionDecision::Deny);
    }

    #[test]
    fn deny_wins_an_equally_specific_tie_in_either_authoring_order() {
        for pair in [
            [
                rule(PermissionDecision::Deny, "bash", None),
                rule(PermissionDecision::Allow, "bash", None),
            ],
            [
                rule(PermissionDecision::Allow, "bash", None),
                rule(PermissionDecision::Deny, "bash", None),
            ],
        ] {
            let ordered = ordered_permission_rules(pair.to_vec());

            assert_eq!(ordered.last().unwrap().decision, PermissionDecision::Deny);
        }
    }

    #[test]
    fn two_globs_of_different_breadth_tie_and_the_deny_takes_the_tie() {
        let ordered = ordered_permission_rules(vec![
            rule(PermissionDecision::Deny, "bash", Some("rm*")),
            rule(PermissionDecision::Allow, "bash", Some("*")),
        ]);

        assert_eq!(ordered.last().unwrap().decision, PermissionDecision::Deny);
    }

    #[test]
    fn ask_outranks_allow_and_loses_to_deny_on_an_equal_tie() {
        let ordered = ordered_permission_rules(vec![
            rule(PermissionDecision::Ask, "bash", None),
            rule(PermissionDecision::Allow, "bash", None),
        ]);
        assert_eq!(ordered.last().unwrap().decision, PermissionDecision::Ask);

        let ordered = ordered_permission_rules(vec![
            rule(PermissionDecision::Deny, "bash", None),
            rule(PermissionDecision::Ask, "bash", None),
        ]);
        assert_eq!(ordered.last().unwrap().decision, PermissionDecision::Deny);
    }

    #[test]
    fn an_untargeted_allow_cannot_reopen_a_tool_an_untargeted_deny_closed() {
        for pair in [
            [
                rule(PermissionDecision::Deny, "bash", None),
                rule(PermissionDecision::Allow, "bash", None),
            ],
            [
                rule(PermissionDecision::Allow, "bash", None),
                rule(PermissionDecision::Deny, "bash", None),
            ],
        ] {
            assert!(declarations_deny_every_target(&pair, "bash"));
        }
    }

    #[test]
    fn a_targeted_deny_never_closes_a_tool_outright() {
        assert!(!declarations_deny_every_target(
            &[rule(PermissionDecision::Deny, "bash", Some("rm*"))],
            "bash"
        ));
    }

    /// Omission has to answer exactly what the ordering answers: a targeted
    /// `allow` outranks an untargeted `deny`, so the tool is still reachable
    /// for the calls that rule covers and must stay in the catalog.
    #[test]
    fn a_targeted_allow_keeps_a_tool_an_untargeted_deny_would_have_closed() {
        for pair in [
            [
                rule(PermissionDecision::Deny, "bash", None),
                rule(PermissionDecision::Allow, "bash", Some("git*")),
            ],
            [
                rule(PermissionDecision::Allow, "bash", Some("git*")),
                rule(PermissionDecision::Deny, "bash", None),
            ],
        ] {
            assert!(!declarations_deny_every_target(&pair, "bash"));
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
}

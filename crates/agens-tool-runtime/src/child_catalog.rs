//! Resolving a delegated subagent's native tool surface from its declared
//! `permission_rules`.
//!
//! The child's catalog is a filter over the parent's native tools, never a
//! union: a declaration can only narrow what the child inherits, and an
//! `allow` naming a tool the parent does not hold is a hard error rather than
//! a silent clamp. A `deny` or `ask` naming an unheld tool exceeds nothing, so
//! it is retained inert instead — see [`normalize_declared_tool`].
//!
//! A tool is omitted from the catalog exactly when the declarations leave no
//! call to it that the policy could answer with anything but `Deny`. Every
//! other narrowing — a target-scoped `deny bash rm*`, or a `deny bash` beside
//! an `allow bash git*` that outranks it for the calls it names — leaves the
//! tool in the catalog and relies on the retained policy rules, because the
//! tool has to be present for the narrower rule to have anything to act on.
//!
//! Omission is not decided here. `agens_core::declarations_deny_every_target`
//! derives it from the same ordering that resolves the retained rules, so the
//! two enforcement mechanisms answer one question rather than two similar
//! ones and cannot disagree about a declaration set.

use agens_core::{
    PermissionDecision, PermissionPattern, PermissionRule, SafetyPredicate,
    declarations_deny_every_target, ordered_permission_rules, permission_target_kind_for_tool,
};
use agens_tools::{NativeToolCatalog, NativeToolMetadata, TaskDeclarationRejection};

/// A declaration that cannot be delegated, kept structured rather than
/// formatted so the offending name survives all the way to the parent's tool
/// result instead of only reaching the diagnostics log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildSurfaceRejection {
    pub reason: TaskDeclarationRejection,
    pub tool: String,
}

impl ChildSurfaceRejection {
    /// The operator-facing form, recorded in the diagnostics log. The parent's
    /// tool result carries the same two facts through
    /// [`agens_tools::TaskDeclarationRejection`] instead of this string,
    /// because a terminal message bypasses the output sanitizer.
    pub fn message(&self) -> String {
        let subject = match self.reason {
            TaskDeclarationRejection::ExceedsParentSurface => "the parent does not hold",
            TaskDeclarationRejection::ConfigurationDenies => "the configuration denies",
        };
        format!(
            "permission declaration grants a tool {subject}: {}",
            self.tool
        )
    }
}

/// Native tool identities auto-authorized for every delegated child: bounded
/// filesystem and VCS reads. `native::webfetch` is deliberately excluded —
/// it is `ToolAccess::ReadOnly` too, but that flag classifies worktree
/// impact, not network egress, so treating it as read-class here would grant
/// unattended network access with no declaration.
const AUTO_ALLOW_NATIVE_TOOLS: [&str; 6] = [
    "native::read",
    "native::list",
    "native::search",
    "native::grep",
    "native::glob",
    "native::git_read",
];

#[derive(Debug)]
pub struct ChildToolSurface {
    pub tools: Vec<NativeToolMetadata>,
    pub rules: Vec<PermissionRule>,
    pub safety_predicates: Vec<SafetyPredicate>,
}

/// Resolves the surface a delegated child runs under from the parent's own
/// configured `[permissions]` rules and the agent definition's declarations.
///
/// `parent_rules` bound the result and are never a source of authority: only a
/// declaration can authorize a child tool, so a configured `allow` grants the
/// child nothing on its own. It can, however, carve an exception out of a
/// configured `deny`, because the configured rules are resolved against each
/// other before any declaration sees them.
///
/// Where that resolution nets to a denial it is enforced three ways — the tool
/// leaves the catalog when no call to it could survive, a declaration that
/// would reopen such a tool is a hard error, and the whole configured set is
/// carried as a [`SafetyPredicate::ConfiguredDenial`] so no declaration can
/// outrank it on the child's own policy. The primary path holds the same
/// predicate over the same rules, which is what keeps the two from answering
/// a configured deny differently.
pub fn resolve_child_surface(
    parent_rules: &[PermissionRule],
    declarations: &[PermissionRule],
) -> Result<ChildToolSurface, ChildSurfaceRejection> {
    let metadata = NativeToolCatalog::metadata();

    let parent_rules = parent_rules
        .iter()
        .cloned()
        .flat_map(|rule| normalize_declared_tool(rule, &metadata))
        .collect::<Vec<_>>();

    for declaration in declarations {
        if declaration.decision != PermissionDecision::Allow {
            continue;
        }
        if !metadata
            .iter()
            .any(|entry| declaration_names_tool(declaration, entry))
        {
            return Err(ChildSurfaceRejection {
                reason: TaskDeclarationRejection::ExceedsParentSurface,
                tool: declaration_tool_label(&declaration.tool),
            });
        }
        if let Some(entry) = metadata.iter().find(|entry| {
            declaration_names_tool(declaration, entry)
                && declarations_deny_every_target(&parent_rules, &entry.qualified_name)
        }) {
            return Err(ChildSurfaceRejection {
                reason: TaskDeclarationRejection::ConfigurationDenies,
                tool: entry.qualified_name.clone(),
            });
        }
    }

    let normalized_declarations = declarations
        .iter()
        .cloned()
        .flat_map(|declaration| normalize_declared_tool(declaration, &metadata))
        .collect::<Vec<_>>();

    let tools = metadata
        .into_iter()
        .filter(|entry| {
            !declarations_deny_every_target(&normalized_declarations, &entry.qualified_name)
                && !declarations_deny_every_target(&parent_rules, &entry.qualified_name)
        })
        .collect::<Vec<_>>();

    let mut rules = AUTO_ALLOW_NATIVE_TOOLS
        .into_iter()
        .map(|tool| {
            PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact(tool.into()),
                PermissionPattern::Any,
            )
        })
        .collect::<Vec<_>>();
    rules.extend(normalized_declarations);

    let safety_predicates = vec![SafetyPredicate::ConfiguredDenial(ordered_permission_rules(
        parent_rules,
    ))];

    Ok(ChildToolSurface {
        tools,
        rules: ordered_permission_rules(rules),
        safety_predicates,
    })
}

fn declaration_names_tool(declaration: &PermissionRule, entry: &NativeToolMetadata) -> bool {
    let bare = entry
        .qualified_name
        .strip_prefix("native::")
        .unwrap_or(entry.qualified_name.as_str());
    declaration.tool.matches(&entry.qualified_name) || declaration.tool.matches(bare)
}

/// A dispatched child call carries its tool identity fully qualified and
/// further encoded by the dispatcher (`native:4:bash`), but a declaration
/// written in an agent's markdown names it bare (`write`) or qualified
/// (`native::write`) — the short forms the parent path resolves through
/// `EffectiveCapabilitySet::from_agent`'s dispatcher-backed alias lookup.
/// A tool pattern, literal or wildcard, is matched here against the same
/// bare/qualified forms `declaration_names_tool` uses for catalog omission
/// and against the same forms the load-time diagnostic checks, then expanded
/// into one concrete `Exact` rule per matched native tool. This is the only
/// normalization a declared tool pattern needs: the dispatcher's own alias
/// lookup then carries an `Exact(qualified_name)` the rest of the way to the
/// identity string policy evaluation actually compares against. A raw
/// `Glob` tool pattern is never retained past this point, because it can
/// never be compared against that identity string directly.
///
/// A declared target's `/`-crossing behavior depends on which concrete tool
/// it belongs to (`permission_target_kind_for_tool`), and a wildcard tool
/// pattern is only classifiable once it has been expanded to a concrete
/// native tool — never from the raw declared token. The target glob is
/// therefore rebuilt per expanded tool from its own source pattern, so two
/// tools expanded from the same wildcard (for example a pattern spanning
/// `bash` and a path-shaped tool) each keep the target-kind their own name
/// implies, rather than inheriting whatever kind the raw wildcard token
/// happened to classify as.
///
/// A declaration matching no native tool is kept verbatim rather than dropped.
/// `permission_rules` are shared with the primary path, where the same rule may
/// name an MCP tool this surface never holds, so vanishing here would silently
/// change what the definition means depending on how it was launched. Only a
/// `deny` or `ask` reaches this branch — an unmatched `allow` is rejected by
/// the subset invariant before normalization — so a retained rule can narrow
/// and never widen.
fn normalize_declared_tool(
    declaration: PermissionRule,
    metadata: &[NativeToolMetadata],
) -> Vec<PermissionRule> {
    let expanded = metadata
        .iter()
        .filter(|entry| declaration_names_tool(&declaration, entry))
        .map(|entry| PermissionRule {
            tool: PermissionPattern::Exact(entry.qualified_name.clone()),
            target: retarget_for_tool(&declaration.target, &entry.qualified_name),
            ..declaration.clone()
        })
        .collect::<Vec<_>>();

    if expanded.is_empty() {
        return vec![declaration];
    }

    expanded
}

fn retarget_for_tool(target: &PermissionPattern, qualified_tool_name: &str) -> PermissionPattern {
    match target {
        PermissionPattern::Glob(_) => {
            let source = target
                .glob_source()
                .expect("a Glob variant always carries its source pattern");
            PermissionPattern::glob_for_target_kind(
                source,
                permission_target_kind_for_tool(qualified_tool_name),
            )
            .expect("a source pattern already validated under one target kind stays valid under another")
        }
        PermissionPattern::Any | PermissionPattern::Exact(_) => target.clone(),
    }
}

fn declaration_tool_label(pattern: &PermissionPattern) -> String {
    match pattern {
        PermissionPattern::Any => "*".to_owned(),
        PermissionPattern::Exact(value) => value.clone(),
        PermissionPattern::Glob(_) => pattern.glob_source().unwrap_or("*").to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(decision: PermissionDecision, tool: &str) -> PermissionRule {
        PermissionRule::global(
            decision,
            PermissionPattern::glob(tool).unwrap(),
            PermissionPattern::Any,
        )
    }

    fn tool_names(surface: &ChildToolSurface) -> Vec<String> {
        surface
            .tools
            .iter()
            .map(|entry| entry.qualified_name.clone())
            .collect()
    }

    #[test]
    fn empty_declarations_yield_every_native_tool() {
        let surface = resolve_child_surface(&[], &[]).unwrap();

        assert_eq!(
            tool_names(&surface),
            NativeToolCatalog::metadata()
                .into_iter()
                .map(|entry| entry.qualified_name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_declared_deny_omits_its_tool_from_the_catalog() {
        let surface =
            resolve_child_surface(&[], &[rule(PermissionDecision::Deny, "bash")]).unwrap();

        assert!(!tool_names(&surface).contains(&"native::bash".to_owned()));
        assert_eq!(
            tool_names(&surface).len(),
            NativeToolCatalog::metadata().len() - 1
        );
    }

    #[test]
    fn a_target_scoped_deny_keeps_its_tool_in_the_catalog() {
        let targeted_deny = PermissionRule::global(
            PermissionDecision::Deny,
            PermissionPattern::glob("bash").unwrap(),
            PermissionPattern::glob("rm -rf /**").unwrap(),
        );
        let surface = resolve_child_surface(
            &[],
            &[rule(PermissionDecision::Allow, "bash"), targeted_deny],
        )
        .unwrap();

        assert!(
            tool_names(&surface).contains(&"native::bash".to_owned()),
            "a target-scoped deny must not omit its tool from the catalog, \
             only an untargeted deny does that"
        );
        assert!(
            surface.rules.iter().any(|rule| {
                rule.decision == PermissionDecision::Deny && rule.tool.matches("native::bash")
            }),
            "the target-scoped deny must still be retained as a policy rule"
        );
    }

    #[test]
    fn an_allow_naming_an_unknown_tool_errors_naming_it() {
        let rejection =
            resolve_child_surface(&[], &[rule(PermissionDecision::Allow, "not_a_real_tool")])
                .unwrap_err();

        assert_eq!(
            rejection,
            ChildSurfaceRejection {
                reason: TaskDeclarationRejection::ExceedsParentSurface,
                tool: "not_a_real_tool".into(),
            }
        );
        assert!(rejection.message().contains("not_a_real_tool"));
    }

    /// The subset invariant governs what a declaration would GRANT. A `deny`
    /// or `ask` naming a tool this surface never holds — an MCP tool the
    /// primary path resolves, or a typo — exceeds nothing, so rejecting it
    /// would make an otherwise valid definition undelegatable.
    #[test]
    fn a_deny_or_ask_naming_an_unheld_tool_is_retained_rather_than_rejected() {
        for decision in [PermissionDecision::Deny, PermissionDecision::Ask] {
            for tool in ["webfetc", "mcp::github::create_issue", "zz*"] {
                let surface = resolve_child_surface(&[], &[rule(decision, tool)])
                    .unwrap_or_else(|error| panic!("{decision:?} {tool} must resolve: {error:?}"));

                assert_eq!(
                    tool_names(&surface).len(),
                    NativeToolCatalog::metadata().len(),
                    "{decision:?} {tool} matches no native tool and must omit none"
                );
                assert!(
                    surface
                        .rules
                        .iter()
                        .any(|rule| rule.decision == decision && rule.tool.matches(tool)),
                    "{decision:?} {tool} must be retained as a rule, not dropped"
                );
            }
        }
    }

    #[test]
    fn declared_rules_are_appended_after_the_derived_allow_rules() {
        let surface =
            resolve_child_surface(&[], &[rule(PermissionDecision::Deny, "read")]).unwrap();

        let allow_read_index = surface
            .rules
            .iter()
            .position(|rule| {
                rule.decision == PermissionDecision::Allow && rule.tool.matches("native::read")
            })
            .expect("derived allow rule for read must exist");
        let deny_read_index = surface
            .rules
            .iter()
            .position(|rule| {
                rule.decision == PermissionDecision::Deny && rule.tool.matches("native::read")
            })
            .expect("declared deny rule for read must exist");

        assert!(allow_read_index < deny_read_index);
    }

    fn parent_deny(tool: &str, target: Option<&str>) -> PermissionRule {
        PermissionRule::global(
            PermissionDecision::Deny,
            PermissionPattern::Exact(format!("native::{tool}")),
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

    /// A child must never hold what the parent's own configuration denies, so
    /// an untargeted configured deny removes the tool from the child catalog
    /// no matter what the definition declares.
    #[test]
    fn a_configured_deny_omits_its_tool_from_the_child_catalog() {
        let surface =
            resolve_child_surface(&[parent_deny("bash", None)], &[]).expect("surface must resolve");

        assert!(!tool_names(&surface).contains(&"native::bash".to_owned()));
    }

    #[test]
    fn a_declared_allow_cannot_reopen_a_tool_the_configuration_denies() {
        let rejection = resolve_child_surface(
            &[parent_deny("bash", None)],
            &[rule(PermissionDecision::Allow, "bash")],
        )
        .unwrap_err();

        assert_eq!(
            rejection,
            ChildSurfaceRejection {
                reason: TaskDeclarationRejection::ConfigurationDenies,
                tool: "native::bash".into(),
            }
        );
        assert!(rejection.message().contains("native::bash"));
    }

    /// A targeted configured deny leaves the tool reachable, so it has to be
    /// enforced above the declarations rather than beside them — a declared
    /// `allow bash` would otherwise outrank it on the child's own policy.
    #[test]
    fn a_targeted_configured_deny_becomes_a_child_safety_predicate() {
        let surface = resolve_child_surface(
            &[parent_deny("bash", Some("rm*"))],
            &[rule(PermissionDecision::Allow, "bash")],
        )
        .expect("a targeted configured deny must not reject the delegation");

        assert!(tool_names(&surface).contains(&"native::bash".to_owned()));
        assert!(
            surface.safety_predicates.iter().any(|predicate| matches!(
                predicate,
                SafetyPredicate::ConfiguredDenial(rules)
                    if rules.iter().any(|rule| rule.decision == PermissionDecision::Deny
                        && rule.tool.matches("native::bash")
                        && rule.target.matches("rm -rf /tmp/x"))
            )),
            "the configured deny must survive as a hard safety predicate"
        );
    }

    /// The configured rules resolve against each other before any declaration
    /// sees them, so a configured `allow` still carves an exception out of a
    /// configured `deny` — while granting the child nothing on its own.
    #[test]
    fn a_configured_carve_out_survives_into_the_child_predicate() {
        let carve_out = PermissionRule::global(
            PermissionDecision::Allow,
            PermissionPattern::Exact("native::bash".into()),
            PermissionPattern::glob_for_target_kind(
                "git*",
                permission_target_kind_for_tool("bash"),
            )
            .unwrap(),
        );
        let surface = resolve_child_surface(
            &[parent_deny("bash", None), carve_out],
            &[rule(PermissionDecision::Allow, "bash")],
        )
        .expect("a configured carve-out must not reject the delegation");

        assert!(
            tool_names(&surface).contains(&"native::bash".to_owned()),
            "a tool the configuration still allows for some target must stay in the catalog"
        );
    }

    #[test]
    fn a_configured_allow_grants_the_child_nothing_on_its_own() {
        let surface = resolve_child_surface(
            &[PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::bash".into()),
                PermissionPattern::Any,
            )],
            &[],
        )
        .expect("surface must resolve");

        assert!(
            !surface.rules.iter().any(|rule| {
                rule.decision == PermissionDecision::Allow && rule.tool.matches("native::bash")
            }),
            "only a declaration authorizes a child tool, never the parent's configuration"
        );
    }

    #[test]
    fn webfetch_is_excluded_from_the_derived_allow_set() {
        let surface = resolve_child_surface(&[], &[]).unwrap();

        assert!(
            !surface.rules.iter().any(|rule| {
                rule.decision == PermissionDecision::Allow && rule.tool.matches("native::webfetch")
            }),
            "webfetch must require an explicit declaration, never a derived allow"
        );
    }
}

//! Resolving a delegated subagent's native tool surface from its declared
//! `permission_rules`.
//!
//! The child's catalog is a filter over the parent's native tools, never a
//! union: a declaration can only narrow what the child inherits, and any
//! declaration naming a tool the parent does not hold is a hard error rather
//! than a silent clamp.
//!
//! An untargeted `deny` (no target pattern, `PermissionPattern::Any`) omits
//! its tool from the catalog entirely. A target-scoped `deny` (for example
//! `deny bash rm*`) leaves the tool in the catalog and relies on the
//! retained policy rule to deny the matching calls at evaluation time — the
//! tool has to still be present for that narrower rule to have anything to
//! act on.

use agens_core::{
    PermissionDecision, PermissionPattern, PermissionRule, permission_target_kind_for_tool,
};
use agens_error::CliError;
use agens_tools::{NativeToolCatalog, NativeToolMetadata};

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
}

pub fn resolve_child_surface(
    declarations: &[PermissionRule],
) -> Result<ChildToolSurface, CliError> {
    let metadata = NativeToolCatalog::metadata();

    for declaration in declarations {
        if !metadata
            .iter()
            .any(|entry| declaration_names_tool(declaration, entry))
        {
            return Err(CliError::configuration(format!(
                "permission declaration names a tool the parent does not hold: {}",
                declaration_tool_label(&declaration.tool)
            )));
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
            !declarations.iter().any(|declaration| {
                declaration.decision == PermissionDecision::Deny
                    && declaration.target == PermissionPattern::Any
                    && declaration_names_tool(declaration, entry)
            })
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

    Ok(ChildToolSurface { tools, rules })
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
fn normalize_declared_tool(
    declaration: PermissionRule,
    metadata: &[NativeToolMetadata],
) -> Vec<PermissionRule> {
    metadata
        .iter()
        .filter(|entry| declaration_names_tool(&declaration, entry))
        .map(|entry| PermissionRule {
            tool: PermissionPattern::Exact(entry.qualified_name.clone()),
            target: retarget_for_tool(&declaration.target, &entry.qualified_name),
            ..declaration.clone()
        })
        .collect()
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
        let surface = resolve_child_surface(&[]).unwrap();

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
        let surface = resolve_child_surface(&[rule(PermissionDecision::Deny, "bash")]).unwrap();

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
        let surface =
            resolve_child_surface(&[rule(PermissionDecision::Allow, "bash"), targeted_deny])
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
    fn a_declaration_naming_an_unknown_tool_errors_naming_it() {
        let error = resolve_child_surface(&[rule(PermissionDecision::Allow, "not_a_real_tool")])
            .unwrap_err();

        assert!(format!("{error:?}").contains("not_a_real_tool"));
    }

    #[test]
    fn declared_rules_are_appended_after_the_derived_allow_rules() {
        let surface = resolve_child_surface(&[rule(PermissionDecision::Deny, "read")]).unwrap();

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

    #[test]
    fn webfetch_is_excluded_from_the_derived_allow_set() {
        let surface = resolve_child_surface(&[]).unwrap();

        assert!(
            !surface.rules.iter().any(|rule| {
                rule.decision == PermissionDecision::Allow && rule.tool.matches("native::webfetch")
            }),
            "webfetch must require an explicit declaration, never a derived allow"
        );
    }
}

//! Resolving a delegated subagent's native tool surface from its declared
//! `permission_rules`.
//!
//! The child's catalog is a filter over the parent's native tools, never a
//! union: a declaration can only narrow what the child inherits, and any
//! declaration naming a tool the parent does not hold is a hard error rather
//! than a silent clamp.

use agens_core::{PermissionDecision, PermissionPattern, PermissionRule};
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
        .map(|mut declaration| {
            declaration.tool = normalize_declared_tool(declaration.tool, &metadata);
            declaration
        })
        .collect::<Vec<_>>();

    let tools = metadata
        .into_iter()
        .filter(|entry| {
            !declarations.iter().any(|declaration| {
                declaration.decision == PermissionDecision::Deny
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

/// A dispatched child call carries its tool identity fully qualified
/// (`native::write`), but a declaration written in an agent's markdown names
/// it bare (`write`) — the same short form the parent path resolves through
/// `EffectiveCapabilitySet::from_agent`'s dispatcher-backed alias lookup.
/// Mirrors that resolution here so a literal declared name matches the
/// qualified identity the policy actually evaluates against; a genuine
/// wildcard is left untouched; a wildcard is matched directly against the
/// already-qualified identity string at evaluation time.
fn normalize_declared_tool(
    pattern: PermissionPattern,
    metadata: &[NativeToolMetadata],
) -> PermissionPattern {
    let literal = match &pattern {
        PermissionPattern::Glob(_) if pattern.glob_source().is_some_and(is_literal_glob) => {
            pattern.glob_source().map(ToOwned::to_owned)
        }
        _ => None,
    };
    let Some(literal) = literal else {
        return pattern;
    };

    metadata
        .iter()
        .find(|entry| {
            let bare = entry
                .qualified_name
                .strip_prefix("native::")
                .unwrap_or(entry.qualified_name.as_str());
            literal == entry.qualified_name || literal == bare
        })
        .map(|entry| PermissionPattern::Exact(entry.qualified_name.clone()))
        .unwrap_or(pattern)
}

fn is_literal_glob(pattern: &str) -> bool {
    !pattern.contains(['*', '?', '[', ']', '{', '}'])
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

//! Resolving a delegated subagent's native tool surface from its declared
//! `permission_rules`.
//!
//! The child's catalog is a filter over the parent's native tools, never a
//! union: a declaration can only narrow what the child inherits, and an
//! `allow` naming a tool the parent does not hold is a hard error rather than
//! a silent clamp. A `deny` or `ask` naming an unheld tool exceeds nothing, so
//! it is retained inert instead — see [`normalize_declared_tool`].
//!
//! What the child keeps, it may use. Nothing here can be resolved by asking:
//! a delegated execution has no surface to put a prompt on, so an undecided
//! call is a denied call. Authorizing only a read-shaped subset and leaving
//! the rest undecided therefore did not make a child cautious, it made every
//! writing role inert — it could see `bash` in its catalog, call it, and be
//! refused for a prompt nobody could ever answer. Narrowing a child is done by
//! saying so, in the configured `[permissions]` or the agent's own
//! declarations, both of which are enforced below.
//!
//! A tool is omitted from the catalog exactly when the declarations leave no
//! call to it that the policy could answer with anything but `Deny`. Every
//! other narrowing — a target-scoped `deny bash rm*`, or a `deny bash` beside
//! an `allow bash git*` that outranks it for the calls it names — leaves the
//! tool in the catalog and relies on the retained policy rules, because the
//! tool has to be present for the narrower rule to have anything to act on.
//!
//! Omission is not decided here. `agens_core::declarations_deny_every_target`
//! asks the precedence owner what it would decide for each region the rules
//! carve out, so the two enforcement mechanisms answer one question rather
//! than two similar ones.

use agens_core::{
    ConfiguredFloor, PermissionDecision, PermissionPattern, PermissionRule,
    declarations_deny_every_target, permission_target_kind_for_tool,
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

/// Native tools a delegated child holds beside the catalog's. Their
/// implementations are bound to the execution or the installation they read
/// from rather than to the worktree, so the child's runtime constructs and
/// registers them itself instead of reading them out of
/// [`NativeToolCatalog::metadata`].
///
/// They are enumerated here all the same, because a declaration is resolved
/// against whatever this module calls the child's surface. A tool the surface
/// omits is a tool no declaration can name — the rule survives as a pattern
/// that never matches the dispatcher identity, and reads as enforced while
/// deciding nothing.
///
/// `native::skill` is here because an agent definition that tells its executor
/// to read a skill, as the SDD phase definitions do, is describing a tool that
/// executor has to hold. Withholding it did not restrict anything: it left
/// those instructions unexecutable while the parent that wrote them could read
/// the same skill freely.
/// `native::ask_user` is here for the same reason `native::skill` is: a
/// subagent that hits a real fork in the work has the same standing to ask the
/// person as the thread that delegated to it. Withholding it did not make a
/// child safer, it made it guess.
pub const CHILD_NON_CATALOG_TOOLS: [&str; 4] = [
    "native::ask_user",
    "native::skill",
    "native::task_control",
    "native::task_message",
];

#[derive(Debug)]
pub struct ChildToolSurface {
    pub tools: Vec<NativeToolMetadata>,
    /// The coordination tools the declarations leave reachable, in
    /// [`CHILD_NON_CATALOG_TOOLS`] order.
    pub coordination_tools: Vec<&'static str>,
    /// The parent's MCP tool identities the declarations leave reachable.
    pub remote_tools: Vec<String>,
    pub rules: Vec<PermissionRule>,
    pub configured_floor: ConfiguredFloor,
}

/// Resolves the surface a delegated child runs under from the parent's own
/// configured `[permissions]` rules and the agent definition's declarations.
///
/// `parent_rules` bound the result and are never a source of authority: a
/// configured `allow` widens nothing, because the child already authorizes
/// whatever the narrowing leaves it. It can, however, carve an exception out
/// of a configured `deny`, because the configured rules are resolved against
/// each other before any declaration sees them.
///
/// Where that resolution nets to a denial it is enforced three ways — the tool
/// leaves the catalog when no call to it could survive, a declaration that
/// would reopen such a tool is a hard error, and the whole configured set is
/// carried as a [`ConfiguredFloor`] so no declaration can outrank it on the
/// child's own policy. The floor also carries a configured `ask` through, which
/// a child resolves to a denial stating the prompt was unreachable. The primary
/// path holds the same floor over the same rules, which is what keeps the two
/// from answering a configured rule differently; the one deliberate difference
/// is that a configured `allow` authorizes there and not here.
/// `remote_tools` are the MCP identities the parent has already connected.
/// They belong to the surface for the same reason the natives do — they are
/// tools the child will hold — and being here is what lets a definition scope
/// them. Grafting them on after this function returns would leave `allow
/// engram::mem_search` rejected as exceeding a surface that simply had not
/// been told about it yet, so an agent could hold every remote tool or none,
/// but never the ones it named.
/// The catalog a delegated child is resolved against: every native tool
/// except the ones that move the session itself.
fn child_native_metadata() -> Vec<NativeToolMetadata> {
    NativeToolCatalog::metadata()
        .into_iter()
        .filter(|entry| !agens_tools::is_session_scoped_native_tool(&entry.qualified_name))
        .collect()
}

pub fn resolve_child_surface(
    parent_rules: &[PermissionRule],
    declarations: &[PermissionRule],
    remote_tools: &[String],
) -> Result<ChildToolSurface, ChildSurfaceRejection> {
    let metadata = child_native_metadata();
    let surface = metadata
        .iter()
        .map(|entry| entry.qualified_name.as_str())
        .chain(CHILD_NON_CATALOG_TOOLS)
        .chain(remote_tools.iter().map(String::as_str))
        .collect::<Vec<_>>();

    let parent_rules = parent_rules
        .iter()
        .cloned()
        .flat_map(|rule| normalize_declared_tool(rule, &surface))
        .collect::<Vec<_>>();

    for declaration in declarations {
        if declaration.decision != PermissionDecision::Allow {
            continue;
        }
        if !surface
            .iter()
            .any(|tool| declaration_names_tool(declaration, tool))
        {
            return Err(ChildSurfaceRejection {
                reason: TaskDeclarationRejection::ExceedsParentSurface,
                tool: declaration_tool_label(&declaration.tool),
            });
        }
        if let Some(tool) = surface.iter().find(|tool| {
            declaration_names_tool(declaration, tool)
                && declarations_deny_every_target(&parent_rules, tool)
        }) {
            return Err(ChildSurfaceRejection {
                reason: TaskDeclarationRejection::ConfigurationDenies,
                tool: (*tool).to_owned(),
            });
        }
    }

    let normalized_declarations = declarations
        .iter()
        .cloned()
        .flat_map(|declaration| normalize_declared_tool(declaration, &surface))
        .collect::<Vec<_>>();
    let reachable = |tool: &str| {
        !declarations_deny_every_target(&normalized_declarations, tool)
            && !declarations_deny_every_target(&parent_rules, tool)
    };

    let tools = metadata
        .into_iter()
        .filter(|entry| reachable(&entry.qualified_name))
        .collect::<Vec<_>>();
    let coordination_tools = CHILD_NON_CATALOG_TOOLS
        .into_iter()
        .filter(|tool| reachable(tool))
        .collect::<Vec<_>>();

    // A tool the child still holds is authorized outright, because a child
    // cannot be asked: leaving it undecided would only mean the model calls it
    // and the gate answers `Ask` into a surface with nobody on it.
    //
    // Unless the definition allowed that tool itself. An `allow bash git*` is
    // an author enumerating the calls this agent may make, and a blanket allow
    // beside it would not narrow to `git*`, it would erase it — the agent
    // would hold unrestricted bash because its author took the trouble to
    // restrict it. A `deny` or `ask` composes the other way: it subtracts from
    // whatever it sits beside, so the blanket allow stays and the declaration
    // carves out of it.
    let enumerated = |tool: &str| {
        normalized_declarations
            .iter()
            .any(|rule| rule.decision == PermissionDecision::Allow && rule.tool.matches(tool))
    };

    let remote_tools = remote_tools
        .iter()
        .filter(|tool| reachable(tool))
        .cloned()
        .collect::<Vec<_>>();

    let mut rules = tools
        .iter()
        .map(|entry| entry.qualified_name.as_str())
        .chain(coordination_tools.iter().copied())
        .chain(remote_tools.iter().map(String::as_str))
        .filter(|tool| !enumerated(tool))
        .map(|tool| {
            PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact(tool.into()),
                PermissionPattern::Any,
            )
        })
        .collect::<Vec<_>>();
    rules.extend(normalized_declarations);

    Ok(ChildToolSurface {
        tools,
        coordination_tools,
        remote_tools,
        rules,
        configured_floor: ConfiguredFloor::restricting(parent_rules),
    })
}

fn declaration_names_tool(declaration: &PermissionRule, qualified_name: &str) -> bool {
    let bare = qualified_name
        .strip_prefix("native::")
        .unwrap_or(qualified_name);
    declaration.tool.matches(qualified_name) || declaration.tool.matches(bare)
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
/// `surface` is every native the child's dispatcher registers, not only the
/// catalog's: a tool absent from it is one no declaration can reach, because
/// the unexpanded pattern is never compared against a dispatcher identity.
///
/// A declaration matching no native tool is kept verbatim rather than dropped.
/// `permission_rules` are shared with the primary path, where the same rule may
/// name an MCP tool this surface never holds, so vanishing here would silently
/// change what the definition means depending on how it was launched. Only a
/// `deny` or `ask` reaches this branch — an unmatched `allow` is rejected by
/// the subset invariant before normalization — so a retained rule can narrow
/// and never widen.
fn normalize_declared_tool(declaration: PermissionRule, surface: &[&str]) -> Vec<PermissionRule> {
    let expanded = surface
        .iter()
        .filter(|tool| declaration_names_tool(&declaration, tool))
        .map(|tool| PermissionRule {
            tool: PermissionPattern::Exact((*tool).to_owned()),
            target: retarget_for_tool(&declaration.target, tool),
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
    use agens_core::{
        PermissionMode, PermissionPolicy, PermissionRequest, PermissionSession, ToolAccess,
    };

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
        let surface = resolve_child_surface(&[], &[], &[]).unwrap();

        assert_eq!(
            tool_names(&surface),
            child_native_metadata()
                .into_iter()
                .map(|entry| entry.qualified_name)
                .collect::<Vec<_>>()
        );
    }

    /// Moving the session is the session's own decision. A child that could
    /// move it would move a directory the delegating thread never named, and
    /// would move it where that thread's footer and audit log cannot follow.
    #[test]
    fn a_child_never_holds_the_tools_that_move_the_session() {
        let surface = resolve_child_surface(&[], &[], &[]).unwrap();

        for tool in agens_tools::SESSION_SCOPED_NATIVE_TOOLS {
            assert!(!tool_names(&surface).contains(&tool.to_owned()));
        }
    }

    #[test]
    fn a_declared_deny_omits_its_tool_from_the_catalog() {
        let surface =
            resolve_child_surface(&[], &[rule(PermissionDecision::Deny, "bash")], &[]).unwrap();

        assert!(!tool_names(&surface).contains(&"native::bash".to_owned()));
        assert_eq!(
            tool_names(&surface).len(),
            child_native_metadata().len() - 1
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
            &[],
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
        let rejection = resolve_child_surface(
            &[],
            &[rule(PermissionDecision::Allow, "not_a_real_tool")],
            &[],
        )
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
                let surface = resolve_child_surface(&[], &[rule(decision, tool)], &[])
                    .unwrap_or_else(|error| panic!("{decision:?} {tool} must resolve: {error:?}"));

                assert_eq!(
                    tool_names(&surface).len(),
                    child_native_metadata().len(),
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

    /// The read-class tools carry a derived `allow`, which a declaration has to
    /// be able to narrow. Precedence decides that, not the position the two
    /// rules end up in.
    #[test]
    fn a_declared_narrowing_outranks_the_derived_allow_for_a_read_class_tool() {
        let declared = PermissionRule::global(
            PermissionDecision::Deny,
            PermissionPattern::glob("read").unwrap(),
            PermissionPattern::glob(".env*").unwrap(),
        );
        let surface = resolve_child_surface(&[], &[declared], &[]).unwrap();
        let policy = PermissionPolicy::new(PermissionMode::Edit, surface.rules);
        let decision = |target| {
            policy.evaluate(
                &PermissionRequest::new("project", "native::read", target, ToolAccess::ReadOnly),
                &[],
                &PermissionSession::new(),
            )
        };

        assert_eq!(decision(".env"), PermissionDecision::Deny);
        assert_eq!(decision("notes.md"), PermissionDecision::Allow);
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
        let surface = resolve_child_surface(&[parent_deny("bash", None)], &[], &[])
            .expect("surface must resolve");

        assert!(!tool_names(&surface).contains(&"native::bash".to_owned()));
    }

    #[test]
    fn a_declared_allow_cannot_reopen_a_tool_the_configuration_denies() {
        let rejection = resolve_child_surface(
            &[parent_deny("bash", None)],
            &[rule(PermissionDecision::Allow, "bash")],
            &[],
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
    fn a_targeted_configured_deny_outranks_a_declared_allow_in_the_child() {
        let surface = resolve_child_surface(
            &[parent_deny("bash", Some("rm*"))],
            &[rule(PermissionDecision::Allow, "bash")],
            &[],
        )
        .expect("a targeted configured deny must not reject the delegation");

        assert!(tool_names(&surface).contains(&"native::bash".to_owned()));
        assert_eq!(
            child_decision(&surface, "rm -rf /tmp/x"),
            PermissionDecision::Deny
        );
        assert_eq!(
            child_decision(&surface, "echo hi"),
            PermissionDecision::Allow
        );
    }

    /// A configured `ask` is not a denial, so it never omits a tool — it has to
    /// reach the child as a decision, where the unreachable prompt is what
    /// turns it into a refusal.
    #[test]
    fn a_configured_ask_survives_a_declared_allow_in_the_child() {
        let ask = PermissionRule::global(
            PermissionDecision::Ask,
            PermissionPattern::Exact("native::bash".into()),
            PermissionPattern::glob_for_target_kind(
                "git push*",
                permission_target_kind_for_tool("bash"),
            )
            .unwrap(),
        );
        let surface =
            resolve_child_surface(&[ask], &[rule(PermissionDecision::Allow, "bash")], &[])
                .expect("a configured ask must not reject the delegation");

        assert!(tool_names(&surface).contains(&"native::bash".to_owned()));
        assert_eq!(
            child_decision(&surface, "git push origin main"),
            PermissionDecision::Ask
        );
        assert_eq!(
            child_decision(&surface, "echo hi"),
            PermissionDecision::Allow
        );
    }

    /// Resolves a `bash` call the way a delegated child does, floor included.
    fn child_decision(surface: &ChildToolSurface, command: &str) -> PermissionDecision {
        PermissionPolicy::new(PermissionMode::Edit, surface.rules.clone())
            .with_configured_floor(surface.configured_floor.clone())
            .evaluate(
                &PermissionRequest::new("project", "native::bash", command, ToolAccess::Write),
                &[],
                &PermissionSession::new(),
            )
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
            &[],
        )
        .expect("a configured carve-out must not reject the delegation");

        assert!(
            tool_names(&surface).contains(&"native::bash".to_owned()),
            "a tool the configuration still allows for some target must stay in the catalog"
        );
    }

    fn authorizes(surface: &ChildToolSurface, tool: &str) -> bool {
        surface
            .rules
            .iter()
            .any(|rule| rule.decision == PermissionDecision::Allow && rule.tool.matches(tool))
    }

    /// A subagent nobody narrowed can do the work it was delegated. This is the
    /// whole of the default: an agent definition that says nothing about
    /// permissions gets the surface, because the alternative is a role that
    /// reads files, calls `bash`, and is refused for a prompt no delegated
    /// execution can ever display.
    #[test]
    fn an_undeclared_child_is_authorized_for_every_tool_it_holds() {
        let surface = resolve_child_surface(&[], &[], &[]).unwrap();

        for tool in ["native::bash", "native::write", "native::edit"] {
            assert!(
                authorizes(&surface, tool),
                "a child with no declarations must be able to use {tool}"
            );
        }
        for tool in CHILD_NON_CATALOG_TOOLS {
            assert!(authorizes(&surface, tool));
        }
    }

    /// The one tool whose reach leaves the machine. It follows the same default
    /// as the rest — `ToolAccess` classifies worktree impact, not egress, so
    /// nothing here distinguishes it — which means an unattended subagent can
    /// make network requests unless a declaration says otherwise. Pinned so
    /// that stays a decision somebody made rather than a detail that drifted.
    #[test]
    fn an_undeclared_child_may_reach_the_network_and_a_declaration_is_what_stops_it() {
        assert!(authorizes(
            &resolve_child_surface(&[], &[], &[]).unwrap(),
            "native::webfetch"
        ));

        let declared =
            resolve_child_surface(&[], &[rule(PermissionDecision::Deny, "webfetch")], &[])
                .expect("surface must resolve");

        assert!(
            !authorizes(&declared, "native::webfetch"),
            "a declared deny must still take the network away"
        );
        assert!(
            !tool_names(&declared).contains(&"native::webfetch".to_owned()),
            "and take it out of the catalog, so the model is not offered it at all"
        );
    }

    /// A definition that enumerates what it may run keeps its enumeration. The
    /// blanket authorization exists for definitions that said nothing; adding
    /// it beside an `allow bash git*` would not narrow that agent to git, it
    /// would hand it unrestricted `bash` as a reward for having been careful.
    #[test]
    fn a_declared_allow_is_not_widened_by_the_blanket_authorization() {
        let surface = resolve_child_surface(
            &[],
            &[PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("bash".into()),
                PermissionPattern::glob("git*").expect("valid target glob"),
            )],
            &[],
        )
        .expect("surface must resolve");

        assert!(
            !surface.rules.iter().any(|rule| {
                rule.decision == PermissionDecision::Allow
                    && rule.tool.matches("native::bash")
                    && rule.target == PermissionPattern::Any
            }),
            "the scoped allow must not be joined by an unscoped one"
        );
        assert!(
            authorizes(&surface, "native::write"),
            "scoping one tool must not disturb the tools the definition left alone"
        );
    }

    fn engram_tools() -> Vec<String> {
        vec![
            "engram::mem_search".to_owned(),
            "engram::mem_save".to_owned(),
        ]
    }

    /// An MCP tool is a tool the child holds, so it follows the same default as
    /// a native one.
    #[test]
    fn remote_tools_the_parent_connected_are_authorized_for_the_child_too() {
        let surface = resolve_child_surface(&[], &[], &engram_tools()).unwrap();

        assert!(authorizes(&surface, "engram::mem_save"));
        assert!(
            surface
                .remote_tools
                .contains(&"engram::mem_save".to_owned())
        );
    }

    /// And the config can say otherwise. `remote_tools` is what the child's
    /// runtime offers, so a denied server tool has to leave that list, not
    /// merely lose its authorization — a tool the model is shown and then
    /// refused for is a worse answer than one it was never shown.
    #[test]
    fn a_denied_remote_tool_is_not_offered_to_the_child_at_all() {
        let surface = resolve_child_surface(
            &[],
            &[rule(PermissionDecision::Deny, "engram::mem_save")],
            &engram_tools(),
        )
        .expect("a deny naming a connected remote tool must resolve");

        assert!(
            !surface
                .remote_tools
                .contains(&"engram::mem_save".to_owned())
        );
        assert!(!authorizes(&surface, "engram::mem_save"));
        assert!(
            surface
                .remote_tools
                .contains(&"engram::mem_search".to_owned()),
            "denying one remote tool must leave the rest of the server alone"
        );
    }

    /// A definition may scope its remote access — which is only possible
    /// because the identities reach the surface before declarations are
    /// checked. Resolved after the fact, this `allow` was rejected outright as
    /// naming a tool the surface did not have.
    #[test]
    fn a_definition_can_name_the_remote_tools_it_may_call() {
        let declared = PermissionRule::global(
            PermissionDecision::Allow,
            PermissionPattern::Exact("engram::mem_search".into()),
            PermissionPattern::Any,
        );
        let surface = resolve_child_surface(&[], std::slice::from_ref(&declared), &engram_tools())
            .expect("a declaration naming a connected remote tool must resolve");

        let granted = surface
            .rules
            .iter()
            .filter(|rule| {
                rule.decision == PermissionDecision::Allow
                    && rule.tool.matches("engram::mem_search")
            })
            .count();

        assert_eq!(
            granted, 1,
            "the declared allow must stand alone, not be joined by a derived one"
        );
        assert!(
            authorizes(&surface, "engram::mem_save"),
            "a remote tool the definition never named still follows the default"
        );
    }

    /// The configured floor outranks the blanket authorization, which is the
    /// property that keeps `[permissions]` meaningful now that a child
    /// authorizes itself: a tool the configuration denies outright is gone from
    /// the catalog, not merely allowed-then-overruled.
    #[test]
    fn the_configured_floor_still_outranks_what_the_child_authorizes_itself() {
        let surface = resolve_child_surface(&[parent_deny("bash", None)], &[], &[])
            .expect("surface must resolve");

        assert!(!authorizes(&surface, "native::bash"));
        assert!(!tool_names(&surface).contains(&"native::bash".to_owned()));
    }
}

//! One case table pinning permission-rule precedence to a single answer.
//!
//! The same declaration set is resolved twice — once through the delegated
//! child's surface (`resolve_child_surface`) and once through the primary
//! agent's capability set (`EffectiveCapabilitySet`) — and both must reach the
//! identical decision. The two paths reach `PermissionPolicy` by different
//! routes and previously disagreed; this table is what keeps them from
//! drifting apart again.
//!
//! Only tools that carry no derived grant are used as subjects. A delegated
//! child auto-authorizes the read-class natives, which is a child-scoped
//! grant rather than a precedence rule, so `read`/`grep`/`list` and friends
//! would differ between the paths for reasons that have nothing to do with
//! precedence.
//!
//! [`CONFIGURED_CASES`] extends the same comparison to the parent's configured
//! `[permissions]` block, which reaches the two paths by different routes again
//! and has to land on one answer for the same reason.

use std::fs;
use std::sync::{Arc, Mutex};

use agens_config::{ConfigPermissionDecision, ConfigPermissionRule, ConfigPermissionScope};
use agens_core::{
    AgentDefinition, PermissionDecision, PermissionMode, PermissionPolicy, PermissionRequest,
    PermissionRule, PermissionSession, ToolAccess,
};
use agens_permissions::{configured_permission_rules, permission_policy};
use agens_tool_runtime::child_catalog::resolve_child_surface;
use agens_tools::{
    AgentCatalog, DispatchTool, EffectiveCapabilitySet, NativeToolCatalog, ToolDispatcher,
    ToolExecutionContext, ToolOutput,
};

struct Case {
    declarations: &'static [&'static str],
    tool: &'static str,
    target: &'static str,
    expected: PermissionDecision,
}

const CASES: &[Case] = &[
    Case {
        declarations: &["allow bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["deny bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["ask bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Ask,
    },
    Case {
        declarations: &[],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Ask,
    },
    // "bash, except these": the broad allow trailing the narrow denies must
    // not overtake them.
    Case {
        declarations: &["deny bash rm*", "allow bash"],
        tool: "bash",
        target: "rm -rf victim.txt",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "deny bash curl*", "allow bash"],
        tool: "bash",
        target: "rm -rf /tmp/victim.txt",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "deny bash curl*", "allow bash"],
        tool: "bash",
        target: "curl https://example.invalid",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "deny bash curl*", "allow bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Allow,
    },
    // A narrower tool pattern outranks a broader one on an equal target.
    Case {
        declarations: &["allow *", "deny bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow bash", "deny *"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Deny,
    },
    // On an equal target and an equal tool specificity, `deny` wins in either
    // authoring order: declaration order never decides safety.
    Case {
        declarations: &["deny *", "allow *"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow *", "deny *"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow bash", "deny bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Deny,
    },
    // `ask` sits between `allow` and `deny`: the more restrictive decision
    // wins a tie, and requiring a human is more restrictive than granting.
    Case {
        declarations: &["ask bash", "allow bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Ask,
    },
    Case {
        declarations: &["ask bash", "deny bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Deny,
    },
    // Two globs of different breadth are equally specific, so the deny holds
    // whichever side of the allow it is written on.
    Case {
        declarations: &["deny bash rm*", "allow bash *"],
        tool: "bash",
        target: "rm -rf victim.txt",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow bash *", "deny bash rm*"],
        tool: "bash",
        target: "rm -rf victim.txt",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "allow bash *"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["deny write src/secret/**", "allow write src/**"],
        tool: "write",
        target: "src/secret/key.txt",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow write src/**", "deny write src/secret/**"],
        tool: "write",
        target: "src/secret/key.txt",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny write src/secret/**", "allow write src/**"],
        tool: "write",
        target: "src/main.rs",
        expected: PermissionDecision::Allow,
    },
    // "deny X except for these": an untargeted deny must not erase a targeted
    // allow on one path while the other honors it.
    Case {
        declarations: &["deny bash", "allow bash git*"],
        tool: "bash",
        target: "git status",
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["allow bash git*", "deny bash"],
        tool: "bash",
        target: "git status",
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["deny bash", "allow bash git*"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow write src/**", "deny write"],
        tool: "write",
        target: "src/main.rs",
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["deny write", "allow write src/**"],
        tool: "write",
        target: "src/main.rs",
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["allow write src/**", "deny write"],
        tool: "write",
        target: "README.md",
        expected: PermissionDecision::Deny,
    },
    // A targeted deny outranks an untargeted allow even when the allow names
    // the tool exactly and the deny only matches it by wildcard.
    Case {
        declarations: &["deny bas* rm*", "allow bash"],
        tool: "bash",
        target: "rm -rf /tmp/victim.txt",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow *", "deny bash rm*"],
        tool: "bash",
        target: "rm -rf victim.txt",
        expected: PermissionDecision::Deny,
    },
    // The same shape on a path-shaped tool, whose target glob keeps segment
    // discipline.
    Case {
        declarations: &["deny write .env*", "allow write"],
        tool: "write",
        target: ".env",
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny write .env*", "allow write"],
        tool: "write",
        target: "src/main.rs",
        expected: PermissionDecision::Allow,
    },
    // A declaration matching no tool decides nothing and rejects nothing.
    Case {
        declarations: &["deny zz*"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Ask,
    },
    Case {
        declarations: &["deny webfetc", "allow bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Allow,
    },
];

struct ConfiguredCase {
    configured: &'static [&'static str],
    declarations: &'static [&'static str],
    tool: &'static str,
    target: &'static str,
    expected: PermissionDecision,
}

/// The parent's configured `[permissions]` block is a floor: a declaration can
/// narrow it further but can never reopen what it nets to `Deny`, on either
/// path. The configured rules are resolved among themselves first, so a
/// configured `allow` can still carve an exception out of a configured `deny`.
const CONFIGURED_CASES: &[ConfiguredCase] = &[
    ConfiguredCase {
        configured: &[],
        declarations: &["allow bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Allow,
    },
    ConfiguredCase {
        configured: &["deny bash"],
        declarations: &[],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Deny,
    },
    // The shape the two paths used to answer oppositely: an untargeted
    // configured deny against a targeted declared allow.
    ConfiguredCase {
        configured: &["deny bash"],
        declarations: &["allow bash git*"],
        tool: "bash",
        target: "git status",
        expected: PermissionDecision::Deny,
    },
    ConfiguredCase {
        configured: &["deny write"],
        declarations: &["allow write src/**"],
        tool: "write",
        target: "src/main.rs",
        expected: PermissionDecision::Deny,
    },
    // A targeted configured deny leaves everything it does not name to the
    // declarations.
    ConfiguredCase {
        configured: &["deny bash rm*"],
        declarations: &["allow bash"],
        tool: "bash",
        target: "rm -rf victim.txt",
        expected: PermissionDecision::Deny,
    },
    ConfiguredCase {
        configured: &["deny bash rm*"],
        declarations: &["allow bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Allow,
    },
    // An equally targeted declared allow cannot reopen a configured deny.
    ConfiguredCase {
        configured: &["deny bash rm*"],
        declarations: &["allow bash rm*"],
        tool: "bash",
        target: "rm -rf victim.txt",
        expected: PermissionDecision::Deny,
    },
    // Nor can a strictly narrower one.
    ConfiguredCase {
        configured: &["deny write src/**"],
        declarations: &["allow write src/generated/**"],
        tool: "write",
        target: "src/generated/api.rs",
        expected: PermissionDecision::Deny,
    },
    // The configuration resolves against itself before any declaration sees
    // it, so a configured carve-out survives.
    ConfiguredCase {
        configured: &["deny bash", "allow bash git*"],
        declarations: &["allow bash"],
        tool: "bash",
        target: "git status",
        expected: PermissionDecision::Allow,
    },
    ConfiguredCase {
        configured: &["deny bash", "allow bash git*"],
        declarations: &["allow bash"],
        tool: "bash",
        target: "echo hi",
        expected: PermissionDecision::Deny,
    },
];

#[test]
fn the_child_path_and_the_parent_path_decide_every_configured_shape_identically() {
    let mut disagreements = Vec::new();

    for case in CONFIGURED_CASES {
        let declarations = parsed_declarations(case.declarations);
        let configured = configured_rules(case.configured);

        let child = configured_child_decision(&configured, &declarations, case.tool, case.target);
        let parent =
            configured_parent_decision(case.configured, &declarations, case.tool, case.target);

        if child != case.expected || parent != case.expected {
            disagreements.push(format!(
                "config {:?} + {:?} on {} {:?}: expected {:?}, child {child:?}, parent {parent:?}",
                case.configured, case.declarations, case.tool, case.target, case.expected
            ));
        }
    }

    assert!(
        disagreements.is_empty(),
        "{} of {} cases disagreed:\n{}",
        disagreements.len(),
        CONFIGURED_CASES.len(),
        disagreements.join("\n")
    );
}

/// Parses `decision tool [target]` into a configured `[permissions]` entry,
/// deliberately reusing the declaration spelling so a case reads as one rule
/// set written in two places.
fn configured_entries(entries: &[&str]) -> Vec<ConfigPermissionRule> {
    entries
        .iter()
        .map(|entry| {
            let mut parts = entry.split_whitespace();
            let decision = match parts.next() {
                Some("allow") => ConfigPermissionDecision::Allow,
                Some("deny") => ConfigPermissionDecision::Deny,
                Some("ask") => ConfigPermissionDecision::Ask,
                other => panic!("unsupported configured decision {other:?}"),
            };
            ConfigPermissionRule {
                scope: ConfigPermissionScope::Global,
                decision,
                tool_pattern: parts.next().expect("a configured rule names a tool").into(),
                target_pattern: parts.next().map(str::to_owned),
            }
        })
        .collect()
}

/// Resolves configured entries the way a delegated child does: the qualified
/// tool name is kept because the child's dispatcher does not exist yet.
fn configured_rules(entries: &[&str]) -> Vec<PermissionRule> {
    configured_permission_rules(&configured_entries(entries), "project", |configured| {
        Ok(agens_core::PermissionPattern::Exact(configured.to_owned()))
    })
    .expect("configured rules must resolve")
}

/// A resolution error means no child turn ever starts, which denies every call
/// the delegation would have made.
fn configured_child_decision(
    configured: &[PermissionRule],
    declarations: &[PermissionRule],
    tool: &str,
    target: &str,
) -> PermissionDecision {
    let Ok(surface) = resolve_child_surface(configured, declarations) else {
        return PermissionDecision::Deny;
    };
    let qualified = format!("native::{tool}");

    if !surface
        .tools
        .iter()
        .any(|entry| entry.qualified_name == qualified)
    {
        return PermissionDecision::Deny;
    }

    PermissionPolicy::with_safety_predicates(
        PermissionMode::Edit,
        surface.rules,
        surface.safety_predicates,
    )
    .evaluate(&request(&qualified, target), &[], &PermissionSession::new())
}

fn configured_parent_decision(
    configured: &[&str],
    declarations: &[PermissionRule],
    tool: &str,
    target: &str,
) -> PermissionDecision {
    let dispatcher = Arc::new(Mutex::new(native_dispatcher()));
    let mut agent = agent_definition(&[]);
    agent.permission_rules = declarations.to_vec();

    let capabilities = {
        let dispatcher = dispatcher.lock().expect("dispatcher must be available");
        EffectiveCapabilitySet::from_agent(&agent, "project", &dispatcher)
    };
    let identity = dispatcher
        .lock()
        .expect("dispatcher must be available")
        .canonical_identity(&format!("native::{tool}"))
        .expect("the probe dispatcher must hold the subject tool")
        .as_str()
        .to_owned();

    permission_policy(
        &configured_entries(configured),
        "project",
        PermissionMode::Edit,
        &dispatcher,
        Some(&capabilities),
    )
    .expect("the configured policy must resolve")
    .evaluate(&request(&identity, target), &[], &PermissionSession::new())
}

#[test]
fn the_child_path_and_the_parent_path_decide_every_declaration_shape_identically() {
    let mut disagreements = Vec::new();

    for case in CASES {
        let declarations = parsed_declarations(case.declarations);

        let child = child_decision(&declarations, case.tool, case.target);
        let parent = parent_decision(&declarations, case.tool, case.target);

        if child != case.expected || parent != case.expected {
            disagreements.push(format!(
                "{:?} on {} {:?}: expected {:?}, child {child:?}, parent {parent:?}",
                case.declarations, case.tool, case.target, case.expected
            ));
        }
    }

    assert!(
        disagreements.is_empty(),
        "{} of {} cases disagreed:\n{}",
        disagreements.len(),
        CASES.len(),
        disagreements.join("\n")
    );
}

/// Parses declarations through the real agent-markdown grammar, so both paths
/// consume exactly the rules an authored definition would produce.
fn parsed_declarations(declarations: &[&str]) -> Vec<PermissionRule> {
    agent_definition(declarations).permission_rules
}

fn agent_definition(declarations: &[&str]) -> AgentDefinition {
    let temporary = agens_fixtures::session_directory(&format!(
        "precedence-{:x}",
        declarations
            .iter()
            .flat_map(|entry| entry.bytes())
            .fold(0u64, |hash, byte| hash.wrapping_mul(1_099_511_628_211)
                ^ u64::from(byte))
    ));
    let global = temporary.join("global");
    let project = temporary.join("project");
    fs::create_dir_all(&global).unwrap();
    fs::create_dir_all(&project).unwrap();

    let permissions = declarations
        .iter()
        .map(|entry| format!("  - {entry}\n"))
        .collect::<String>();
    let body = if declarations.is_empty() {
        "---\nname: probe\ndescription: probe\nmode: all\n---\nbody\n".to_owned()
    } else {
        format!(
            "---\nname: probe\ndescription: probe\nmode: all\npermissions:\n{permissions}---\nbody\n"
        )
    };
    fs::write(global.join("probe.md"), body).unwrap();

    let discovery = AgentCatalog::discover(&[], &global, &project).unwrap();
    let definition = discovery
        .catalog()
        .agent("probe")
        .expect("the probe definition must load")
        .clone();

    fs::remove_dir_all(&temporary).unwrap();
    definition
}

/// Resolves a request the way a delegated child does: a tool the resolved
/// surface omits is unreachable, which the spec requires to surface as a
/// denial rather than as an unknown tool.
fn child_decision(declarations: &[PermissionRule], tool: &str, target: &str) -> PermissionDecision {
    let surface = resolve_child_surface(&[], declarations).expect("the child surface must resolve");
    let qualified = format!("native::{tool}");

    if !surface
        .tools
        .iter()
        .any(|entry| entry.qualified_name == qualified)
    {
        return PermissionDecision::Deny;
    }

    PermissionPolicy::new(PermissionMode::Edit, surface.rules).evaluate(
        &request(&qualified, target),
        &[],
        &PermissionSession::new(),
    )
}

/// Resolves the same request the way the primary path does, through the
/// dispatcher-backed capability set.
fn parent_decision(
    declarations: &[PermissionRule],
    tool: &str,
    target: &str,
) -> PermissionDecision {
    let dispatcher = native_dispatcher();
    let mut agent = agent_definition(&[]);
    agent.permission_rules = declarations.to_vec();

    let capabilities = EffectiveCapabilitySet::from_agent(&agent, "project", &dispatcher);
    let identity = dispatcher
        .canonical_identity(&format!("native::{tool}"))
        .expect("the probe dispatcher must hold the subject tool")
        .as_str()
        .to_owned();

    PermissionPolicy::new(PermissionMode::Edit, capabilities.permission_rules()).evaluate(
        &request(&identity, target),
        &[],
        &PermissionSession::new(),
    )
}

fn request(tool: &str, target: &str) -> PermissionRequest {
    PermissionRequest::new("project", tool, target, ToolAccess::Write)
}

/// A dispatcher holding exactly the natives a delegated child inherits, so
/// the two paths compare over the same tool surface.
fn native_dispatcher() -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new();
    for entry in NativeToolCatalog::metadata() {
        dispatcher
            .register_native(entry.qualified_name, entry.access, InertTool)
            .unwrap();
    }
    dispatcher
}

struct InertTool;

impl DispatchTool for InertTool {
    fn execute(
        &mut self,
        _: &ToolExecutionContext,
        _: serde_json::Value,
    ) -> Result<ToolOutput, agens_core::Error> {
        Ok(ToolOutput::success("unused"))
    }
}

//! One case table pinning permission-rule precedence to a single answer.
//!
//! The same declaration set is resolved twice — once through the delegated
//! child's surface (`resolve_child_surface`) and once through the primary
//! agent's capability set (`EffectiveCapabilitySet`) — and both must reach the
//! identical decision. The two paths reach `PermissionPolicy` by different
//! routes and previously disagreed; this table is what keeps them from
//! drifting apart again.
//!
//! Tools that carry a derived grant are used only where a rule refuses the
//! call. A delegated child auto-authorizes the read-class natives, which is a
//! child-scoped grant rather than a precedence rule, so a `read`/`grep`/`list`
//! call that no rule names lands on `Allow` in a child and on `Ask` on the
//! primary path — a difference that has nothing to do with precedence. A rule
//! that refuses has to hold on both.
//!
//! [`CONFIGURED_CASES`] extends the same comparison to the parent's configured
//! `[permissions]` block, which reaches the two paths by different routes again
//! and has to land on one answer for the same reason.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agens_config::{ConfigPermissionDecision, ConfigPermissionRule, ConfigPermissionScope};
use agens_core::{
    AgentDefinition, PermissionDecision, PermissionMode, PermissionPolicy, PermissionReach,
    PermissionReadFilter, PermissionRequest, PermissionRule, PermissionSession,
    PermissionTargetKind, ToolAccess, permission_target_kind_for_tool,
};
use agens_permissions::{NativePermissionTarget, configured_permission_rules, permission_policy};
use agens_tool_runtime::child_catalog::resolve_child_surface;
use agens_tools::{
    AgentCatalog, DispatchTool, EffectiveCapabilitySet, NativeToolCatalog, NativeTools,
    SkillResourceTool, TaskTool, ToolDispatchRequest, ToolDispatcher, ToolEvaluationOutcome,
    ToolExecutionContext, ToolOutput,
};

struct Case {
    declarations: &'static [&'static str],
    tool: &'static str,
    target: &'static str,
    /// The `path` argument of a search call, whose target holds the pattern
    /// instead. `None` for every other tool, and for a search that names no
    /// path at all — which is itself a case, since such a search reads the
    /// whole worktree.
    path: Option<&'static str>,
    expected: PermissionDecision,
}

const CASES: &[Case] = &[
    Case {
        declarations: &["allow bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["deny bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["ask bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Ask,
    },
    Case {
        declarations: &[],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Ask,
    },
    // "bash, except these": the broad allow trailing the narrow denies must
    // not overtake them.
    Case {
        declarations: &["deny bash rm*", "allow bash"],
        tool: "bash",
        target: "rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "deny bash curl*", "allow bash"],
        tool: "bash",
        target: "rm -rf /tmp/victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "deny bash curl*", "allow bash"],
        tool: "bash",
        target: "curl https://example.invalid",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "deny bash curl*", "allow bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Allow,
    },
    // A wildcard tool pattern resolves to the very tools it names on both
    // paths, so `*` and `bash` end up selecting the same call and the more
    // restrictive decision takes the tie — the tool patterns never compare as
    // one being narrower than the other.
    Case {
        declarations: &["allow *", "deny bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow bash", "deny *"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // Where two rules select the same call, `deny` wins in either authoring
    // order: declaration order never decides safety.
    Case {
        declarations: &["deny *", "allow *"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow *", "deny *"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow bash", "deny bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // `ask` sits between `allow` and `deny`: the more restrictive decision
    // wins a tie, and requiring a human is more restrictive than granting.
    Case {
        declarations: &["ask bash", "allow bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Ask,
    },
    Case {
        declarations: &["ask bash", "deny bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // `rm*` selects a strict subset of `*`, so the deny outranks the allow
    // whichever side of it the allow is written on.
    Case {
        declarations: &["deny bash rm*", "allow bash *"],
        tool: "bash",
        target: "rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow bash *", "deny bash rm*"],
        tool: "bash",
        target: "rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "allow bash *"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["deny write src/secret/**", "allow write src/**"],
        tool: "write",
        target: "src/secret/key.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow write src/**", "deny write src/secret/**"],
        tool: "write",
        target: "src/secret/key.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny write src/secret/**", "allow write src/**"],
        tool: "write",
        target: "src/main.rs",
        path: None,
        expected: PermissionDecision::Allow,
    },
    // "deny X except for these": an untargeted deny must not erase a targeted
    // allow on one path while the other honors it.
    Case {
        declarations: &["deny bash", "allow bash git*"],
        tool: "bash",
        target: "git status",
        path: None,
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["allow bash git*", "deny bash"],
        tool: "bash",
        target: "git status",
        path: None,
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["deny bash", "allow bash git*"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow write src/**", "deny write"],
        tool: "write",
        target: "src/main.rs",
        path: None,
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["deny write", "allow write src/**"],
        tool: "write",
        target: "src/main.rs",
        path: None,
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["allow write src/**", "deny write"],
        tool: "write",
        target: "README.md",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // A targeted deny outranks an untargeted allow even when the allow names
    // the tool exactly and the deny only matches it by wildcard.
    Case {
        declarations: &["deny bas* rm*", "allow bash"],
        tool: "bash",
        target: "rm -rf /tmp/victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow *", "deny bash rm*"],
        tool: "bash",
        target: "rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // The same shape on a path-shaped tool, whose target glob keeps segment
    // discipline.
    Case {
        declarations: &["deny write .env*", "allow write"],
        tool: "write",
        target: ".env",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny write .env*", "allow write"],
        tool: "write",
        target: "src/main.rs",
        path: None,
        expected: PermissionDecision::Allow,
    },
    // An explicitly spelled wildcard names exactly the calls an absent target
    // names, so the two are interchangeable and neither outranks the other.
    Case {
        declarations: &["deny bash", "allow bash *"],
        tool: "bash",
        target: "rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow bash *", "deny bash"],
        tool: "bash",
        target: "rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow bash", "deny bash *"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash *", "allow bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // `**` is the path-shaped spelling of the same breadth; `*` on a path stops
    // at a separator and is genuinely narrower, which the two rows after it pin.
    Case {
        declarations: &["deny write", "allow write **"],
        tool: "write",
        target: ".env",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow write **", "deny write"],
        tool: "write",
        target: ".env",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow write", "deny write *"],
        tool: "write",
        target: ".env",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["allow write", "deny write *"],
        tool: "write",
        target: "src/main.rs",
        path: None,
        expected: PermissionDecision::Allow,
    },
    // A `bash` rule names a command, and a shell expression runs several. A
    // deny holds when any of them matches, however the command was dressed up.
    Case {
        declarations: &["deny bash rm*", "allow bash"],
        tool: "bash",
        target: "cd /tmp && rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "allow bash"],
        tool: "bash",
        target: "/bin/rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "allow bash"],
        tool: "bash",
        target: "sudo rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "allow bash"],
        tool: "bash",
        target: "ls | xargs rm",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "allow bash"],
        tool: "bash",
        target: "bash -c \"rm -rf victim.txt\"",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "allow bash"],
        tool: "bash",
        target: "echo $(rm -rf victim.txt)",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny bash rm*", "allow bash"],
        tool: "bash",
        target: r"\rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // An allow names what it names: it authorizes a compound command only when
    // every part of it is authorized.
    Case {
        declarations: &["allow bash git*"],
        tool: "bash",
        target: "git status && rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Ask,
    },
    Case {
        declarations: &["allow bash git*"],
        tool: "bash",
        target: "git add . && git commit",
        path: None,
        expected: PermissionDecision::Allow,
    },
    // A path-shaped target is matched by what it names, not by the spelling the
    // caller happened to produce.
    Case {
        declarations: &["deny write .env*", "allow write"],
        tool: "write",
        target: "./.env",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny write src/secret/**", "allow write src/**"],
        tool: "write",
        target: "./src/./secret/key.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // A call on the directory a deny names is inside what the deny selects,
    // however the operator spelled the rule.
    Case {
        declarations: &["deny write src/secret/**", "allow write **"],
        tool: "write",
        target: "src/secret/",
        path: None,
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny write src/secret", "allow write **"],
        tool: "write",
        target: "src/secret/",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // A search is named by its pattern and reads whatever its path points at,
    // so a rule written against either axis selects it. Only the refusals sit
    // in this table: a child auto-authorizes `grep` while the primary agent
    // asks for it, so a search no rule names is the one shape the two paths
    // answer differently by design.
    Case {
        declarations: &["deny grep **/.env"],
        tool: "grep",
        target: "OPENAI_API_KEY",
        path: Some(".env"),
        expected: PermissionDecision::Deny,
    },
    Case {
        declarations: &["deny grep **/.env", "allow grep **"],
        tool: "grep",
        target: "OPENAI_API_KEY",
        path: Some(".env"),
        expected: PermissionDecision::Deny,
    },
    // A search of the whole worktree names no file, however that root is
    // spelled — and a search that names no root at all is that same call with
    // the argument left out. Neither can be decided on the root, so both run
    // and are decided file by file; see [`REPORTED_FILE_CASES`].
    Case {
        declarations: &["deny grep **/.env", "allow grep **"],
        tool: "grep",
        target: "OPENAI_API_KEY",
        path: Some("."),
        expected: PermissionDecision::Allow,
    },
    Case {
        declarations: &["deny grep **/.env", "allow grep **"],
        tool: "grep",
        target: "OPENAI_API_KEY",
        path: None,
        expected: PermissionDecision::Allow,
    },
    // The pattern axis keeps deciding what it always decided.
    Case {
        declarations: &["deny grep OPENAI*", "allow grep **"],
        tool: "grep",
        target: "OPENAI_API_KEY",
        path: Some("notes.md"),
        expected: PermissionDecision::Deny,
    },
    // A declaration matching no tool decides nothing and rejects nothing.
    Case {
        declarations: &["deny zz*"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Ask,
    },
    Case {
        declarations: &["deny webfetc", "allow bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Allow,
    },
];

struct ConfiguredCase {
    configured: &'static [&'static str],
    declarations: &'static [&'static str],
    tool: &'static str,
    target: &'static str,
    /// See [`Case::path`].
    path: Option<&'static str>,
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
        path: None,
        expected: PermissionDecision::Allow,
    },
    // Two selections that overlap without either containing the other. Neither
    // rule alone settles `config/.env`, so they tie and the more restrictive
    // decision takes it. This is the shape the shipped configuration's comment
    // documents, and a reader cannot derive it from the containment rule.
    ConfiguredCase {
        configured: &["deny read **/.env", "allow read config/**"],
        declarations: &[],
        tool: "read",
        target: "config/.env",
        path: None,
        expected: PermissionDecision::Deny,
    },
    ConfiguredCase {
        configured: &["deny read **/.env", "allow read config/**"],
        declarations: &[],
        tool: "read",
        target: "config/settings.toml",
        path: None,
        expected: PermissionDecision::Allow,
    },
    ConfiguredCase {
        configured: &["deny bash"],
        declarations: &[],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // The shape the two paths used to answer oppositely: an untargeted
    // configured deny against a targeted declared allow.
    ConfiguredCase {
        configured: &["deny bash"],
        declarations: &["allow bash git*"],
        tool: "bash",
        target: "git status",
        path: None,
        expected: PermissionDecision::Deny,
    },
    ConfiguredCase {
        configured: &["deny write"],
        declarations: &["allow write src/**"],
        tool: "write",
        target: "src/main.rs",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // A targeted configured deny leaves everything it does not name to the
    // declarations.
    ConfiguredCase {
        configured: &["deny bash rm*"],
        declarations: &["allow bash"],
        tool: "bash",
        target: "rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    ConfiguredCase {
        configured: &["deny bash rm*"],
        declarations: &["allow bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Allow,
    },
    // An equally targeted declared allow cannot reopen a configured deny.
    ConfiguredCase {
        configured: &["deny bash rm*"],
        declarations: &["allow bash rm*"],
        tool: "bash",
        target: "rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // Nor can a strictly narrower one.
    ConfiguredCase {
        configured: &["deny write src/**"],
        declarations: &["allow write src/generated/**"],
        tool: "write",
        target: "src/generated/api.rs",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // The configuration resolves against itself before any declaration sees
    // it, so a configured carve-out survives.
    ConfiguredCase {
        configured: &["deny bash", "allow bash git*"],
        declarations: &["allow bash"],
        tool: "bash",
        target: "git status",
        path: None,
        expected: PermissionDecision::Allow,
    },
    ConfiguredCase {
        configured: &["deny bash", "allow bash git*"],
        declarations: &["allow bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Deny,
    },
    // A configured `ask` is an approval the operator asked for. A declaration
    // cannot skip it, and it has to reach a delegated child too, where the
    // unreachable prompt is what turns it into a refusal.
    ConfiguredCase {
        configured: &["ask bash git push*"],
        declarations: &["allow bash"],
        tool: "bash",
        target: "git push origin main",
        path: None,
        expected: PermissionDecision::Ask,
    },
    ConfiguredCase {
        configured: &["ask bash git push*"],
        declarations: &["allow bash"],
        tool: "bash",
        target: "echo hi",
        path: None,
        expected: PermissionDecision::Allow,
    },
    ConfiguredCase {
        configured: &["ask bash"],
        declarations: &["allow bash"],
        tool: "bash",
        target: "rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Ask,
    },
    ConfiguredCase {
        configured: &["ask write"],
        declarations: &["allow write src/**"],
        tool: "write",
        target: "src/main.rs",
        path: None,
        expected: PermissionDecision::Ask,
    },
    // The other half of "a declaration can narrow the configured floor
    // further": a declared deny holds against a configured allow, however
    // narrowly the configured rule was written.
    ConfiguredCase {
        configured: &["allow bash rm*"],
        declarations: &["deny bash"],
        tool: "bash",
        target: "rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    ConfiguredCase {
        configured: &["allow bash"],
        declarations: &["deny bash rm*"],
        tool: "bash",
        target: "rm -rf victim.txt",
        path: None,
        expected: PermissionDecision::Deny,
    },
    ConfiguredCase {
        configured: &["allow write src/**"],
        declarations: &["ask write"],
        tool: "write",
        target: "src/main.rs",
        path: None,
        expected: PermissionDecision::Ask,
    },
    // The shape the shipped configuration is written in: a configured path
    // deny has to reach the tool that reports the lines it matched, on both
    // paths, and a declared `allow grep` must not reopen it.
    ConfiguredCase {
        configured: &["deny grep **/.env"],
        declarations: &[],
        tool: "grep",
        target: "OPENAI_API_KEY",
        path: Some(".env"),
        expected: PermissionDecision::Deny,
    },
    ConfiguredCase {
        configured: &["deny grep **/.env"],
        declarations: &["allow grep"],
        tool: "grep",
        target: "OPENAI_API_KEY",
        path: Some(".env"),
        expected: PermissionDecision::Deny,
    },
    ConfiguredCase {
        configured: &["deny grep **/.env"],
        declarations: &["allow grep"],
        tool: "grep",
        target: "OPENAI_API_KEY",
        path: None,
        expected: PermissionDecision::Allow,
    },
];

/// One authorized call plus one file whose contents that call would report.
///
/// The tables above decide calls. A call that reports the contents of a file
/// set it was not named by — a rooted search, a `git_read` diff — is a single
/// authorized call that reads many files, so what a rule naming one of those
/// files does cannot be written as a row there. It is written here instead, and
/// it has to come out the same on both paths for the same reason every other
/// row does.
struct ReportedFileCase {
    configured: &'static [&'static str],
    declarations: &'static [&'static str],
    /// The tool making the call. Its per-file question is asked under this name,
    /// so a rule has to be written against this tool to reach the files.
    tool: &'static str,
    /// The path argument the call was given, `None` when it named none. A
    /// `git_read` call has no path argument at all, so it is always `None`.
    root: Option<&'static str>,
    /// A file the call reaches, as it reaches it.
    file: &'static str,
    /// Whether what that file holds may reach the caller.
    reported: bool,
}

const REPORTED_FILE_CASES: &[ReportedFileCase] = &[
    // The shape the shipped configuration is written in, entered from above the
    // file it denies — the whole point of deciding per file.
    ReportedFileCase {
        configured: &["deny grep **/.env"],
        declarations: &[],
        tool: "grep",
        root: None,
        file: ".env",
        reported: false,
    },
    ReportedFileCase {
        configured: &["deny grep **/.env"],
        declarations: &[],
        tool: "grep",
        root: None,
        file: "notes.md",
        reported: true,
    },
    ReportedFileCase {
        configured: &["deny grep **/.env"],
        declarations: &[],
        tool: "grep",
        root: Some("."),
        file: "src/.env",
        reported: false,
    },
    ReportedFileCase {
        configured: &["deny grep **/.env"],
        declarations: &[],
        tool: "grep",
        root: Some("src"),
        file: "src/.env",
        reported: false,
    },
    ReportedFileCase {
        configured: &["deny grep **/.env"],
        declarations: &[],
        tool: "grep",
        root: Some("src"),
        file: "src/main.rs",
        reported: true,
    },
    // A declaration reaches the files a search reads exactly as a configured
    // rule does, and on a path that has nothing to do with `.env`.
    ReportedFileCase {
        configured: &[],
        declarations: &["deny grep src/secret/**", "allow grep **"],
        tool: "grep",
        root: Some("src"),
        file: "src/secret/key",
        reported: false,
    },
    ReportedFileCase {
        configured: &[],
        declarations: &["deny grep src/secret/**", "allow grep **"],
        tool: "grep",
        root: Some("src"),
        file: "src/main.rs",
        reported: true,
    },
    // An `ask` withholds beside a `deny`: the prompt that would settle it is
    // not reachable once the call it belongs to is already running.
    ReportedFileCase {
        configured: &[],
        declarations: &["ask grep src/secret/**", "allow grep **"],
        tool: "grep",
        root: Some("src"),
        file: "src/secret/key",
        reported: false,
    },
    // The narrower rule decides a file exactly as it decides a call. The root
    // here is the worktree, because a search rooted at `src` names a path the
    // broad deny selects and is refused outright — which is the correct answer
    // for a root a rule names, and a different question from this one.
    ReportedFileCase {
        configured: &[],
        declarations: &[
            "deny grep src/**",
            "allow grep src/generated/**",
            "allow grep **",
        ],
        tool: "grep",
        root: Some("."),
        file: "src/generated/schema.rs",
        reported: true,
    },
    ReportedFileCase {
        configured: &[],
        declarations: &[
            "deny grep src/**",
            "allow grep src/generated/**",
            "allow grep **",
        ],
        tool: "grep",
        root: Some("."),
        file: "src/main.rs",
        reported: false,
    },
    // The same axis for `git_read`, whose diff reports the contents of files
    // the call never named — its target is an operation keyword. A rule reaches
    // those files only by naming them, which is what these rows check, starting
    // with the shape the shipped configuration is written in.
    ReportedFileCase {
        configured: &["deny git_read **/.env"],
        declarations: &[],
        tool: "git_read",
        root: None,
        file: ".env",
        reported: false,
    },
    ReportedFileCase {
        configured: &["deny git_read **/.env"],
        declarations: &[],
        tool: "git_read",
        root: None,
        file: "src/.env",
        reported: false,
    },
    ReportedFileCase {
        configured: &["deny git_read **/.env"],
        declarations: &[],
        tool: "git_read",
        root: None,
        file: "notes.md",
        reported: true,
    },
    ReportedFileCase {
        configured: &[],
        declarations: &["deny git_read src/secret/**", "allow git_read **"],
        tool: "git_read",
        root: None,
        file: "src/secret/key",
        reported: false,
    },
    ReportedFileCase {
        configured: &[],
        declarations: &["deny git_read src/secret/**", "allow git_read **"],
        tool: "git_read",
        root: None,
        file: "src/main.rs",
        reported: true,
    },
    ReportedFileCase {
        configured: &[],
        declarations: &["ask git_read src/secret/**", "allow git_read **"],
        tool: "git_read",
        root: None,
        file: "src/secret/key",
        reported: false,
    },
    ReportedFileCase {
        configured: &[],
        declarations: &[
            "deny git_read src/**",
            "allow git_read src/generated/**",
            "allow git_read **",
        ],
        tool: "git_read",
        root: None,
        file: "src/generated/schema.rs",
        reported: true,
    },
    ReportedFileCase {
        configured: &[],
        declarations: &[
            "deny git_read src/**",
            "allow git_read src/generated/**",
            "allow git_read **",
        ],
        tool: "git_read",
        root: None,
        file: "src/main.rs",
        reported: false,
    },
];

/// The value a call is named by, held fixed per tool so every row is about the
/// file rather than about the target.
///
/// A search is named by a pattern no rule mentions. A `git_read` call is named
/// by an operation keyword, and `diff` is the one operation that reports what a
/// file holds — the others report paths, commit subjects or refs and have no
/// per-file question to ask.
fn call_target(tool: &str) -> &'static str {
    match tool {
        "grep" => "OPENAI_API_KEY",
        "git_read" => "diff",
        other => panic!("{other} does not report the contents of a file set"),
    }
}

/// What one path answers about a call that reads a file set: whether the call
/// runs at all, and whether one of the files it reaches may be reported.
fn reported_file_answer(
    policy: PermissionPolicy,
    identity: &str,
    tool: &str,
    root: Option<&str>,
    file: &str,
) -> (PermissionDecision, bool) {
    let call = policy.evaluate(
        &request(identity, tool, call_target(tool), root),
        &[],
        &PermissionSession::new(),
    );
    let permits = PermissionReadFilter::new(
        policy,
        Vec::new(),
        "project",
        identity,
        ToolAccess::ReadOnly,
    )
    .permits(file);

    (call, permits)
}

/// Compares one row across both paths, under one spelling of the path the call
/// was given and of the file it reaches.
fn reported_file_disagreements(
    case: &ReportedFileCase,
    root: Option<&str>,
    file: &str,
    spelling: &str,
) -> Vec<String> {
    let declarations = parsed_declarations(case.declarations);
    let configured = configured_rules(case.configured);
    let tool = case.tool;

    let (child_policy, child_identity) = configured_child_policy(&configured, &declarations, tool)
        .unwrap_or_else(|| panic!("a delegated child must be able to reach {tool}"));
    let (parent_policy, parent_identity) =
        configured_parent_policy(case.configured, &declarations, tool);

    let answers = [
        (
            "child",
            reported_file_answer(child_policy, &child_identity, tool, root, file),
        ),
        (
            "parent",
            reported_file_answer(parent_policy, &parent_identity, tool, root, file),
        ),
    ];

    answers
        .into_iter()
        .filter_map(|(path, (call, permits))| {
            let fault = if call == PermissionDecision::Deny {
                "refused the whole call instead of the file"
            } else if permits && !case.reported {
                "reported a file a rule names"
            } else if !permits && case.reported {
                "withheld a file no rule names"
            } else {
                return None;
            };

            Some(format!(
                "config {:?} + {:?} on {tool} given {root:?} over {file:?} spelled {spelling}: \
                 the {path} path {fault}",
                case.configured, case.declarations
            ))
        })
        .collect()
}

/// A call that reaches a file it may not report has to run and withhold that
/// file, on both paths. Refusing the call instead is the answer this table
/// exists to rule out: it would make every recursive search and every diff
/// useless under any configuration that denies one file.
#[test]
fn a_call_that_reads_a_file_set_reports_the_same_files_on_both_paths() {
    let disagreements = REPORTED_FILE_CASES
        .iter()
        .flat_map(|case| reported_file_disagreements(case, case.root, case.file, "as written"))
        .collect::<Vec<_>>();

    assert!(
        disagreements.is_empty(),
        "{} of {} file-set calls disagreed:\n{}",
        disagreements.len(),
        REPORTED_FILE_CASES.len(),
        disagreements.join("\n")
    );
}

/// The spelling axis reaches the per-file question too. A rule names a file,
/// and neither the spelling of that file nor the spelling of the path the call
/// was given may change what the call is allowed to report.
#[test]
fn every_spelling_of_a_file_a_call_reads_reports_the_same_files() {
    let mut disagreements = Vec::new();

    for case in REPORTED_FILE_CASES {
        for spelling in PATH_SPELLINGS {
            let root = case.root.map(|root| (spelling.rewrite)(root));
            let file = (spelling.rewrite)(case.file);

            disagreements.extend(reported_file_disagreements(
                case,
                root.as_deref(),
                &file,
                spelling.name,
            ));
        }
    }

    assert!(
        disagreements.is_empty(),
        "{} spellings of a file a call reads disagreed:\n{}",
        disagreements.len(),
        disagreements.join("\n")
    );
}

/// A rewrite of a path target into an equivalent spelling of the same file.
struct Spelling {
    name: &'static str,
    rewrite: fn(&str) -> String,
}

/// The spelling axis of the two tables above: a path names a file, and every
/// spelling of that file has to reach the same decision under every rule shape
/// already enumerated, on both paths.
///
/// This is a dimension rather than a handful of extra rows on purpose. A
/// spelling added here is checked against every path-shaped case in both
/// tables, so the cost of covering the next one is a single entry.
const PATH_SPELLINGS: &[Spelling] = &[
    Spelling {
        name: "as written",
        rewrite: str::to_owned,
    },
    Spelling {
        name: "doubled separators",
        rewrite: |target| target.replace('/', "//"),
    },
    Spelling {
        name: "tripled separators",
        rewrite: |target| target.replace('/', "///"),
    },
    Spelling {
        name: "leading ./",
        rewrite: |target| format!("./{target}"),
    },
    Spelling {
        name: "repeated leading ./",
        rewrite: |target| format!("././{target}"),
    },
    Spelling {
        name: "interior /./",
        rewrite: |target| target.replace('/', "/./"),
    },
    Spelling {
        name: "leading ./ over doubled separators",
        rewrite: |target| format!(".//{}", target.replace('/', "//")),
    },
    Spelling {
        name: "trailing /.",
        rewrite: |target| format!("{target}/."),
    },
];

#[test]
fn every_spelling_of_a_path_decides_the_same_on_both_paths() {
    let mut disagreements = Vec::new();

    for case in CASES.iter().filter(|case| is_path_shaped(case.tool)) {
        for spelling in PATH_SPELLINGS {
            let Some((target, path)) = respelled(case.tool, case.target, case.path, spelling)
            else {
                continue;
            };
            let path = path.as_deref();
            let declarations = parsed_declarations(case.declarations);

            let child = child_decision(&declarations, case.tool, &target, path);
            let parent = parent_decision(&declarations, case.tool, &target, path);

            if child != case.expected || parent != case.expected {
                disagreements.push(format!(
                    "{:?} on {} {:?} spelled {} as {target:?}/{path:?}: expected {:?}, \
                     child {child:?}, parent {parent:?}",
                    case.declarations, case.tool, case.target, spelling.name, case.expected
                ));
            }
        }
    }

    for case in CONFIGURED_CASES
        .iter()
        .filter(|case| is_path_shaped(case.tool))
    {
        for spelling in PATH_SPELLINGS {
            let Some((target, path)) = respelled(case.tool, case.target, case.path, spelling)
            else {
                continue;
            };
            let path = path.as_deref();
            let declarations = parsed_declarations(case.declarations);
            let configured = configured_rules(case.configured);

            let child =
                configured_child_decision(&configured, &declarations, case.tool, &target, path);
            let parent = configured_parent_decision(
                case.configured,
                &declarations,
                case.tool,
                &target,
                path,
            );

            if child != case.expected || parent != case.expected {
                disagreements.push(format!(
                    "config {:?} + {:?} on {} {:?} spelled {} as {target:?}/{path:?}: \
                     expected {:?}, child {child:?}, parent {parent:?}",
                    case.configured,
                    case.declarations,
                    case.tool,
                    case.target,
                    spelling.name,
                    case.expected
                ));
            }
        }
    }

    assert!(
        disagreements.is_empty(),
        "{} spellings disagreed:\n{}",
        disagreements.len(),
        disagreements.join("\n")
    );
}

/// A command line is spelled by its own axis — wrappers, separators and
/// quoting — which the tables above already enumerate; only path targets are
/// rewritten here.
fn is_path_shaped(tool: &str) -> bool {
    matches!(
        permission_target_kind_for_tool(tool),
        PermissionTargetKind::Path
    )
}

/// Applies a spelling to the path a case names: the `path` argument of a
/// search, or the target of every other path-shaped tool.
///
/// A search that names no path names no file to respell — its target is a
/// pattern, and the spelling axis has nothing to say about one — so it is left
/// out rather than rewritten into a case about something else.
fn respelled(
    tool: &str,
    target: &str,
    path: Option<&str>,
    spelling: &Spelling,
) -> Option<(String, Option<String>)> {
    match (path, is_search_tool(tool)) {
        (Some(path), _) => Some((target.to_owned(), Some((spelling.rewrite)(path)))),
        (None, false) => Some(((spelling.rewrite)(target), None)),
        (None, true) => None,
    }
}

#[test]
fn the_child_path_and_the_parent_path_decide_every_configured_shape_identically() {
    let mut disagreements = Vec::new();

    for case in CONFIGURED_CASES {
        let declarations = parsed_declarations(case.declarations);
        let configured = configured_rules(case.configured);

        let child = configured_child_decision(
            &configured,
            &declarations,
            case.tool,
            case.target,
            case.path,
        );
        let parent = configured_parent_decision(
            case.configured,
            &declarations,
            case.tool,
            case.target,
            case.path,
        );

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
/// set written in two places. A configured target is a single TOML string and
/// may contain spaces, so everything past the tool name is the target.
fn configured_entries(entries: &[&str]) -> Vec<ConfigPermissionRule> {
    entries
        .iter()
        .map(|entry| {
            let mut parts = entry.splitn(3, ' ');
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

/// A bare tool name written in `[permissions]` has to resolve to the tool it
/// names, for every native and not for some of them.
///
/// Only the caller resolving against a live dispatcher recovers from a name
/// left bare, and only because the dispatcher answers to both spellings. Every
/// other caller — a delegated child among them — is handed the name as written,
/// so a name that stays bare is one the qualification step did not do.
///
/// The names are read off the registered surface rather than off the catalog,
/// because the catalog is not the surface: five natives are registered beside
/// it, and a rule naming one of those has the same reason to bind.
#[test]
fn every_native_tool_named_in_configuration_resolves_to_the_tool_it_names() {
    let mut unresolved = Vec::new();

    for qualified in registered_native_surface() {
        let bare = qualified
            .strip_prefix("native::")
            .expect("a native tool name is qualified");
        let rules = configured_permission_rules(
            &configured_entries(&[&format!("deny {bare}")]),
            "project",
            |configured| Ok(agens_core::PermissionPattern::Exact(configured.to_owned())),
        )
        .expect("configured rules must resolve");

        if !rules[0].tool.matches(&qualified) {
            unresolved.push(bare.to_owned());
        }
    }

    assert!(
        unresolved.is_empty(),
        "these configured names never reach the tool they name: {unresolved:?}"
    );
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
    path: Option<&str>,
) -> PermissionDecision {
    let Some((policy, identity)) = configured_child_policy(configured, declarations, tool) else {
        return PermissionDecision::Deny;
    };

    policy.evaluate(
        &request(&identity, tool, target, path),
        &[],
        &PermissionSession::new(),
    )
}

/// The policy a delegated child runs a call to `tool` under, and the name it
/// holds that tool by. `None` when the child could never reach the tool at
/// all, which is a denial rather than a decision.
fn configured_child_policy(
    configured: &[PermissionRule],
    declarations: &[PermissionRule],
    tool: &str,
) -> Option<(PermissionPolicy, String)> {
    let surface = resolve_child_surface(configured, declarations).ok()?;
    let qualified = format!("native::{tool}");
    let reachable = surface
        .tools
        .iter()
        .any(|entry| entry.qualified_name == qualified)
        || surface
            .coordination_tools
            .iter()
            .any(|name| *name == qualified);

    reachable.then(|| {
        // A dispatched child call is decided against the dispatcher's own
        // identity string, and the dispatcher rewrites an `Exact` rule naming
        // either spelling of a registered tool to it first. Without that step
        // here a rule would read as inert that production honors.
        let policy = PermissionPolicy::new(PermissionMode::Edit, surface.rules)
            .with_configured_floor(surface.configured_floor)
            .normalized_tool_aliases(|name| {
                (name == tool || name == qualified).then(|| qualified.clone())
            });

        (policy, qualified)
    })
}

/// One rule, written in an agent definition and in the operator's
/// `[permissions]` block, has to decide the same call the same way.
///
/// The two spellings reach a child's policy by different routes — a declared
/// tool name is expanded against the child's surface, a configured one is
/// resolved against it — so a tool missing from whatever that surface
/// enumerates makes one spelling inert while the other binds. The coordination
/// tools are where that last happened: the child's runtime registers them
/// beside the catalog rather than from it.
///
/// All four routes are compared, not just the child's two. A child answers
/// `Deny` for a tool its resolved surface omits, so the child alone would
/// report a `deny` as binding whether or not the rule ever reached its policy —
/// the primary path holds every coordination tool unconditionally and has no
/// such shortcut, which is what makes the `deny` half falsifiable.
#[test]
fn a_declared_rule_and_a_configured_rule_decide_a_coordination_tool_identically() {
    let mut disagreements = Vec::new();

    for tool in ["task_control", "task_message"] {
        for (spelling, expected) in [
            ("deny", PermissionDecision::Deny),
            ("ask", PermissionDecision::Ask),
        ] {
            let entry = format!("{spelling} {tool}");
            let declarations = parsed_declarations(&[entry.as_str()]);
            let decisions = [
                (
                    "declared, child",
                    configured_child_decision(&[], &declarations, tool, "status", None),
                ),
                (
                    "configured, child",
                    configured_child_decision(
                        &configured_rules(&[entry.as_str()]),
                        &[],
                        tool,
                        "status",
                        None,
                    ),
                ),
                (
                    "declared, primary",
                    parent_decision(&declarations, tool, "status", None),
                ),
                (
                    "configured, primary",
                    configured_parent_decision(&[entry.as_str()], &[], tool, "status", None),
                ),
            ];

            for (route, decision) in decisions {
                if decision != expected {
                    disagreements.push(format!(
                        "{entry} ({route}): expected {expected:?}, got {decision:?}"
                    ));
                }
            }
        }
    }

    assert!(
        disagreements.is_empty(),
        "a rule that binds written one way must bind written the other, on both paths:\n{}",
        disagreements.join("\n")
    );
}

fn configured_parent_decision(
    configured: &[&str],
    declarations: &[PermissionRule],
    tool: &str,
    target: &str,
    path: Option<&str>,
) -> PermissionDecision {
    let (policy, identity) = configured_parent_policy(configured, declarations, tool);

    policy.evaluate(
        &request(&identity, tool, target, path),
        &[],
        &PermissionSession::new(),
    )
}

/// The same, for the primary path: the policy a dispatcher-backed capability
/// set produces, and the dispatcher identity it holds the tool by.
fn configured_parent_policy(
    configured: &[&str],
    declarations: &[PermissionRule],
    tool: &str,
) -> (PermissionPolicy, String) {
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

    let policy = permission_policy(
        &configured_entries(configured),
        "project",
        PermissionMode::Edit,
        &dispatcher,
        Some(&capabilities),
    )
    .expect("the configured policy must resolve");

    (policy, identity)
}

/// Both paths must read a declaration the same way. The one place they are
/// allowed to part is where no declaration reads at all: the parent turns an
/// undecided call into a question, and a delegated execution has no surface to
/// put a question on, so the child decides it instead of stalling on a prompt
/// nobody can answer.
///
/// That exemption is deliberately narrow — it applies only where the parent
/// itself reached `Ask`, and only to turn it into `Allow`. Any other
/// divergence, in either direction, is the two paths disagreeing about what a
/// rule means, which is the thing this table exists to catch.
#[test]
fn the_child_path_and_the_parent_path_decide_every_declaration_shape_identically() {
    let mut disagreements = Vec::new();
    let mut undecided = 0usize;

    for case in CASES {
        let declarations = parsed_declarations(case.declarations);

        let child = child_decision(&declarations, case.tool, case.target, case.path);
        let parent = parent_decision(&declarations, case.tool, case.target, case.path);

        if parent == case.expected
            && parent == PermissionDecision::Ask
            && child == PermissionDecision::Allow
        {
            undecided += 1;
            continue;
        }

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
    assert!(
        undecided > 0,
        "the table must still cover calls no declaration decides, or the one sanctioned \
         difference between the two paths is going untested"
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
fn child_decision(
    declarations: &[PermissionRule],
    tool: &str,
    target: &str,
    path: Option<&str>,
) -> PermissionDecision {
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
        &request(&qualified, tool, target, path),
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
    path: Option<&str>,
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
        &request(&identity, tool, target, path),
        &[],
        &PermissionSession::new(),
    )
}

/// Builds the request a case describes. `identity` is the tool name the
/// evaluating path holds — a qualified name in a child, a dispatcher identity
/// on the primary path — while `tool` is the bare name the case is written
/// against, which is what says whether the target is a search pattern.
fn request(identity: &str, tool: &str, target: &str, path: Option<&str>) -> PermissionRequest {
    PermissionRequest::reaching(
        "project",
        identity,
        target,
        ToolAccess::Write,
        &search_reach(tool, target, path),
    )
}

/// Projects a case's arguments the way a dispatched call does, through the
/// production target parser, so a row carries the same axes a real call carries
/// rather than a hand-built approximation of them.
///
/// Only a search names a path beside the target it is named by. A `git_read`
/// call is named by an operation and names no path at all, so what its diff may
/// report is settled per file rather than by anything the call reached for.
fn search_reach(tool: &str, target: &str, path: Option<&str>) -> Vec<PermissionReach> {
    let arguments = match tool {
        "grep" => {
            let mut arguments = serde_json::Map::from_iter([("pattern".to_owned(), target.into())]);
            if let Some(path) = path {
                arguments.insert("path".to_owned(), path.into());
            }
            arguments
        }
        "git_read" => {
            assert!(path.is_none(), "a git_read call names no path");
            serde_json::Map::from_iter([("operation".to_owned(), target.into())])
        }
        _ => {
            assert!(
                path.is_none(),
                "only a search names a path beside its target"
            );
            return Vec::new();
        }
    };

    NativePermissionTarget::parse(
        &format!("native::{tool}"),
        &serde_json::Value::Object(arguments),
    )
    .expect("a dispatched call must parse")
    .reach()
}

/// Whether a case's target is a search pattern rather than the file it reads.
fn is_search_tool(tool: &str) -> bool {
    tool == "grep"
}

/// The call that proves a tool asks the rules about each file it reports.
///
/// A row cannot claim the per-file question in prose: the claim is this call,
/// executed against a worktree holding a denied secret under a rule that denies
/// it. A tool wired to nothing runs the call and prints the secret.
struct PerFileProbe {
    /// The arguments of one call that reaches the denied file.
    arguments: &'static str,
    /// The target of the rule denying that file, so a row picks a file its own
    /// tool actually reaches rather than inheriting one that may not exist for
    /// it.
    denies: &'static str,
    /// A line the same call must still return, so a tool cannot pass the probe
    /// by returning nothing at all.
    still_reports: &'static str,
}

/// The call that proves a rule naming a file selects the call that would open
/// it, for a tool whose own target is not that file.
///
/// A tool can report what a call reaches beyond the target it is named by, and
/// what it reports folds into the same act a rule is matched against — which is
/// how a rule naming a file reaches a call named by something else. Where that
/// covers only part of what the tool can return, the row keeps its `unbound`
/// reason for the rest and carries this to keep the covered part from being
/// prose.
struct ReachProbe {
    /// The arguments of one call whose reach a rule can name.
    arguments: &'static str,
    /// A rule target naming the file that call would open, which must deny it.
    denied_by: &'static str,
    /// A rule target naming some other file, which must leave the call
    /// unselected — otherwise a tool reporting every path in the worktree would
    /// pass by denying everything.
    unmatched_by: &'static str,
}

/// One native tool, classified by the facts that decide whether a path deny
/// binds it.
struct SurfaceEntry {
    tool: &'static str,
    /// Whether the tool can return what a file holds, as opposed to its name.
    returns_file_contents: bool,
    /// Whether the path the call is given is the target a rule is matched
    /// against, so a deny refuses the call outright.
    decided_on_its_target: bool,
    /// The call that reports files it never named, present when the tool asks
    /// the same rules once per file while it runs.
    decided_per_file: Option<PerFileProbe>,
    /// The call that proves a rule naming a file reaches this tool for the part
    /// of what it can return that a path can name. Present only where that
    /// coverage is partial: a row it covers entirely is `decided_on_its_target`
    /// instead, and a row it covers not at all carries `unbound` alone.
    partly_reached: Option<ReachProbe>,
    /// Why no path rule reaches what this tool can return. Required of exactly
    /// the rows neither mechanism fully covers, and forbidden of the rest, so an
    /// exception is written down rather than inferred from an absence. A row
    /// carrying a [`ReachProbe`] states here what that probe leaves over.
    unbound: Option<&'static str>,
    /// The shape a rule's target is matched under, which decides whether a bare
    /// `*` crosses a `/`. `None` where the target is neither a path nor a
    /// command line, so the question does not arise.
    matched_as: Option<PermissionTargetKind>,
}

/// The whole registered native surface, classified.
///
/// **Scope.** This table covers every native tool a production dispatcher
/// registers, in any configuration, and nothing else. It does not cover the
/// remote tools an MCP server supplies, which share the primary dispatcher with
/// these and are the one other thing on it that can return the contents of a
/// file. That omission is a decision, not an oversight: a remote tool's
/// arguments are defined by the server serving it, so nothing here can say
/// which file a given call would read, and there is no path to decide. The
/// whole class is therefore reached by no rule written against a path, which
/// [`no_path_rule_reaches_an_mcp_tool_and_naming_the_tool_refuses_every_call`]
/// executes and [`every_native_tool_that_returns_file_contents_is_reached_by_a_path_rule`]
/// keeps outside this table on purpose rather than by omission.
///
/// This table exists because the enumeration is what keeps being got wrong: a
/// tool that returns the contents of a file and is decided neither way is a
/// path deny that does not bind, and repeated passes over this surface each
/// missed one. Adding a native tool without adding a row here fails the test
/// below, which is the point — the classification is the decision, and it has
/// to be made deliberately rather than inherited.
///
/// **What is proven and what is claimed.** `decided_per_file` is executed: the
/// row carries the call that proves it, run by
/// [`every_tool_this_table_says_asks_per_file_withholds_a_denied_file`], and a
/// row cannot assert the per-file question without supplying it.
/// `partly_reached` is executed the same way, by
/// [`every_tool_this_table_says_a_rule_reaches_past_its_target_is_denied_by_naming_the_file`].
/// `matched_as` is checked against `permission_target_kind_for_tool`. The tool
/// list is read off the production dispatchers rather than restated, across
/// both of the configurations that change it.
/// `returns_file_contents`, `decided_on_its_target` and `unbound` are CLAIMS,
/// asserted against each other but against no execution: a row could assert
/// `decided_on_its_target` for a tool whose target parses to something other
/// than a path and nothing here would contradict it. What each rule then
/// decides is pinned separately, by [`REPORTED_FILE_CASES`] and by the
/// shipped-configuration turns in `child.rs`.
const TOOL_SURFACE: &[SurfaceEntry] = &[
    SurfaceEntry {
        tool: "native::read",
        returns_file_contents: true,
        decided_on_its_target: true,
        decided_per_file: None,
        partly_reached: None,
        unbound: None,
        matched_as: Some(PermissionTargetKind::Path),
    },
    SurfaceEntry {
        tool: "native::write",
        returns_file_contents: false,
        decided_on_its_target: true,
        decided_per_file: None,
        partly_reached: None,
        unbound: None,
        matched_as: Some(PermissionTargetKind::Path),
    },
    // `edit` reports the region it rewrote, which is the file's own text.
    SurfaceEntry {
        tool: "native::edit",
        returns_file_contents: true,
        decided_on_its_target: true,
        decided_per_file: None,
        partly_reached: None,
        unbound: None,
        matched_as: Some(PermissionTargetKind::Path),
    },
    SurfaceEntry {
        tool: "native::list",
        returns_file_contents: false,
        decided_on_its_target: true,
        decided_per_file: None,
        partly_reached: None,
        unbound: None,
        matched_as: Some(PermissionTargetKind::Path),
    },
    SurfaceEntry {
        tool: "native::search",
        returns_file_contents: true,
        decided_on_its_target: true,
        decided_per_file: Some(PerFileProbe {
            arguments: r#"{"path":".","query":"OPENAI_API_KEY"}"#,
            denies: "**/secrets.md",
            still_reports: "OPENAI_API_KEY is set",
        }),
        partly_reached: None,
        unbound: None,
        matched_as: Some(PermissionTargetKind::Path),
    },
    SurfaceEntry {
        tool: "native::grep",
        returns_file_contents: true,
        decided_on_its_target: true,
        decided_per_file: Some(PerFileProbe {
            arguments: r#"{"pattern":"OPENAI_API_KEY"}"#,
            denies: "**/secrets.md",
            still_reports: "OPENAI_API_KEY is set",
        }),
        partly_reached: None,
        unbound: None,
        matched_as: Some(PermissionTargetKind::Path),
    },
    // `glob` reports the paths its pattern names and never their contents. Its
    // pattern is matched as text rather than as the set it denotes, which is a
    // separate limitation over names, not over contents.
    SurfaceEntry {
        tool: "native::glob",
        returns_file_contents: false,
        decided_on_its_target: false,
        decided_per_file: None,
        partly_reached: None,
        unbound: None,
        matched_as: Some(PermissionTargetKind::Path),
    },
    // `git_read` is named by an operation keyword, so no rule written against
    // it selects a file; the files its `diff` reports are decided one by one.
    SurfaceEntry {
        tool: "native::git_read",
        returns_file_contents: true,
        decided_on_its_target: false,
        decided_per_file: Some(PerFileProbe {
            arguments: r#"{"operation":"diff"}"#,
            denies: "**/secrets.md",
            still_reports: "notes: second",
        }),
        partly_reached: None,
        unbound: None,
        matched_as: Some(PermissionTargetKind::Path),
    },
    // `webfetch` returns HTTP and HTTPS responses and cannot address a file.
    SurfaceEntry {
        tool: "native::webfetch",
        returns_file_contents: false,
        decided_on_its_target: false,
        decided_per_file: None,
        partly_reached: None,
        unbound: None,
        matched_as: Some(PermissionTargetKind::Path),
    },
    SurfaceEntry {
        tool: "native::bash",
        returns_file_contents: true,
        decided_on_its_target: false,
        decided_per_file: None,
        partly_reached: None,
        unbound: Some(
            "a rule written for bash is matched against the command line rather than against \
             any path, and the command chooses what it prints. The exception is total, and it \
             is the accepted cost of granting a shell at all.",
        ),
        matched_as: Some(PermissionTargetKind::FreeFormText),
    },
    // Registered directly on the primary path, beside the catalog rather than
    // out of it. It puts a question on an interactive surface and returns what
    // the person answers, so it opens no file and addresses none: its target is
    // the constant `ask_user`, and it projects no reach. Neither probe applies
    // — a `PerFileProbe` needs a file the call would report and a `ReachProbe` a
    // file a rule could name it by, and there is no call to this tool that has
    // either. A delegated child never holds it, having no surface to ask on.
    SurfaceEntry {
        tool: "native::ask_user",
        returns_file_contents: false,
        decided_on_its_target: false,
        decided_per_file: None,
        partly_reached: None,
        unbound: None,
        matched_as: None,
    },
    // Registered directly on the primary path, beside the catalog rather than
    // out of it.
    SurfaceEntry {
        tool: "native::skill",
        returns_file_contents: true,
        decided_on_its_target: false,
        decided_per_file: None,
        partly_reached: Some(ReachProbe {
            arguments: r#"{"skill":"probe","resource_class":"reference","resource":"notes.md"}"#,
            denied_by: "**/.agens/**",
            unmatched_by: "**/elsewhere/**",
        }),
        unbound: Some(
            "a skill call is named by a skill name rather than by a path. A skill discovered \
             under the project root reports the file it would open, so a rule naming that file \
             selects the call — but a skill discovered beside the global configuration ordinarily \
             has no project-relative path for a rule to name, and for those the exception stands. \
             It rests on where the two roots sit rather than on the origin: a global skills root \
             that happens to lie under the project root does have such a spelling, and a rule \
             naming it binds. It is bounded rather than closed by what the tool can \
             open: a skill's files are read relative to that skill's own directory descriptor, \
             under a single normal filename with no traversal, rejecting symbolic links and \
             files carrying more than one link, so the only files it can return are the ones \
             installed as that skill's own assets. It is absent from every delegated child.",
        ),
        matched_as: Some(PermissionTargetKind::Path),
    },
    // Registered directly on the primary path. A delegated child never holds
    // it, which is what keeps delegation from nesting.
    SurfaceEntry {
        tool: "native::task",
        returns_file_contents: true,
        decided_on_its_target: false,
        decided_per_file: None,
        partly_reached: None,
        unbound: Some(
            "a task call is named by the agent it resolves to, so `deny task(reviewer)` refuses \
             every delegation to that agent while no rule written against a path selects one, \
             and what it returns is whatever the child reports. No rule reaches that text here \
             — but the child read those files under these same configured rules, resolved into \
             its own surface by `resolve_child_surface`, so a file this configuration denies \
             was already withheld before the report was written.",
        ),
        matched_as: None,
    },
    // Registered directly on both paths: by a delegated child's runtime for the
    // execution it is, and on the primary path beside `task` for the executions
    // it launches, which is why they are on this surface at all. Both report on
    // an execution rather than on the worktree.
    SurfaceEntry {
        tool: "native::task_control",
        returns_file_contents: false,
        decided_on_its_target: false,
        decided_per_file: None,
        partly_reached: None,
        unbound: None,
        matched_as: None,
    },
    SurfaceEntry {
        tool: "native::task_message",
        returns_file_contents: false,
        decided_on_its_target: false,
        decided_per_file: None,
        partly_reached: None,
        unbound: None,
        matched_as: None,
    },
];

/// Agent definitions that leave a session with no subagent-mode agent, by
/// overriding both built-ins with modes that are not `subagent`.
///
/// `register_production_task_tool` returns before registering anything when no
/// such agent exists, so this is the configuration where a parent holds neither
/// `task` nor the two tools that coordinate a live delegation.
const AGENTS_WITHOUT_A_SUBAGENT: [(&str, &str); 2] = [
    (
        "explore",
        "---\nname: explore\ndescription: primary override\nmode: primary\npermissions: []\n---\nPrimary work.\n",
    ),
    (
        "general",
        "---\nname: general\ndescription: all override\nmode: all\npermissions: []\n---\nAll work.\n",
    ),
];

/// The natives one production parent dispatcher registers, for a session
/// holding `agents`.
fn registered_parent_natives(label: &str, agents: &[(&str, &str)]) -> Vec<String> {
    let temporary = agens_fixtures::session_directory(label);
    let bootstrap = agens_fixtures::session_bootstrap(&temporary, agents);
    let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

    let (_, parent) = agens_tool_runtime::runtime::production_tool_runtime_with_task_runner(
        &bootstrap,
        &project_root,
        Some(&agens_tools::SkillCatalog::default()),
        InertTaskRunner,
    )
    .expect("the parent runtime must build");

    let registered = sorted_native_names(&[parent]);
    fs::remove_dir_all(temporary).unwrap();

    registered
}

/// The natives the two production child dispatchers register, for a delegation
/// no declaration narrows.
fn registered_child_natives() -> Vec<String> {
    let mut registered = registered_child_native_access()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    registered.dedup();

    registered
}

/// The same natives, each paired with the access class the child dispatcher
/// registering it chose. A child's coordination pair is constructed and
/// registered by the child runtime rather than read out of the catalog, so this
/// is the only place their class can be read back from.
///
/// Both dispatchers contribute, and a name they registered under two different
/// classes appears twice here rather than being collapsed into one answer.
fn registered_child_native_access() -> Vec<(String, ToolAccess)> {
    let temporary = agens_fixtures::session_directory("registered-child-natives");
    let project_root = temporary.join("project");
    fs::create_dir_all(&project_root).unwrap();

    let surface = resolve_child_surface(&[], &[]).expect("the child surface must resolve");
    let registry = agens_tools::TaskExecutionRegistry::new();
    let execution = registry
        .admit(agens_tools::TaskLaunchMode::Foreground)
        .expect("a foreground execution must be admissible");
    let (_, child) = agens_tool_runtime::runtime::production_child_tool_runtime(
        &project_root,
        agens_config::ToolLimitSettings::default(),
        &surface,
        registry,
        execution,
        None,
    )
    .expect("the child runtime must build");

    let (_, dangerous) = agens_tool_runtime::runtime::production_dangerous_child_tool_runtime(
        &project_root,
        agens_config::ToolLimitSettings::default(),
    )
    .expect("the dangerous child runtime must build");

    let registered = sorted_native_access(&[child, dangerous]);
    fs::remove_dir_all(temporary).unwrap();

    registered
}

fn sorted_native_names(dispatchers: &[Arc<Mutex<ToolDispatcher>>]) -> Vec<String> {
    let mut registered = sorted_native_access(dispatchers)
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    registered.dedup();

    registered
}

fn sorted_native_access(dispatchers: &[Arc<Mutex<ToolDispatcher>>]) -> Vec<(String, ToolAccess)> {
    let mut registered = dispatchers
        .iter()
        .flat_map(|dispatcher| {
            let dispatcher = dispatcher.lock().expect("dispatcher must be available");

            dispatcher
                .registered_native_names()
                .into_iter()
                .map(|name| {
                    let access = dispatcher
                        .native_access(&name)
                        .expect("a name the dispatcher reported must resolve");
                    (name, access)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    registered.sort_by(|left, right| left.0.cmp(&right.0));
    registered.dedup();

    registered
}

/// Every native a production dispatcher registers in any configuration,
/// gathered from the dispatchers themselves.
///
/// The catalog is not the surface. `ask_user`, `skill`, `task` and the two
/// coordination tools are registered directly beside the catalog's ten on the
/// primary path, and a delegated child registers the coordination pair the same
/// way, so a table compared against `NativeToolCatalog::metadata()` leaves five
/// tools unclassified while reading as complete.
///
/// Neither is one session's dispatcher the surface: what a parent registers
/// depends on whether any subagent-mode agent exists, so this unions both
/// configurations. Which of them contributes what is pinned by
/// [`a_parent_holding_no_subagent_registers_the_surface_without_the_delegation_tools`].
fn registered_native_surface() -> Vec<String> {
    let mut registered = registered_parent_natives("registered-native-surface", &[])
        .into_iter()
        .chain(registered_parent_natives(
            "registered-native-surface-no-subagent",
            &AGENTS_WITHOUT_A_SUBAGENT,
        ))
        .chain(registered_child_natives())
        .collect::<Vec<_>>();
    registered.sort_unstable();
    registered.dedup();

    registered
}

/// The two lists naming the natives registered beside the catalog have to hold
/// what the dispatchers actually register beside it.
///
/// Both lists resolve names: a declaration is expanded over the child's, and a
/// configured entry is qualified against the shared one. A tool registered
/// without being added to them is a rule that reads as enforced and matches no
/// dispatcher identity. Only this direction catches that — a name in a list and
/// in no dispatcher already fails the classification above, while a name in a
/// dispatcher and in no list failed nothing.
///
/// Neither comparison can catch a list that SHRINKS, because the same list
/// drives the registration and both sides shrink together. The child's two are
/// therefore named here outright: a subagent reports its progress and its
/// result through them, so a child registering neither cannot answer the
/// delegation that launched it.
#[test]
fn every_native_registered_beside_the_catalog_is_named_by_the_list_that_resolves_it() {
    let catalog = NativeToolCatalog::metadata()
        .into_iter()
        .map(|entry| entry.qualified_name)
        .collect::<Vec<_>>();
    let beside = |registered: Vec<String>| {
        registered
            .into_iter()
            .filter(|tool| !catalog.contains(tool))
            .collect::<Vec<_>>()
    };
    let child = registered_child_natives();

    assert_eq!(
        beside(registered_native_surface()),
        agens_tools::NATIVE_TOOLS_REGISTERED_OUTSIDE_THE_CATALOG,
        "a native registered outside the catalog has to be named where configured rules are \
         qualified"
    );
    assert_eq!(
        beside(child.clone()),
        agens_tool_runtime::child_catalog::CHILD_NON_CATALOG_TOOLS,
        "a native a child registers outside the catalog has to be named where declarations are \
         expanded"
    );

    for tool in ["native::task_control", "native::task_message"] {
        assert!(
            child.iter().any(|registered| registered == tool),
            "a delegated child has to hold {tool} to report back at all: {child:?}"
        );
    }
}

/// The access class each native registered beside the catalog is registered
/// under, and what that class decides.
///
/// These five have no catalog entry to be compared against: each declares its
/// access at its own registration site, and until it is written down somewhere
/// that declaration answers to nothing.
///
/// Access decides one thing in the policy, and it decides it above every rule:
/// the `ChatWrite` hard-safety predicate refuses a `Write`-access call outright
/// whenever the session is in chat mode. A write tool registered as `ReadOnly`
/// is therefore a tool chat mode will run. The dispatcher also re-checks it
/// when redeeming an authorization, so a registration that changed class
/// invalidates the handles taken under the old one.
///
/// It decides nothing else here, and two things it is easy to assume it decides
/// it does not: the fallback for a call no rule matched is settled by the
/// unmatched override, the session's bypass and the mode, none of which read
/// access; and the automatic grant a delegated subagent runs under is a list of
/// names in `child_catalog` rather than a class — `AUTO_ALLOW_NATIVE_TOOLS`'
/// six, plus whichever of the two `CHILD_NON_CATALOG_TOOLS` this delegation's
/// own rules leave reachable, so eight at most and six at least. It excludes
/// `native::webfetch` precisely because that tool is `ReadOnly` and the class
/// is the wrong predicate for it.
const ACCESS_OF_THE_NATIVES_REGISTERED_BESIDE_THE_CATALOG: [(&str, ToolAccess); 5] = [
    // Puts a question on an interactive surface and returns the answer given
    // there, touching neither the worktree nor the network. The class is
    // load-bearing rather than incidental: a chat-mode turn is exactly the turn
    // that has to be able to ask, and `Write` would have the hard-safety
    // predicate refuse it there above every rule.
    ("native::ask_user", ToolAccess::ReadOnly),
    // Returns a skill's own installed files and changes nothing.
    ("native::skill", ToolAccess::ReadOnly),
    // Runs a whole turn on a surface of its own, whose calls write; a class
    // saying otherwise would put every delegation inside chat mode.
    ("native::task", ToolAccess::Write),
    // Both act on a live execution — backgrounding it, cancelling it, queueing
    // a message onto it — rather than reading anything.
    ("native::task_control", ToolAccess::Write),
    ("native::task_message", ToolAccess::Write),
];

/// [`ACCESS_OF_THE_NATIVES_REGISTERED_BESIDE_THE_CATALOG`] answers for every
/// native outside the catalog, and for the child's own registration of the two
/// it shares with the parent.
///
/// Without the first assertion the table is one-directional: a sixth native
/// registered beside the catalog is forced into
/// [`NATIVE_TOOLS_REGISTERED_OUTSIDE_THE_CATALOG`] by the test above, and its
/// access class would then go unwritten and unasserted — the same gap in the
/// same shape, one list along.
///
/// The second exists because the coordination pair is registered on two paths
/// that share no constant: the parent's registration is what the probe
/// dispatcher mirrors, while the runtime for a live delegation chooses the
/// class at its own call site and was compared against nothing. A class the
/// child dispatchers do not agree on shows up here as two answers for one name,
/// and a pair no child registers as none.
#[test]
fn every_native_beside_the_catalog_has_its_access_written_down_and_held_by_the_child_too() {
    let written = ACCESS_OF_THE_NATIVES_REGISTERED_BESIDE_THE_CATALOG
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();

    assert_eq!(
        written,
        agens_tools::NATIVE_TOOLS_REGISTERED_OUTSIDE_THE_CATALOG,
        "a native registered beside the catalog has to have its access class written down, not \
         only its name"
    );

    let child = registered_child_native_access();
    for tool in agens_tool_runtime::child_catalog::CHILD_NON_CATALOG_TOOLS {
        let expected = ACCESS_OF_THE_NATIVES_REGISTERED_BESIDE_THE_CATALOG
            .iter()
            .find(|(name, _)| *name == tool)
            .map(|(_, access)| *access)
            .expect("a child coordination tool is registered beside the catalog");
        let held = child
            .iter()
            .filter(|(name, _)| name == tool)
            .map(|(_, access)| *access)
            .collect::<Vec<_>>();

        assert_eq!(
            held,
            vec![expected],
            "{tool} has to reach a delegated child under the one class written down for it, and \
             under no other"
        );
    }
}

/// The probe dispatcher holds each native under the access class production
/// registers it by, not merely under the same name.
///
/// Access is what the `ChatWrite` predicate refuses on, above every rule, so a
/// harness holding a write tool as read-class runs in chat mode a call the
/// session it stands in for would refuse outright — and answers every
/// hard-safety case differently. It does not decide the fallback for an
/// unmatched call; the statement above
/// [`ACCESS_OF_THE_NATIVES_REGISTERED_BESIDE_THE_CATALOG`] says what does.
///
/// This asks the probe dispatcher itself, and compares its answers against two
/// statements neither it nor
/// [`production_parent_natives`] is derived from: the catalog's own declared
/// access for the ten it holds, and
/// [`ACCESS_OF_THE_NATIVES_REGISTERED_BESIDE_THE_CATALOG`] for the five it does
/// not.
#[test]
fn the_probe_dispatcher_holds_every_native_under_the_access_production_registers_it_by() {
    let dispatcher = native_dispatcher();
    let declared = NativeToolCatalog::metadata()
        .into_iter()
        .map(|entry| (entry.qualified_name, entry.access))
        .chain(
            ACCESS_OF_THE_NATIVES_REGISTERED_BESIDE_THE_CATALOG
                .iter()
                .map(|(name, access)| ((*name).to_owned(), *access)),
        );

    for (name, access) in declared {
        assert_eq!(
            dispatcher.native_access(&name),
            Some(access),
            "{name} must be held under the access it is registered by"
        );
    }
}

/// The classified surface is a union across configurations, and this says which
/// configuration contributes what.
///
/// A session with no subagent-mode agent registers strictly less: `task` never
/// reaches the dispatcher, and neither do the two tools that coordinate a live
/// delegation, because the same early return registers all three.
#[test]
fn a_parent_holding_no_subagent_registers_the_surface_without_the_delegation_tools() {
    let delegating = registered_parent_natives("parent-with-a-subagent", &[]);
    let alone = registered_parent_natives("parent-without-a-subagent", &AGENTS_WITHOUT_A_SUBAGENT);

    assert!(
        alone.iter().all(|tool| delegating.contains(tool)),
        "a session with no subagent must register no tool the delegating one lacks: \
         {alone:?} against {delegating:?}"
    );
    assert_eq!(
        delegating
            .iter()
            .filter(|tool| !alone.contains(tool))
            .collect::<Vec<_>>(),
        vec![
            "native::task",
            "native::task_control",
            "native::task_message"
        ],
        "these are exactly the tools a delegation brings with it"
    );
}

/// The surface the table classifies has to be the surface that exists, and
/// every tool on it that can return the contents of a file has to be decided
/// one of the two ways — or carry a written reason why neither reaches it.
#[test]
fn every_native_tool_that_returns_file_contents_is_reached_by_a_path_rule() {
    let mut classified = TOOL_SURFACE
        .iter()
        .map(|entry| entry.tool.to_owned())
        .collect::<Vec<_>>();
    classified.sort_unstable();

    assert_eq!(
        classified,
        registered_native_surface(),
        "every native tool the production dispatchers register must be classified here, \
         and every row must name a real tool"
    );

    let mismatched = TOOL_SURFACE
        .iter()
        .filter(|entry| {
            let reached = entry.decided_on_its_target || entry.decided_per_file.is_some();
            entry.returns_file_contents && !reached != entry.unbound.is_some()
        })
        .map(|entry| entry.tool)
        .collect::<Vec<_>>();

    assert!(
        mismatched.is_empty(),
        "a tool that returns the contents of a file and is decided neither on its target \
         nor per file is a path deny that does not bind, and has to say why that is \
         acceptable; a tool a rule does reach must claim no exception: {mismatched:?}"
    );

    let unbound = TOOL_SURFACE
        .iter()
        .filter(|entry| entry.unbound.is_some())
        .map(|entry| entry.tool)
        .collect::<Vec<_>>();

    assert_eq!(
        unbound,
        vec!["native::bash", "native::skill", "native::task"],
        "adding an exception to this list is a deliberate act and has to show up in the diff"
    );

    for entry in TOOL_SURFACE {
        let Some(expected) = entry.matched_as else {
            continue;
        };
        let bare = entry.tool.trim_start_matches("native::");

        assert_eq!(
            permission_target_kind_for_tool(bare),
            expected,
            "{bare} is matched under the wrong target shape"
        );
    }

    let mut with_a_remote_tool = native_dispatcher();
    with_a_remote_tool
        .register_mcp(&probe_remote_metadata(), probe_remote_tool())
        .expect("the probe dispatcher must accept the remote tool");
    let mut still_native = with_a_remote_tool.registered_native_names();
    still_native.sort_unstable();

    // This does not guard the registration. `register_mcp` cannot produce a
    // `native:`-prefixed identity by any path, so no change there reaches this
    // assertion, and it is not standing in for the MCP surface being left
    // undecided — that is executed by
    // `no_path_rule_reaches_an_mcp_tool_and_naming_the_tool_refuses_every_call`.
    //
    // What it guards is `registered_native_names`, which is how every list
    // above reads a dispatcher's surface: it must report the natives of a
    // dispatcher that holds both kinds and nothing else. It tells them apart
    // twice over — by the `native:` prefix and by the length header matching
    // the name — and either check alone already excludes a remote identity, so
    // this fails only when a reader loses both. That is a real edit and a
    // narrow guard, not a broad one.
    assert_eq!(
        still_native, classified,
        "a dispatcher holding both kinds has to report the native surface this table classifies \
         and nothing else, or every list derived from `registered_native_names` is reading a \
         surface it was not asked about"
    );
}

/// The metadata one remote tool arrives with, named the way a rule naming it
/// has to be written: `<server>::<tool>`.
fn probe_remote_metadata() -> agens_tools::RemoteToolMetadata {
    agens_tools::RemoteToolMetadata {
        qualified_name: PROBE_REMOTE_TOOL.into(),
        server_name: "probe".into(),
        tool_name: "read_text_file".into(),
        description: None,
        input_schema: serde_json::json!({"type": "object"}),
        access: agens_tools::RemoteToolAccess::ReadOnly,
    }
}

/// The production adapter a remote tool reaches the dispatcher through. Its
/// registry is empty because no probe here executes the call — what is under
/// test is the target and the reach the adapter projects, which it decides
/// without consulting the server.
fn probe_remote_tool() -> agens_dispatch::RegisteredMcpTool {
    agens_dispatch::RegisteredMcpTool {
        name: PROBE_REMOTE_TOOL.into(),
        registry: Arc::new(Mutex::new(agens_tools::McpRegistry::new())),
    }
}

const PROBE_REMOTE_TOOL: &str = "probe::read_text_file";

/// What a rule can and cannot decide about a remote tool, and why
/// [`TOOL_SURFACE`] stops where it does.
///
/// A remote tool's arguments are defined by the server that serves it. Nothing
/// in agens can say which file — if any — `{"path": "src/.env"}` names to that
/// server, so the adapter projects the tool's own name as the target and
/// reports no reach at all. A rule written against a path therefore resolves
/// cleanly at startup, binds to this tool, and then selects no call it ever
/// makes.
///
/// What does bind is a rule naming the tool: with no target it refuses every
/// call, and with the tool's own name as the target it refuses them the same
/// way, because that name is what each call is matched against. This is the
/// whole class — every MCP tool is decided this way, whatever it returns.
#[test]
fn no_path_rule_reaches_an_mcp_tool_and_naming_the_tool_refuses_every_call() {
    let arguments = serde_json::json!({"path": "src/.env"});

    for (entry, denied) in [
        (format!("deny {PROBE_REMOTE_TOOL} **/.env"), false),
        (format!("deny {PROBE_REMOTE_TOOL} src/.env"), false),
        (format!("deny {PROBE_REMOTE_TOOL}"), true),
        (
            format!("deny {PROBE_REMOTE_TOOL} {PROBE_REMOTE_TOOL}"),
            true,
        ),
    ] {
        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register_mcp(&probe_remote_metadata(), probe_remote_tool())
            .expect("the probe dispatcher must accept the remote tool");
        let dispatcher = Arc::new(Mutex::new(dispatcher));

        let policy = permission_policy(
            &configured_entries(&[&entry]),
            "project",
            PermissionMode::Edit,
            &dispatcher,
            None,
        )
        .expect("a rule naming a registered remote tool must resolve");
        let outcome = dispatcher
            .lock()
            .expect("dispatcher must be available")
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new("project", PROBE_REMOTE_TOOL, arguments.clone()),
            )
            .expect("the call must be decidable");

        assert_eq!(
            matches!(outcome, ToolEvaluationOutcome::Denied),
            denied,
            "`{entry}` must {} a call carrying {arguments}",
            if denied { "refuse" } else { "not select" }
        );
    }
}

/// The model-facing spelling of the same remote tool, which is also a spelling
/// a rule may be written in: `register_mcp` installs it as an alias, and the
/// repository's own configuration fixture uses it.
const PROBE_REMOTE_TOOL_AS_ADVERTISED: &str = "probe_read_text_file";

/// A dispatcher for a session that declared the probe server and reached
/// nothing from it — a server that failed to start, or one shipped `disabled`.
fn dispatcher_declaring_an_absent_probe_server() -> Arc<Mutex<ToolDispatcher>> {
    let mut dispatcher = native_dispatcher();
    dispatcher.declare_mcp_servers(["probe".to_owned()]);

    Arc::new(Mutex::new(dispatcher))
}

/// A rule naming a remote tool is resolved against whatever the dispatcher
/// holds when the session starts. A server that failed to start contributes no
/// tools, so the name stops resolving — and refusing to run over it would blame
/// the operator's configuration for a server being unreachable, on the very
/// rule the documentation tells them to write.
///
/// Such a name is therefore retained as written, and it binds for real the
/// moment the server comes back: the dispatcher's own alias lookup carries it
/// to the identity policy compares against.
///
/// One remote tool answers to two names — `<server>::<tool>` and the
/// `<server>_<tool>` it is advertised to the model under — and a rule may be
/// written in either. Only the first says on its own that it is remote; the
/// second is shaped exactly like a bare native name, so nothing distinguishes
/// `probe_read_text_file` from a misspelt `webfetc` except the configuration
/// that declares the server. Both spellings are therefore softened on the same
/// condition: the session declares that server and holds nothing from it.
#[test]
fn both_spellings_of_a_tool_of_a_declared_absent_server_resolve_and_bind_when_it_returns() {
    for entry in [PROBE_REMOTE_TOOL, PROBE_REMOTE_TOOL_AS_ADVERTISED] {
        let policy = permission_policy(
            &configured_entries(&[&format!("deny {entry}")]),
            "project",
            PermissionMode::Edit,
            &dispatcher_declaring_an_absent_probe_server(),
            None,
        )
        .unwrap_or_else(|error| {
            panic!("`deny {entry}` names a tool of a declared server and must resolve: {error:?}")
        });

        let mut returned = ToolDispatcher::new();
        returned
            .register_mcp(&probe_remote_metadata(), probe_remote_tool())
            .expect("the probe dispatcher must accept the remote tool");
        let outcome = returned
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    "project",
                    PROBE_REMOTE_TOOL,
                    serde_json::json!({"path": "src/.env"}),
                ),
            )
            .expect("the call must be decidable");

        assert!(
            matches!(outcome, ToolEvaluationOutcome::Denied),
            "`deny {entry}` has to refuse the call once the server it names comes back, or it \
             reads as denying and denies nothing: {outcome:?}"
        );
    }
}

/// Nothing beyond that is softened, and both spellings refuse in step.
///
/// Every entry here is a `deny`. The `allow` side of the same names is decided
/// differently, and is pinned by
/// [`an_allow_naming_an_unresolvable_tool_is_dropped_where_a_deny_refuses_to_run`].
///
/// "Refuses" is refusing to build the policy, which is not one moment on both
/// surfaces: `agens chat` builds it for the turn it was asked to run, so the
/// command fails, while the TUI builds it inside its own submit path, so the
/// session starts and every prompt fails instead. Neither runs a call under a
/// rule it could not resolve, which is what this pins.
///
/// A tool name misspelt against a server this session DOES hold has a live
/// surface to be checked against. A name for a server the configuration never
/// declared is indistinguishable from a typo, whichever spelling it is written
/// in. And `native` is not a server: `native::` is the exact prefix the
/// dispatcher qualifies its own tools under, so a misspelt native keeps failing
/// in either of ITS spellings.
///
/// `native::` takes precedence over any server that claims it, which is what
/// the last case asks: a session declaring a server literally called `native`
/// must not turn a misspelt native into a retained remote rule. The comparison
/// is exact rather than case-folded, because `native::` is the literal prefix
/// natives are qualified under. `Native` is a legal MCP server name and is
/// treated as one — `Native::writ` is refused for the ordinary reason that no
/// server by that name was declared, rather than for being native.
#[test]
fn a_name_no_declared_server_and_no_native_surface_explains_refuses_to_run() {
    let mut holding_the_probe_server = native_dispatcher();
    holding_the_probe_server.declare_mcp_servers(["probe".to_owned()]);
    holding_the_probe_server
        .register_mcp(&probe_remote_metadata(), probe_remote_tool())
        .expect("the probe dispatcher must accept the remote tool");

    for (entry, dispatcher) in [
        ("deny probe::read_txt_file", holding_the_probe_server),
        ("deny probe_read_txt_file", {
            let mut held = native_dispatcher();
            held.declare_mcp_servers(["probe".to_owned()]);
            held.register_mcp(&probe_remote_metadata(), probe_remote_tool())
                .expect("the probe dispatcher must accept the remote tool");
            held
        }),
        ("deny ghost::read_text_file", native_dispatcher()),
        ("deny ghost_read_text_file", native_dispatcher()),
        ("deny webfetc", native_dispatcher()),
        ("deny native::webfetc", native_dispatcher()),
        ("deny Native::writ", native_dispatcher()),
        ("deny native::webfetc", {
            let mut named_native = native_dispatcher();
            named_native.declare_mcp_servers(["native".to_owned()]);
            named_native
        }),
    ] {
        let rejected = permission_policy(
            &configured_entries(&[entry]),
            "project",
            PermissionMode::Edit,
            &Arc::new(Mutex::new(dispatcher)),
            None,
        );

        assert!(
            rejected.is_err(),
            "`{entry}` names nothing this session's configuration or native surface can answer \
             for, and must refuse to run rather than read as enforced"
        );
    }
}

/// A session declaring both `a` and `a_b`, having reached the tools of
/// whichever of them is named. Each serves one tool of its own, neither of
/// which is advertised as `a_b_c` — so that name is explained by both servers
/// and held by neither.
fn dispatcher_declaring_two_servers(held: &[&str]) -> Arc<Mutex<ToolDispatcher>> {
    let mut dispatcher = native_dispatcher();
    dispatcher.declare_mcp_servers(["a".to_owned(), "a_b".to_owned()]);

    for server in held {
        dispatcher
            .register_mcp(
                &agens_tools::RemoteToolMetadata {
                    qualified_name: format!("{server}::served"),
                    server_name: (*server).into(),
                    tool_name: "served".into(),
                    description: None,
                    input_schema: serde_json::json!({"type": "object"}),
                    access: agens_tools::RemoteToolAccess::ReadOnly,
                },
                probe_remote_tool(),
            )
            .expect("the probe dispatcher must accept the remote tool");
    }

    Arc::new(Mutex::new(dispatcher))
}

/// One `<server>_<tool>` name can be explained by more than one declared
/// server, and the question asked of them is whether ANY of them explains it —
/// not what the first one in some order happens to say.
///
/// Deciding on whichever server is examined first refuses a correctly spelt
/// rule whenever that server is the one that is running: `a` is here, therefore
/// `a_b_c` is a typo — while `a_b`, which is not here, explains the name
/// exactly.
///
/// A name that any absent declared server explains is retained for the reason
/// every absent surface is retained: it binds when that server returns. A name
/// every server that could explain it is holding has a live surface to be
/// checked against, and fails there.
#[test]
fn a_name_two_declared_servers_could_explain_is_resolved_by_any_of_them() {
    for (held, resolves) in [
        (&["a"][..], true),
        (&["a_b"][..], true),
        (&[][..], true),
        (&["a", "a_b"][..], false),
    ] {
        let resolved = permission_policy(
            &configured_entries(&["deny a_b_c"]),
            "project",
            PermissionMode::Edit,
            &dispatcher_declaring_two_servers(held),
            None,
        );

        assert_eq!(
            resolved.is_ok(),
            resolves,
            "with {held:?} running, `deny a_b_c` must {}: {resolved:?}",
            if resolves {
                "resolve against the declared server that is not here"
            } else {
                "be checked against the live surfaces that hold every name explaining it"
            }
        );
    }
}

/// A refusal names the rule it refused and why it could not be resolved.
///
/// The operator who removed a server and the operator who mistyped a tool are
/// looking at different mistakes, and `[permissions]` may be written in a
/// project document that is committed to a repository while `[mcp]` may not
/// (`agens-bootstrap` rejects a project `[mcp]` block). A collaborator whose
/// global configuration never declared that server therefore inherits a rule
/// they did not write, and a refusal that says only that the configuration is
/// invalid leaves them nothing to act on.
#[test]
fn a_refusal_names_the_rule_it_refused_and_why_it_could_not_be_resolved() {
    let mut holding_the_probe_server = native_dispatcher();
    holding_the_probe_server.declare_mcp_servers(["probe".to_owned()]);
    holding_the_probe_server
        .register_mcp(&probe_remote_metadata(), probe_remote_tool())
        .expect("the probe dispatcher must accept the remote tool");

    for (entry, dispatcher, expected) in [
        (
            "deny ghost::read_text_file",
            native_dispatcher(),
            vec!["ghost::read_text_file", "ghost", "declares"],
        ),
        (
            "deny ghost_read_text_file",
            native_dispatcher(),
            vec!["ghost_read_text_file", "native tool"],
        ),
        (
            "deny webfetc",
            native_dispatcher(),
            vec!["webfetc", "native tool"],
        ),
        (
            "deny probe::read_txt_file",
            holding_the_probe_server,
            vec!["probe::read_txt_file", "probe", "serves"],
        ),
        (
            "deny read ",
            native_dispatcher(),
            vec!["native::read", "target"],
        ),
    ] {
        let message = permission_policy(
            &configured_entries(&[entry]),
            "project",
            PermissionMode::Edit,
            &Arc::new(Mutex::new(dispatcher)),
            None,
        )
        .expect_err("the rule under test must be refused")
        .to_string();

        for fragment in expected {
            assert!(
                message.contains(fragment),
                "the refusal of `{entry}` has to say {fragment:?}, and says: {message}"
            );
        }
    }
}

/// A configured `allow` naming a tool nothing here can resolve is dropped, and
/// only `allow`.
///
/// It is the same trade an agent definition already makes for the same reason
/// (`agens_tools`' `resolved_selectors`): a grant for a tool no call can name
/// grants nothing, so refusing to run over one turns a harmless stale line into
/// a session that cannot start. `[permissions]` may be written in a project
/// document that is committed to a repository, so that line can arrive from a
/// collaborator's checkout rather than from the operator's own file.
///
/// `deny` and `ask` keep refusing. Dropping one of those silently would leave
/// an operator believing a restriction is in force when the name they wrote
/// reaches nothing — which is fail-open in exactly the direction a dropped
/// `allow` is fail-closed.
#[test]
fn an_allow_naming_an_unresolvable_tool_is_dropped_where_a_deny_refuses_to_run() {
    for tool in ["engram_mem_save", "engram::mem_save", "webfetc"] {
        let allowed = permission_policy(
            &configured_entries(&[&format!("allow {tool}")]),
            "project",
            PermissionMode::Edit,
            &Arc::new(Mutex::new(native_dispatcher())),
            None,
        );

        assert!(
            allowed.is_ok(),
            "`allow {tool}` grants nothing this session can call and must not stop it starting: \
             {allowed:?}"
        );

        for decision in ["deny", "ask"] {
            let refused = permission_policy(
                &configured_entries(&[&format!("{decision} {tool}")]),
                "project",
                PermissionMode::Edit,
                &Arc::new(Mutex::new(native_dispatcher())),
                None,
            );

            assert!(
                refused.is_err(),
                "`{decision} {tool}` reads as a restriction and reaches nothing, so it must \
                 refuse to run: {refused:?}"
            );
        }
    }
}

/// A session's own configuration decides which server names explain a rule, so
/// the dispatcher a production parent hands to `permission_policy` has to carry
/// them — including for a server that was never contacted at all.
///
/// A `disabled` server is the case the shipped `example/config.toml` documents
/// — its `[mcp.*]` blocks are commented out, under a heading asking the reader
/// to uncomment and adapt one of them, so the server built here is this test's
/// own. Nothing is discovered from it, so every name it would have explained
/// resolves through this declaration or through nothing.
#[test]
fn a_production_parent_declares_a_disabled_server_so_a_rule_naming_its_tools_resolves() {
    let temporary = agens_fixtures::session_directory("declared-disabled-server");
    let mut bootstrap = agens_fixtures::session_bootstrap(&temporary, &[]);
    bootstrap.mcp_servers.push(disabled_probe_server());
    let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

    let (_, dispatcher) = agens_tool_runtime::runtime::production_tool_runtime_with_task_runner(
        &bootstrap,
        &project_root,
        Some(&agens_tools::SkillCatalog::default()),
        InertTaskRunner,
    )
    .expect("the parent runtime must build");

    for entry in [PROBE_REMOTE_TOOL, PROBE_REMOTE_TOOL_AS_ADVERTISED] {
        let resolved = permission_policy(
            &configured_entries(&[&format!("deny {entry}")]),
            "project",
            PermissionMode::Edit,
            &dispatcher,
            None,
        );

        assert!(
            resolved.is_ok(),
            "`deny {entry}` names a tool of a server this configuration declares, and a session \
             that disabled that server must still run: {resolved:?}"
        );
    }

    let rejected = permission_policy(
        &configured_entries(&["deny ghost_read_text_file"]),
        "project",
        PermissionMode::Edit,
        &dispatcher,
        None,
    );

    assert!(
        rejected.is_err(),
        "a name no declared server explains must still refuse to run: {rejected:?}"
    );

    fs::remove_dir_all(temporary).unwrap();
}

/// One `disabled` MCP server, declared and never contacted.
fn disabled_probe_server() -> agens_config::McpServerConfig {
    agens_config::McpServerConfig {
        name: "probe".into(),
        disabled: true,
        transport: agens_config::McpTransport::Stdio,
        command: Some(std::path::PathBuf::from("/nonexistent/probe")),
        args: Vec::new(),
        environment: std::collections::BTreeMap::new(),
        cwd: None,
        url: None,
        headers: std::collections::BTreeMap::new(),
        max_retries: 0,
        timeout_ms: 1_000,
    }
}

/// A configured rule naming a native this session does not register has to
/// resolve, because a session that registers fewer natives is a configuration
/// agens supports rather than an operator error.
///
/// `register_production_task_tool` returns before registering anything when no
/// subagent-mode agent exists, so `task` and the two coordination tools are
/// absent from a parent built under [`AGENTS_WITHOUT_A_SUBAGENT`]. A rule
/// naming one of them is spelt correctly, names a real tool, and passes
/// configuration validation — and resolving it through the live dispatcher
/// alone made agens refuse to run.
///
/// This goes through `permission_policy` itself rather than through a resolver
/// substituted for it, because the dispatcher lookup is the whole mechanism
/// under test.
#[test]
fn a_rule_naming_a_native_this_session_never_registers_still_runs_the_session() {
    let temporary = agens_fixtures::session_directory("rule-naming-an-unregistered-native");
    let bootstrap = agens_fixtures::session_bootstrap(&temporary, &AGENTS_WITHOUT_A_SUBAGENT);
    let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

    let (_, dispatcher) = agens_tool_runtime::runtime::production_tool_runtime_with_task_runner(
        &bootstrap,
        &project_root,
        Some(&agens_tools::SkillCatalog::default()),
        InertTaskRunner,
    )
    .expect("the parent runtime must build");

    for tool in ["task", "task_control", "task_message"] {
        assert!(
            dispatcher
                .lock()
                .expect("dispatcher must be available")
                .canonical_identity(&format!("native::{tool}"))
                .is_none(),
            "{tool} must be absent from this parent, or the case under test is not the one \
             described"
        );

        let resolved = permission_policy(
            &configured_entries(&[&format!("deny {tool}")]),
            "project",
            PermissionMode::Edit,
            &dispatcher,
            None,
        );

        let policy = resolved.unwrap_or_else(|error| {
            panic!(
                "`deny {tool}` names a real native and must not refuse to run a session that \
                 registers no delegation: {error:?}"
            )
        });

        let mut delegating = ToolDispatcher::new();
        delegating
            .register_native(
                format!("native::{tool}"),
                agens_core::ToolAccess::Write,
                InertTool,
            )
            .expect("the delegation tool must register");
        let outcome = delegating
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new(
                    "project",
                    tool,
                    serde_json::json!({"target": "reviewer"}),
                ),
            )
            .expect("the call must be decidable");

        assert!(
            matches!(outcome, ToolEvaluationOutcome::Denied),
            "`deny {tool}` has to refuse the call once a session registers the tool it names, or \
             it reads as denying and denies nothing: {outcome:?}"
        );
    }

    let rejected = permission_policy(
        &configured_entries(&["deny tsk"]),
        "project",
        PermissionMode::Edit,
        &dispatcher,
        None,
    );

    assert!(
        rejected.is_err(),
        "a name that is no native at all must still refuse to run: {rejected:?}"
    );

    fs::remove_dir_all(temporary).unwrap();
}

/// The secret a probe must never report. It is written into the working tree
/// and never committed, so `.git` holds no copy of it that a probe could reach
/// without asking the rules about `.env`.
const PER_FILE_PROBE_SECRET: &str = "sk-live-surface-probe-do-not-leak";

/// Runs one git command in the probe worktree, under an identity and a
/// configuration of its own, so neither the machine's committer identity nor
/// its global ignore rules decide what the fixture ends up containing.
fn probe_git(root: &Path, arguments: &[&str]) {
    let output = std::process::Command::new("git")
        .args(arguments)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "agens")
        .env("GIT_AUTHOR_EMAIL", "agens@example.invalid")
        .env("GIT_COMMITTER_NAME", "agens")
        .env("GIT_COMMITTER_EMAIL", "agens@example.invalid")
        .output()
        .expect("git must be available to probe git_read");

    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A worktree every probe can be run over: `secrets.md` holds the secret and
/// `notes.md` holds a line naming the same key, both reachable by a walk, by a
/// pattern and by a diff against the commit that holds neither.
///
/// The file holding the secret is deliberately not a dotfile. `grep` and `glob`
/// walk with hidden entries excluded, so a probe denying `**/.env` would come
/// back without the secret whether or not the tool ever asked the rules — the
/// walker would have answered for it, and deleting the tool's own check would
/// leave the probe green.
///
/// The repository keeps its metadata outside the worktree, because a probe that
/// walks every file under the root would otherwise walk git's own storage and
/// fail on bytes that are not text.
///
/// The worktree carries its own `.gitignore` re-including both files. The walk
/// `grep` and `search` perform honors ignore files from every parent directory
/// and the machine's `core.excludesFile`, none of which this fixture controls,
/// so an ambient rule naming `secrets.md` would drop the denied file from the
/// walk and leave the probe passing whether or not the tool asked the rules.
///
/// The re-inclusion narrows that, and does not close it: a deeper file outranks
/// a shallower one only within its own ignore category, so an ancestor `.ignore`
/// still outranks this `.gitignore`. What actually makes the probes falsifiable
/// is that each of them first runs with no rule withholding anything and has to
/// come back holding the secret; a walk that never reached the file fails there
/// rather than passing the assertion that follows.
fn worktree_holding_a_denied_secret(project_root: &Path, git_dir: &Path) {
    fs::create_dir_all(project_root).expect("the probe worktree must be creatable");
    let _ = fs::remove_dir(project_root.join(".git"));
    probe_git(
        project_root,
        &[
            "init",
            "--initial-branch=main",
            "--quiet",
            &format!("--separate-git-dir={}", git_dir.display()),
        ],
    );

    fs::write(project_root.join(".gitignore"), "!secrets.md\n!notes.md\n").unwrap();
    fs::write(
        project_root.join("secrets.md"),
        "OPENAI_API_KEY=placeholder\n",
    )
    .unwrap();
    fs::write(project_root.join("notes.md"), "notes: first\n").unwrap();
    probe_git(project_root, &["add", "-A"]);
    probe_git(project_root, &["commit", "--quiet", "-m", "first"]);

    fs::write(
        project_root.join("secrets.md"),
        format!("OPENAI_API_KEY={PER_FILE_PROBE_SECRET}\n"),
    )
    .unwrap();
    fs::write(
        project_root.join("notes.md"),
        "OPENAI_API_KEY is set\nnotes: second\n",
    )
    .unwrap();
}

/// Every per-file claim in [`TOOL_SURFACE`] is executed here, so a row asserting
/// it without the wiring behind it fails rather than reading as proven.
///
/// Each probe runs the real tool over a worktree holding a denied secret, under
/// the rule that denies it, and has to come back with what it is allowed to
/// report and without the secret. A tool that never asks reports both.
///
/// Each probe is run twice: once with no rule withholding anything, where the
/// secret must come back, and once under the rule denying it, where it must
/// not. The first run is what makes the second falsifiable — a call that never
/// reaches the denied file at all comes back without the secret for a reason
/// that has nothing to do with the rules, and would pass the second assertion
/// with the tool's own check deleted.
#[test]
fn every_tool_this_table_says_asks_per_file_withholds_a_denied_file() {
    let temporary = agens_fixtures::session_directory("tool-surface-per-file");
    let project_root = temporary.join("project");
    worktree_holding_a_denied_secret(&project_root, &temporary.join("git"));

    let catalog = NativeToolCatalog::new(
        NativeTools::open(&project_root).expect("the probe worktree must open as a project root"),
    );

    for entry in TOOL_SURFACE {
        let Some(probe) = entry.decided_per_file.as_ref() else {
            continue;
        };

        let bare = entry.tool.trim_start_matches("native::");
        let arguments: serde_json::Value =
            serde_json::from_str(probe.arguments).expect("a probe names its call in JSON");
        let reached = catalog
            .execute(
                entry.tool,
                arguments.clone(),
                &ToolExecutionContext::with_timeout(Duration::from_secs(30)),
            )
            .unwrap_or_else(|error| panic!("{bare} must answer its probe: {error}"));

        assert!(
            reached.content.contains(PER_FILE_PROBE_SECRET),
            "{bare} must reach the denied file when no rule withholds it, or the probe below \
             proves nothing: {}",
            reached.content
        );

        let rule = format!("deny {bare} {}", probe.denies);
        let (policy, identity) = configured_child_policy(&configured_rules(&[&rule]), &[], bare)
            .unwrap_or_else(|| panic!("a delegated child must be able to reach {bare}"));

        let context = ToolExecutionContext::with_timeout(Duration::from_secs(30)).with_read_filter(
            PermissionReadFilter::new(
                policy,
                Vec::new(),
                "project",
                identity,
                ToolAccess::ReadOnly,
            ),
        );
        let output = catalog
            .execute(entry.tool, arguments, &context)
            .unwrap_or_else(|error| panic!("{bare} must answer its probe: {error}"));

        assert!(
            !output.content.contains(PER_FILE_PROBE_SECRET),
            "{bare} reported a denied file under {}: {}",
            probe.arguments,
            output.content
        );
        assert!(
            output.content.contains(probe.still_reports),
            "{bare} must still report what no rule denies under {}, got: {}",
            probe.arguments,
            output.content
        );
    }

    fs::remove_dir_all(temporary).unwrap();
}

/// A skill fixture holding one project skill and one global skill, each with a
/// reference file of its own, and the tool that reads them.
///
/// The two origins are what this fixture exists for: `discover_skill_catalog`
/// passes both roots, and only the project one produces files a rule written
/// against a path can name.
fn skill_tool_over_both_roots(temporary: &Path) -> (std::path::PathBuf, SkillResourceTool) {
    let project_root = temporary.join("project");
    let global_root = temporary.join("global/skills");

    for (root, name, description) in [
        (
            project_root.join(".agens/skills"),
            "probe",
            "project skill probe",
        ),
        (global_root.clone(), "elsewhere", "global skill probe"),
    ] {
        let directory = root.join(name);
        fs::create_dir_all(directory.join("references")).unwrap();
        fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\ninstructions\n"),
        )
        .unwrap();
        fs::write(
            directory.join("references/notes.md"),
            "reference contents\n",
        )
        .unwrap();
    }

    let catalog =
        agens_tools::SkillCatalog::discover(&global_root, project_root.join(".agens/skills"))
            .expect("the probe skill catalog must discover")
            .catalog()
            .clone();

    (
        project_root.clone(),
        SkillResourceTool::new(catalog, project_root),
    )
}

/// Runs one call through a dispatcher holding `tool` under `identity`, decided
/// by a single configured rule, and reports whether the rule refused it.
fn configured_rule_denies_a_call(
    identity: &'static str,
    tool: impl DispatchTool + 'static,
    entry: &str,
    arguments: serde_json::Value,
) -> bool {
    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native(identity, ToolAccess::ReadOnly, tool)
        .expect("the probe dispatcher must accept the tool");
    let dispatcher = Arc::new(Mutex::new(dispatcher));

    let policy = permission_policy(
        &configured_entries(&[entry]),
        "project",
        PermissionMode::Edit,
        &dispatcher,
        None,
    )
    .expect("the configured policy must resolve");
    let outcome = dispatcher
        .lock()
        .expect("dispatcher must be available")
        .evaluate(
            &policy,
            &[],
            &PermissionSession::new(),
            ToolDispatchRequest::new("project", identity, arguments),
        )
        .expect("the call must be decidable");

    matches!(outcome, ToolEvaluationOutcome::Denied)
}

/// Every [`ReachProbe`] in [`TOOL_SURFACE`] is executed here, so a row claiming
/// a rule reaches past its target has the call that proves it.
///
/// A tool reporting nothing beyond its target fails the first case; a tool
/// reporting the whole worktree fails the second.
#[test]
fn every_tool_this_table_says_a_rule_reaches_past_its_target_is_denied_by_naming_the_file() {
    let probed = TOOL_SURFACE
        .iter()
        .filter(|entry| entry.partly_reached.is_some())
        .map(|entry| entry.tool)
        .collect::<Vec<_>>();

    assert_eq!(
        probed,
        vec!["native::skill"],
        "this probe builds a skill call; a row added here needs a fixture of its own"
    );
    let probe = TOOL_SURFACE
        .iter()
        .find_map(|entry| entry.partly_reached.as_ref())
        .expect("the row asserted above carries the probe");

    let temporary = agens_fixtures::session_directory("skill-reach-probe");
    let arguments: serde_json::Value =
        serde_json::from_str(probe.arguments).expect("a probe names its call in JSON");

    for (target, denied) in [(probe.denied_by, true), (probe.unmatched_by, false)] {
        let (_, tool) = skill_tool_over_both_roots(&temporary);

        assert_eq!(
            configured_rule_denies_a_call(
                "native::skill",
                tool,
                &format!("deny skill {target}"),
                arguments.clone(),
            ),
            denied,
            "`deny skill({target})` must {} the call {}",
            if denied { "refuse" } else { "not select" },
            probe.arguments
        );
    }

    fs::remove_dir_all(temporary).unwrap();
}

/// Skills come from two roots, and one rule has to answer for both the way the
/// `skill` row says it does: it binds the project skill and leaves the global
/// one to the bound on what the tool can open.
///
/// This is the exception's residual, executed rather than asserted. The global
/// root of this fixture sits outside the project root, as a global skills
/// directory ordinarily does, so the file that skill opens has no
/// project-relative spelling for a rule to name. Where the two roots are nested
/// the other way the spelling exists and the rule binds, which is why the
/// exception is stated over the paths rather than over the origin.
#[test]
fn one_path_rule_reaches_a_project_skills_files_and_not_a_global_skills() {
    let temporary = agens_fixtures::session_directory("skill-reach-residual");

    for (skill, denied) in [("probe", true), ("elsewhere", false)] {
        for (call, arguments) in [
            ("instructions", serde_json::json!({"skill": skill})),
            (
                "reference",
                serde_json::json!({
                    "skill": skill,
                    "resource_class": "reference",
                    "resource": "notes.md",
                }),
            ),
        ] {
            let (_, tool) = skill_tool_over_both_roots(&temporary);

            assert_eq!(
                configured_rule_denies_a_call(
                    "native::skill",
                    tool,
                    "deny skill **/*.md",
                    arguments
                ),
                denied,
                "`deny skill(**/*.md)` must {} the {skill} skill's own {call}",
                if denied { "refuse" } else { "not select" }
            );
        }
    }

    fs::remove_dir_all(temporary).unwrap();
}

/// The rules are asked about every file the call walks, before the caller's own
/// `glob` decides which of them come back. So a `grep` filtered to `notes.md`
/// still says a file was withheld when the denied file is `secrets.md` — the
/// notice answers for what the call reached, not for what it reported.
///
/// That order is deliberate and this pins it. Asking the rules after the
/// filter would make the notice exact, and would also turn the filter into a
/// way of narrowing the withheld set one filename at a time; the documented
/// limit — that re-scoping narrows it to a directory — holds only because it
/// does not.
#[test]
fn the_rules_decide_every_file_a_grep_walks_before_its_own_filter_decides_what_returns() {
    let temporary = agens_fixtures::session_directory("grep-filter-order");
    let project_root = temporary.join("project");
    worktree_holding_a_denied_secret(&project_root, &temporary.join("git"));

    let catalog = NativeToolCatalog::new(
        NativeTools::open(&project_root).expect("the probe worktree must open as a project root"),
    );
    let (policy, identity) =
        configured_child_policy(&configured_rules(&["deny grep **/secrets.md"]), &[], "grep")
            .expect("a delegated child must be able to reach grep");
    let context = ToolExecutionContext::with_timeout(Duration::from_secs(30)).with_read_filter(
        PermissionReadFilter::new(
            policy,
            Vec::new(),
            "project",
            identity,
            ToolAccess::ReadOnly,
        ),
    );

    let output = catalog
        .execute(
            "native::grep",
            serde_json::json!({"pattern":"OPENAI_API_KEY","glob":"notes.md"}),
            &context,
        )
        .expect("grep must answer");

    assert!(
        !output.content.contains(PER_FILE_PROBE_SECRET),
        "the denied file must stay withheld under any filter: {}",
        output.content
    );
    assert!(
        output.content.contains("notes.md:1:OPENAI_API_KEY is set"),
        "the filter must still return what it selects: {}",
        output.content
    );
    assert!(
        output.content.contains("some files were not read"),
        "the notice answers for every file the call walked, so a filter excluding the \
         denied file must not silence it: {}",
        output.content
    );

    fs::remove_dir_all(temporary).unwrap();
}

/// The rule an operator is told to write has to be the rule that binds.
///
/// A `task` call is named by the agent it resolves to: `TaskTool` parses the
/// invocation, resolves the agent — the requested one, or the default when the
/// call names none — and answers with that agent's own name. A rule written
/// against the description the call carries therefore selects nothing, which is
/// the spelling both documents used to recommend.
#[test]
fn a_task_rule_names_the_agent_a_call_resolves_to_and_not_its_description() {
    let temporary = agens_fixtures::session_directory("task-permission-target");
    let agents = temporary.join("agents");
    fs::create_dir_all(&agents).unwrap();
    fs::write(
        agents.join("worker.md"),
        "---\nname: worker\ndescription: worker agent\nmode: subagent\n---\nworker instructions\n",
    )
    .unwrap();

    let discovery = AgentCatalog::discover(&[], &agents, &temporary.join("missing")).unwrap();
    let task = TaskTool::from_catalogs_with_model_validator(
        discovery.catalog().clone(),
        agens_tools::SkillCatalog::default(),
        "parent-model",
        EveryModel,
        InertTaskRunner,
    );
    let arguments = serde_json::json!({"description": "inspect the repository"});

    assert_eq!(
        task.permission_target(&arguments)
            .expect("a task call must project to a permission target"),
        "worker",
        "the target is the agent the call resolves to"
    );

    let mut dispatcher = ToolDispatcher::new();
    dispatcher
        .register_native("native::task", ToolAccess::Write, task)
        .expect("the probe dispatcher must accept the task tool");
    let dispatcher = Arc::new(Mutex::new(dispatcher));

    for (entry, denied) in [
        ("deny task worker", true),
        ("deny task inspect the repository", false),
        ("deny task inspect*", false),
    ] {
        let policy = permission_policy(
            &configured_entries(&[entry]),
            "project",
            PermissionMode::Edit,
            &dispatcher,
            None,
        )
        .expect("the configured policy must resolve");
        let outcome = dispatcher
            .lock()
            .expect("dispatcher must be available")
            .evaluate(
                &policy,
                &[],
                &PermissionSession::new(),
                ToolDispatchRequest::new("project", "native::task", arguments.clone()),
            )
            .expect("a task call must be decidable");

        assert_eq!(
            matches!(outcome, ToolEvaluationOutcome::Denied),
            denied,
            "`{entry}` must {} this call",
            if denied { "deny" } else { "not select" }
        );
    }

    fs::remove_dir_all(temporary).unwrap();
}

struct EveryModel;

impl agens_tools::AgentModelValidator for EveryModel {
    fn validate_model(&self, _: &str) -> Result<(), agens_tools::AgentModelValidationError> {
        Ok(())
    }
}

struct InertTaskRunner;

impl agens_tools::TaskRunner for InertTaskRunner {
    fn run(
        &self,
        request: agens_tools::TaskTurnRequest,
        _: &agens_tools::TaskRunContext,
    ) -> Result<agens_tools::TaskTurnResult, agens_tools::TaskRunnerError> {
        Ok(agens_tools::TaskTurnResult {
            output: request.description().to_owned(),
            iterations: 1,
        })
    }
}

/// The native surface a production primary agent holds, as the names and access
/// classes read off the production dispatcher itself.
///
/// Built once for the whole binary: a probe dispatcher is wanted per case, and
/// building a session's runtime for each of them would rebuild a bootstrap per
/// row.
fn production_parent_natives() -> &'static [(String, ToolAccess)] {
    static SURFACE: std::sync::OnceLock<Vec<(String, ToolAccess)>> = std::sync::OnceLock::new();

    SURFACE.get_or_init(|| {
        let temporary = agens_fixtures::session_directory("production-parent-natives");
        let bootstrap = agens_fixtures::session_bootstrap(&temporary, &[]);
        let project_root = agens_bootstrap::session_root::discovered_root_for_tests(&bootstrap);

        let (_, parent) = agens_tool_runtime::runtime::production_tool_runtime_with_task_runner(
            &bootstrap,
            &project_root,
            Some(&agens_tools::SkillCatalog::default()),
            InertTaskRunner,
        )
        .expect("the parent runtime must build");

        let parent = parent.lock().expect("dispatcher must be available");
        let mut surface = parent
            .registered_native_names()
            .into_iter()
            .map(|name| {
                let access = parent
                    .native_access(&name)
                    .expect("a name the dispatcher reported must resolve");
                (name, access)
            })
            .collect::<Vec<_>>();
        surface.sort_by(|left, right| left.0.cmp(&right.0));

        drop(parent);
        fs::remove_dir_all(temporary).unwrap();

        surface
    })
}

/// A dispatcher holding exactly the natives a production primary agent holds,
/// under the access classes it holds them by.
///
/// The two paths have to compare over the surface that exists. A harness built
/// from `NativeToolCatalog::metadata()` holds ten, while the primary path holds
/// fifteen — and `permission_policy` refuses a configured name its dispatcher
/// cannot resolve, so the five it left out were names no parent-path case could
/// even be written against.
fn native_dispatcher() -> ToolDispatcher {
    let mut dispatcher = ToolDispatcher::new();
    for (name, access) in production_parent_natives() {
        dispatcher
            .register_native(name.clone(), *access, InertTool)
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

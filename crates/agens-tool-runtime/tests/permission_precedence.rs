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
    ToolDispatcher, ToolExecutionContext, ToolOutput,
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

    surface
        .tools
        .iter()
        .any(|entry| entry.qualified_name == qualified)
        .then(|| {
            (
                PermissionPolicy::new(PermissionMode::Edit, surface.rules)
                    .with_configured_floor(surface.configured_floor),
                qualified,
            )
        })
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

#[test]
fn the_child_path_and_the_parent_path_decide_every_declaration_shape_identically() {
    let mut disagreements = Vec::new();

    for case in CASES {
        let declarations = parsed_declarations(case.declarations);

        let child = child_decision(&declarations, case.tool, case.target, case.path);
        let parent = parent_decision(&declarations, case.tool, case.target, case.path);

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
    /// A line the same call must still return, so a tool cannot pass the probe
    /// by returning nothing at all.
    still_reports: &'static str,
}

/// One native tool, classified by the two facts that decide whether a path deny
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
}

/// The whole native surface, classified.
///
/// This table exists because the enumeration is what keeps being got wrong: a
/// tool that returns the contents of a file and is decided neither way is a
/// path deny that does not bind, and three separate passes over this surface
/// missed one. Adding a native tool without adding a row here fails the test
/// below, which is the point — the classification is the decision, and it has
/// to be made deliberately rather than inherited.
///
/// A row's per-file claim is executed rather than restated: it carries the call
/// that proves it, run by
/// [`every_tool_this_table_says_asks_per_file_withholds_a_denied_file`]. What
/// each rule decides is pinned separately, by [`REPORTED_FILE_CASES`] and by the
/// shipped-configuration turns in `child.rs`.
const TOOL_SURFACE: &[SurfaceEntry] = &[
    SurfaceEntry {
        tool: "native::read",
        returns_file_contents: true,
        decided_on_its_target: true,
        decided_per_file: None,
    },
    SurfaceEntry {
        tool: "native::write",
        returns_file_contents: false,
        decided_on_its_target: true,
        decided_per_file: None,
    },
    // `edit` reports the region it rewrote, which is the file's own text.
    SurfaceEntry {
        tool: "native::edit",
        returns_file_contents: true,
        decided_on_its_target: true,
        decided_per_file: None,
    },
    SurfaceEntry {
        tool: "native::list",
        returns_file_contents: false,
        decided_on_its_target: true,
        decided_per_file: None,
    },
    SurfaceEntry {
        tool: "native::search",
        returns_file_contents: true,
        decided_on_its_target: true,
        decided_per_file: Some(PerFileProbe {
            arguments: r#"{"path":".","query":"OPENAI_API_KEY"}"#,
            still_reports: "OPENAI_API_KEY is set",
        }),
    },
    SurfaceEntry {
        tool: "native::grep",
        returns_file_contents: true,
        decided_on_its_target: true,
        decided_per_file: Some(PerFileProbe {
            arguments: r#"{"pattern":"OPENAI_API_KEY"}"#,
            still_reports: "OPENAI_API_KEY is set",
        }),
    },
    // `glob` reports the paths its pattern names and never their contents. Its
    // pattern is matched as text rather than as the set it denotes, which is a
    // separate limitation over names, not over contents.
    SurfaceEntry {
        tool: "native::glob",
        returns_file_contents: false,
        decided_on_its_target: false,
        decided_per_file: None,
    },
    // `git_read` is named by an operation keyword, so no rule written against
    // it selects a file; the files its `diff` reports are decided one by one.
    SurfaceEntry {
        tool: "native::git_read",
        returns_file_contents: true,
        decided_on_its_target: false,
        decided_per_file: Some(PerFileProbe {
            arguments: r#"{"operation":"diff"}"#,
            still_reports: "notes: second",
        }),
    },
    // `webfetch` returns HTTP and HTTPS responses and cannot address a file.
    SurfaceEntry {
        tool: "native::webfetch",
        returns_file_contents: false,
        decided_on_its_target: false,
        decided_per_file: None,
    },
    // `bash` prints whatever the command it runs prints, and its target is that
    // command line rather than a path. It is the one tool no path deny binds.
    SurfaceEntry {
        tool: "native::bash",
        returns_file_contents: true,
        decided_on_its_target: false,
        decided_per_file: None,
    },
];

/// The surface the table classifies has to be the surface that exists, and
/// every tool on it that can return the contents of a file has to be decided
/// one of the two ways — with `bash` named as the single exception rather than
/// left implicit.
#[test]
fn every_native_tool_that_returns_file_contents_is_reached_by_a_path_rule() {
    let mut classified = TOOL_SURFACE
        .iter()
        .map(|entry| entry.tool)
        .collect::<Vec<_>>();
    let mut registered = NativeToolCatalog::metadata()
        .into_iter()
        .map(|entry| entry.qualified_name)
        .collect::<Vec<_>>();
    classified.sort_unstable();
    registered.sort_unstable();

    assert_eq!(
        classified, registered,
        "every native tool must be classified here, and every row must name a real tool"
    );

    let unreached = TOOL_SURFACE
        .iter()
        .filter(|entry| {
            entry.returns_file_contents
                && !entry.decided_on_its_target
                && entry.decided_per_file.is_none()
        })
        .map(|entry| entry.tool)
        .collect::<Vec<_>>();

    assert_eq!(
        unreached,
        vec!["native::bash"],
        "a tool that returns the contents of a file and is decided neither on its target \
         nor per file is a path deny that does not bind"
    );

    for entry in TOOL_SURFACE {
        let bare = entry.tool.trim_start_matches("native::");
        let expected = if bare == "bash" {
            PermissionTargetKind::FreeFormText
        } else {
            PermissionTargetKind::Path
        };

        assert_eq!(
            permission_target_kind_for_tool(bare),
            expected,
            "{bare} is matched under the wrong target shape"
        );
    }
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

/// A worktree every probe can be run over: `.env` holds the secret and
/// `notes.md` holds a line naming the same key, both reachable by a walk, by a
/// pattern and by a diff against the commit that holds neither.
///
/// The repository keeps its metadata outside the worktree, because a probe that
/// walks every file under the root would otherwise walk git's own storage and
/// fail on bytes that are not text.
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

    fs::write(project_root.join(".env"), "OPENAI_API_KEY=placeholder\n").unwrap();
    fs::write(project_root.join("notes.md"), "notes: first\n").unwrap();
    probe_git(project_root, &["add", "-A"]);
    probe_git(project_root, &["commit", "--quiet", "-m", "first"]);

    fs::write(
        project_root.join(".env"),
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
        let rule = format!("deny {bare} **/.env");
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
        let arguments =
            serde_json::from_str(probe.arguments).expect("a probe names its call in JSON");
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

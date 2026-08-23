//! The hard denylist: the acts a worker never takes on its own authority,
//! whatever its permission configuration says.
//!
//! Level 1 confines a session to a root and level 2 resolves what its rules
//! and grants say about a call. Neither answers the question this module does:
//! some acts cost the same whoever authorized them. Pushing a branch, deleting
//! outside the worktree, escalating privilege, reading a credential, taking an
//! irreversible operation, stopping the server that hosts the run — a
//! configuration that allows those allows them by accident, because nobody
//! writes a permission rule with those cases in mind.
//!
//! A match is not a refusal. It takes the decision away from the worker and
//! gives it to a person: for a run, the call becomes a durable question and the
//! run parks on it. What comes back is an answer, not a retry.
//!
//! # What the match is written against
//!
//! A command, never prose. Every classification below is anchored to a concrete
//! program name, a concrete flag, or a concrete path, because the text this
//! reads is a shell command line while the text a run carries is a task
//! description written for a model. A matcher wide enough to catch "investigate
//! the gateway restart" in prose would fire on the description and still miss
//! the invocation, which is the only thing that matters.
//!
//! It reads a command line the same way [`crate::permission_target`] does, and
//! carries the same caveat: it closes the ordinary spellings of an act, not an
//! adversary hiding one. `sudo`, `/bin/rm`, `sh -c` and `a && b` are seen
//! through; a program invoked through a variable it computed is not.

use std::path::{Component, Path, PathBuf};

use crate::{
    PermissionRequest, PermissionTargetKind, bare_tool_name, permission_target,
    permission_target_kind_for_tool,
};

/// Why a call was taken out of the worker's hands.
///
/// The class travels with the escalation so the question a person answers says
/// what kind of act it was, rather than only which command produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenylistClass {
    /// Publishing work to a remote: the one act a run takes that another person
    /// sees before anyone has reviewed it.
    GitPush,
    /// Removing something the run's worktree does not contain.
    DeletionOutsideWorktree,
    /// Acquiring authority the session was not started with.
    PrivilegeEscalation,
    /// Reading or writing a credential, a key, or a secret store.
    SecretsAccess,
    /// An operation classified irreversible — a restore, a wipe, a data
    /// migration, a mass delete — whether or not it looks in scope.
    IrreversibleOperation,
    /// Stopping, restarting, killing or reconfiguring the server that hosts the
    /// run. The run comes back from its own checkpoint and takes the act again,
    /// so the loop closes on itself.
    ServerLifecycle,
    /// An act reaching outside the run's declared scope, including delegating
    /// that act to a sub-agent.
    OutOfScope,
}

impl DenylistClass {
    /// The stable identifier this class is recorded and matched under.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::GitPush => "git_push",
            Self::DeletionOutsideWorktree => "deletion_outside_worktree",
            Self::PrivilegeEscalation => "privilege_escalation",
            Self::SecretsAccess => "secrets_access",
            Self::IrreversibleOperation => "irreversible_operation",
            Self::ServerLifecycle => "server_lifecycle",
            Self::OutOfScope => "out_of_scope",
        }
    }

    /// What a person reading the question is being asked about.
    #[must_use]
    pub const fn headline(self) -> &'static str {
        match self {
            Self::GitPush => "publish work to a remote",
            Self::DeletionOutsideWorktree => "delete something outside its worktree",
            Self::PrivilegeEscalation => "run with escalated privilege",
            Self::SecretsAccess => "reach a credential, a key or a secret store",
            Self::IrreversibleOperation => "take an irreversible operation",
            Self::ServerLifecycle => "stop, restart or reconfigure the server hosting it",
            Self::OutOfScope => "act outside its declared scope",
        }
    }
}

/// The denylist as one worker's copy of it, bound to the worktree that worker's
/// scope is measured against.
///
/// The worktree is what makes "outside" mean anything: the same `rm` is
/// ordinary inside the run's own checkout and is a deletion of somebody else's
/// work one directory up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denylist {
    worktree: PathBuf,
}

impl Denylist {
    #[must_use]
    pub fn new(worktree: impl Into<PathBuf>) -> Self {
        Self {
            worktree: worktree.into(),
        }
    }

    #[must_use]
    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    /// Which class of act this call is, or `None` when it is an ordinary one.
    ///
    /// A path-shaped call is judged by the file it names. A command line is
    /// judged one invocation at a time, and the first class any of them matches
    /// is the answer: a compound command is every act it runs, so
    /// `git status && git push` is a push.
    #[must_use]
    pub fn classify(&self, request: &PermissionRequest) -> Option<DenylistClass> {
        let tool = bare_tool_name(&request.tool);

        if tool == "bash" {
            return self.classify_command(&request.target);
        }

        match permission_target_kind_for_tool(&tool) {
            PermissionTargetKind::Path => self.classify_path(&request.target),
            PermissionTargetKind::FreeFormText => None,
        }
    }

    /// Whether this call reaches a path the run's worktree does not contain.
    ///
    /// This is what sets [`PermissionRequest::outside_worktree`] for a worker,
    /// so the confinement floor stops being a predicate nothing ever triggers.
    /// A command line is deliberately excluded: `bash`'s target is free-form
    /// text whose operands are a reading rather than a fact, and a reading must
    /// not reach a hard deny. The same operands do reach [`Self::classify`],
    /// where the answer is a question rather than a refusal.
    #[must_use]
    pub fn escapes_worktree(&self, request: &PermissionRequest) -> bool {
        let tool = bare_tool_name(&request.tool);

        if tool == "bash" {
            return false;
        }

        matches!(
            permission_target_kind_for_tool(&tool),
            PermissionTargetKind::Path
        ) && self.escapes(&request.target)
    }

    fn classify_path(&self, target: &str) -> Option<DenylistClass> {
        if names_a_secret(target) {
            return Some(DenylistClass::SecretsAccess);
        }
        if names_a_service_unit(target) {
            return Some(DenylistClass::ServerLifecycle);
        }
        if self.escapes(target) {
            return Some(DenylistClass::OutOfScope);
        }

        None
    }

    fn classify_command(&self, command: &str) -> Option<DenylistClass> {
        permission_target::command_invocation_tokens(command)
            .iter()
            .find_map(|tokens| self.classify_invocation(tokens))
            .or_else(|| classify_embedded_statement(command))
    }

    fn classify_invocation(&self, tokens: &[String]) -> Option<DenylistClass> {
        if let Some(program) = program_of(tokens)
            && PRIVILEGE_ESCALATION.contains(&program)
        {
            return Some(DenylistClass::PrivilegeEscalation);
        }

        let tokens = permission_target::without_wrapper_prefixes(tokens);
        let program = program_of(tokens)?;
        let arguments = tokens.get(1..).unwrap_or_default();

        if let Some(class) = classify_program(program, arguments) {
            return Some(class);
        }

        self.classify_operands(program, arguments)
    }

    /// What the paths an invocation names say about it, once its program has
    /// said nothing on its own.
    fn classify_operands(&self, program: &str, arguments: &[String]) -> Option<DenylistClass> {
        arguments
            .iter()
            .filter(|argument| looks_like_a_path(argument))
            .find_map(|operand| {
                if names_a_secret(operand) {
                    return Some(DenylistClass::SecretsAccess);
                }
                if names_a_service_unit(operand) {
                    return Some(DenylistClass::ServerLifecycle);
                }
                if !self.escapes(operand) {
                    return None;
                }

                Some(if DELETION_PROGRAMS.contains(&program) {
                    DenylistClass::DeletionOutsideWorktree
                } else {
                    DenylistClass::OutOfScope
                })
            })
    }

    /// Whether `value` names something the worktree does not contain.
    ///
    /// Decided lexically, without touching the filesystem: the answer has to be
    /// the same for a path that does not exist yet as for one that does, and a
    /// worker naming a path is frequently naming the first.
    fn escapes(&self, value: &str) -> bool {
        if value.is_empty() {
            return false;
        }
        if value == "~" || value.starts_with("~/") {
            return true;
        }

        let path = Path::new(value);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.worktree.join(path)
        };

        !lexically_resolved(&joined).starts_with(lexically_resolved(&self.worktree))
    }
}

/// Programs that acquire authority the session did not start with.
const PRIVILEGE_ESCALATION: [&str; 5] = ["sudo", "doas", "su", "pkexec", "runas"];

/// Programs whose operands name something being removed.
const DELETION_PROGRAMS: [&str; 5] = ["rm", "rmdir", "unlink", "shred", "srm"];

/// Programs that destroy or overwrite storage outright.
const DESTRUCTIVE_PROGRAMS: [&str; 7] = ["dd", "mkfs", "wipefs", "fdisk", "parted", "shred", "srm"];

/// Programs that supervise long-lived services.
const SERVICE_MANAGERS: [&str; 5] = ["systemctl", "launchctl", "service", "rc-service", "sv"];

/// The verbs those managers use to end or restart a service.
const SERVICE_LIFECYCLE_VERBS: [&str; 8] = [
    "stop",
    "restart",
    "kill",
    "disable",
    "mask",
    "unload",
    "daemon-reload",
    "reload",
];

/// Programs that end a process by name or by signal.
const SIGNAL_PROGRAMS: [&str; 4] = ["kill", "pkill", "killall", "skill"];

/// What the coordinator's own executable is called, so a run cannot stop the
/// server hosting it through the CLI it was given.
const SERVER_EXECUTABLE: &str = "agens";

/// The verbs that end or restart that server.
const SERVER_LIFECYCLE_VERBS: [&str; 4] = ["stop", "restart", "shutdown", "kill"];

/// Programs whose whole purpose is to write a snapshot back over live data.
const RESTORE_PROGRAMS: [&str; 3] = ["pg_restore", "mysqlimport", "mongorestore"];

/// Classifies an invocation by what its program and its flags say it does.
fn classify_program(program: &str, arguments: &[String]) -> Option<DenylistClass> {
    let names = |value: &str| arguments.iter().any(|entry| entry == value);
    let any_contains = |value: &str| arguments.iter().any(|entry| entry.contains(value));

    if program == "git" && names("push") {
        return Some(DenylistClass::GitPush);
    }

    if program == SERVER_EXECUTABLE
        && arguments
            .iter()
            .any(|entry| SERVER_LIFECYCLE_VERBS.contains(&entry.as_str()))
    {
        return Some(DenylistClass::ServerLifecycle);
    }

    if SERVICE_MANAGERS.contains(&program)
        && arguments
            .iter()
            .any(|entry| SERVICE_LIFECYCLE_VERBS.contains(&entry.as_str()))
    {
        return Some(DenylistClass::ServerLifecycle);
    }

    if SIGNAL_PROGRAMS.contains(&program) && any_contains(SERVER_EXECUTABLE) {
        return Some(DenylistClass::ServerLifecycle);
    }

    if DESTRUCTIVE_PROGRAMS.contains(&program) || RESTORE_PROGRAMS.contains(&program) {
        return Some(DenylistClass::IrreversibleOperation);
    }

    if program == "git" && ((names("reset") && names("--hard")) || names("filter-branch")) {
        return Some(DenylistClass::IrreversibleOperation);
    }

    if program == "terraform" && (names("apply") || names("destroy")) {
        return Some(DenylistClass::IrreversibleOperation);
    }

    if program == "kubectl" && names("delete") {
        return Some(DenylistClass::IrreversibleOperation);
    }

    if program == "aws" && (any_contains("delete") || any_contains("restore") || names("rb")) {
        return Some(DenylistClass::IrreversibleOperation);
    }

    if names("restore") || names("wipe") || arguments.iter().any(|entry| names_a_migration(entry)) {
        return Some(DenylistClass::IrreversibleOperation);
    }

    None
}

/// Whether an argument names a schema or data migration, in the spellings the
/// migration tools people actually run use for it.
fn names_a_migration(argument: &str) -> bool {
    let argument = argument.trim_start_matches('-');

    argument == "migrate"
        || argument == "migration"
        || argument.starts_with("migrate:")
        || argument.starts_with("db:migrate")
}

/// SQL that destroys a table or a rowset, written inline in a command line.
///
/// Read off the whole line rather than one invocation, because the statement
/// reaches its client as a single quoted argument and carries no program name
/// of its own.
fn classify_embedded_statement(command: &str) -> Option<DenylistClass> {
    let upper = command.to_ascii_uppercase();
    let destroys = upper.contains("DROP TABLE")
        || upper.contains("DROP DATABASE")
        || upper.contains("DROP SCHEMA")
        || upper.contains("TRUNCATE ")
        || (upper.contains("DELETE FROM") && !upper.contains("WHERE"));

    destroys.then_some(DenylistClass::IrreversibleOperation)
}

const SECRET_DIRECTORIES: [&str; 4] = [".ssh", ".aws", ".gnupg", ".docker"];

const SECRET_NAMES: [&str; 12] = [
    ".env",
    ".netrc",
    ".pgpass",
    ".npmrc",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "credentials",
    "secrets.json",
    "secrets.yaml",
    "secrets.yml",
];

const SECRET_EXTENSIONS: [&str; 8] = [
    "pem", "key", "p12", "pfx", "jks", "keystore", "tfstate", "tfvars",
];

/// Whether a path names a credential, a key or a secret store.
///
/// Matched on the basename and on the directories walked through, so the same
/// answer holds however the path was spelled.
fn names_a_secret(value: &str) -> bool {
    let path = Path::new(value);

    let in_a_secret_directory = path.components().any(|component| {
        matches!(component, Component::Normal(name)
            if SECRET_DIRECTORIES.contains(&name.to_string_lossy().as_ref()))
    });
    if in_a_secret_directory {
        return true;
    }

    let Some(name) = path.file_name().map(|name| name.to_string_lossy()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();

    if SECRET_NAMES.contains(&name.as_str()) || name.starts_with(".env.") {
        return true;
    }

    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|extension| SECRET_EXTENSIONS.contains(&extension.as_str()))
}

/// Whether a path names the service definition that supervises a process.
///
/// Editing one is how a restart survives the run that caused it, which is why
/// it is classified with stopping the server rather than with writing a file.
fn names_a_service_unit(value: &str) -> bool {
    let path = Path::new(value);

    let Some(name) = path.file_name().map(|name| name.to_string_lossy()) else {
        return false;
    };

    let in_a_unit_directory = path.components().any(|component| {
        matches!(component, Component::Normal(directory)
        if matches!(
            directory.to_string_lossy().as_ref(),
            "systemd" | "LaunchAgents" | "LaunchDaemons" | "init.d"
        ))
    });

    in_a_unit_directory
        && (name.ends_with(".service") || name.ends_with(".plist") || name.ends_with(".timer"))
}

/// The program an invocation runs, with its leading environment assignments
/// stepped over so `FOO=1 sudo rm` still reads as `sudo`.
fn program_of(tokens: &[String]) -> Option<&str> {
    tokens
        .iter()
        .find(|token| !permission_target::is_environment_assignment(token))
        .map(|token| permission_target::command_name(token))
}

/// Whether an argument is naming a path rather than a flag or a bare word.
///
/// A bare word is deliberately excluded: `rm build` inside the worktree names
/// something the worktree contains, and reading it as a path would say nothing
/// this module acts on anyway.
fn looks_like_a_path(argument: &str) -> bool {
    !argument.starts_with('-')
        && (argument.starts_with('/')
            || argument.starts_with('~')
            || argument.starts_with("./")
            || argument.starts_with("../")
            || argument.contains('/'))
}

/// Resolves `.` and `..` lexically, leaving whatever the path walks through
/// alone. A `..` that would climb past the root is dropped there, which is the
/// same place the filesystem stops.
fn lexically_resolved(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }

    resolved
}

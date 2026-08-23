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

    /// Reads a command line as the sequence of invocations it runs, carrying
    /// the working directory forward across it.
    ///
    /// `bash` runs the whole line with the worktree as its working directory,
    /// so a `cd` inside the line moves every operand that follows it. Judging
    /// each invocation against the worktree instead would read
    /// `cd .. && rm -rf victim` as a deletion of something the worktree
    /// contains, which is the one thing it is not.
    fn classify_command(&self, command: &str) -> Option<DenylistClass> {
        let mut directory = self.worktree.clone();

        for tokens in permission_target::command_invocation_tokens(command) {
            if let Some(class) = self.classify_invocation(&directory, &tokens) {
                return Some(class);
            }

            if let Some(change) = directory_change(&tokens) {
                let moved = change.applied_to(&directory);

                if self.escapes_from(&directory, &moved) {
                    return Some(DenylistClass::OutOfScope);
                }

                directory = lexically_resolved(&moved);
            }
        }

        classify_embedded_statement(command)
    }

    fn classify_invocation(&self, directory: &Path, tokens: &[String]) -> Option<DenylistClass> {
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

        self.classify_operands(directory, program, arguments)
    }

    /// What the paths an invocation names say about it, once its program has
    /// said nothing on its own.
    ///
    /// An argument that does not read as a path is not silently taken for one
    /// relative to the working directory. `python -c "os.remove('/etc/passwd')"`
    /// is a program the matcher cannot read, but the absolute paths written
    /// inside it are still paths, and those are what it is judged by.
    fn classify_operands(
        &self,
        directory: &Path,
        program: &str,
        arguments: &[String],
    ) -> Option<DenylistClass> {
        arguments
            .iter()
            .filter(|argument| !argument.starts_with('-'))
            .find_map(|argument| {
                if looks_like_a_path(argument) {
                    return self.classify_operand(directory, program, argument);
                }

                embedded_paths(argument)
                    .into_iter()
                    .find_map(|embedded| self.classify_operand(directory, program, embedded))
            })
    }

    fn classify_operand(
        &self,
        directory: &Path,
        program: &str,
        operand: &str,
    ) -> Option<DenylistClass> {
        if names_a_secret(operand) {
            return Some(DenylistClass::SecretsAccess);
        }
        if names_a_service_unit(operand) {
            return Some(DenylistClass::ServerLifecycle);
        }
        if !self.escapes_from(directory, Path::new(operand)) {
            return None;
        }

        Some(if DELETION_PROGRAMS.contains(&program) {
            DenylistClass::DeletionOutsideWorktree
        } else {
            DenylistClass::OutOfScope
        })
    }

    /// Whether `value` names something the worktree does not contain, read
    /// from `directory` the way the shell would read it.
    ///
    /// Decided lexically, without touching the filesystem: the answer has to be
    /// the same for a path that does not exist yet as for one that does, and a
    /// worker naming a path is frequently naming the first.
    fn escapes_from(&self, directory: &Path, value: &Path) -> bool {
        if value.as_os_str().is_empty() {
            return false;
        }
        if names_the_home_directory(value) {
            return true;
        }

        let joined = if value.is_absolute() {
            value.to_path_buf()
        } else {
            directory.join(value)
        };

        !lexically_resolved(&joined).starts_with(lexically_resolved(&self.worktree))
    }

    fn escapes(&self, value: &str) -> bool {
        self.escapes_from(&self.worktree, Path::new(value))
    }
}

/// Where a `cd` invocation leaves the working directory of the line it is part
/// of.
enum DirectoryChange {
    /// A `cd` with an operand, which the shell resolves the same way it
    /// resolves any other path.
    To(PathBuf),
    /// A `cd` with nothing to resolve — bare, or back to a directory only the
    /// shell remembers. Neither is a place inside the worktree that can be
    /// named, so both are read as leaving it.
    Away,
}

impl DirectoryChange {
    fn applied_to(&self, directory: &Path) -> PathBuf {
        match self {
            Self::To(target) if target.is_absolute() => target.clone(),
            Self::To(target) => directory.join(target),
            Self::Away => PathBuf::from("~"),
        }
    }
}

/// Reads an invocation as a change of working directory, or as nothing when it
/// is an ordinary command.
fn directory_change(tokens: &[String]) -> Option<DirectoryChange> {
    let tokens = permission_target::without_wrapper_prefixes(tokens);
    let program = program_of(tokens)?;
    if !DIRECTORY_PROGRAMS.contains(&program) {
        return None;
    }

    if program == "popd" {
        return Some(DirectoryChange::Away);
    }

    let operand = tokens
        .iter()
        .skip(1)
        .find(|argument| !argument.starts_with('-'));

    Some(match operand {
        Some(operand) if !resolves_outside_the_line(operand) => {
            DirectoryChange::To(PathBuf::from(operand))
        }
        _ => DirectoryChange::Away,
    })
}

/// Programs that move the working directory of the line they are part of.
///
/// `pushd` moves it exactly as `cd` does. `popd` moves it back to a directory
/// held on a stack the matcher never sees, which is why it resolves to nowhere
/// nameable rather than to the worktree.
const DIRECTORY_PROGRAMS: [&str; 3] = ["cd", "pushd", "popd"];

/// Whether a directory operand names a place this line cannot resolve.
///
/// The home directory is one: no worktree contains it. A word the shell expands
/// before the directory exists as text is the other — `cd "$HOME"` and
/// `` cd `dirname $PWD` `` are directories chosen at run time, and joining the
/// unexpanded word onto the working directory would answer that the line never
/// left, which is the one thing that cannot be concluded from it.
fn resolves_outside_the_line(operand: &str) -> bool {
    operand.starts_with('~') || operand.contains('$') || operand.contains('`')
}

/// Whether a path is written against the home directory, which no worktree
/// contains and which cannot be resolved lexically.
fn names_the_home_directory(value: &Path) -> bool {
    let value = value.to_string_lossy();

    value == "~" || value.starts_with("~/")
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

/// Whether an argument is naming a path rather than a flag, a bare word, or a
/// program written for another interpreter.
///
/// A bare word is deliberately excluded: `rm build` inside the worktree names
/// something the worktree contains, and reading it as a path would say nothing
/// this module acts on anyway.
///
/// So is anything carrying shell or source syntax. `-c "os.remove('/etc/passwd')"`
/// contains a separator and would otherwise be joined onto the working
/// directory whole, which resolves to a path inside the worktree and says the
/// opposite of what the argument does. What such an argument names is read by
/// [`embedded_paths`] instead.
fn looks_like_a_path(argument: &str) -> bool {
    !argument.starts_with('-')
        && !argument.chars().any(is_not_path_syntax)
        && is_path_shaped(argument)
}

/// The paths written inside an argument that is not itself a path.
///
/// A fragment is kept when it is spelled as a path — rooted at `/` or `~`,
/// written against the working directory with `./` or `../`, or carrying a
/// separator anywhere. A bare word is dropped: inside a quoted program it is
/// prose as often as it is a file, and re-basing it onto the working directory
/// would invent a path the argument never named.
///
/// The relative ones matter as much as the absolute ones. `rm -rf ../victim/*`
/// carries a glob, so it is not one path the matcher can read whole, and the
/// only thing left saying it leaves the worktree is the `../victim/` inside it.
/// What each fragment names is decided against the same working directory the
/// invocation runs in, by [`Denylist::classify_operand`].
fn embedded_paths(argument: &str) -> Vec<&str> {
    argument
        .split(is_not_path_syntax)
        .map(|fragment| fragment.trim_end_matches(['.', ':', ',']))
        .filter(|fragment| fragment.len() > 1 && is_path_shaped(fragment))
        .collect()
}

/// Whether text is spelled the way a path is spelled, leaving aside whatever
/// shell syntax stands around it.
fn is_path_shaped(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with('~')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.contains('/')
}

/// Whether a character separates a path from the program text around it rather
/// than belonging to the path.
fn is_not_path_syntax(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '\'' | '"'
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ';'
                | ','
                | '='
                | '<'
                | '>'
                | '|'
                | '&'
                | '$'
                | '*'
                | '?'
                | '!'
        )
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

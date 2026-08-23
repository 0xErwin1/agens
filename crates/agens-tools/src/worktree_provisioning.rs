//! Provisioning a freshly created session worktree from the contract the
//! repository declares for itself.
//!
//! A worktree that git has just created holds what git tracks and nothing
//! else. What is missing is everything a repository deliberately keeps out of
//! version control — a `.env`, generated fixtures, a build cache — plus
//! whatever has to run before the checkout is usable at all. Agens carries no
//! defaults for any of it and knows nothing about ecosystems: guessing which
//! files matter or which command initializes a toolchain is worse than doing
//! nothing. The repository declares both, or neither happens.
//!
//! The contract is one file, `.agens/worktree.toml` or `.agens-worktree.toml`
//! at the repository root, holding two independent pieces:
//!
//! ```toml
//! include = """
//! .env
//! fixtures/generated/
//! """
//!
//! [[hooks]]
//! name = "devshell"
//! command = ["nix", "develop", "--no-pure-eval", "-c", "just", "prepare"]
//! timeout_seconds = 1800
//! ```
//!
//! `include` is gitignore syntax and selects from the files git reports as
//! **ignored and untracked at the same time**; anything tracked already
//! arrives through the checkout. Copying never overwrites an existing
//! destination, never descends through a symlinked directory, preserves
//! modes, and records exactly the paths it created, so a later edit of the
//! inclusion list cannot retroactively remove a path from whatever cleanup
//! protects.
//!
//! `hooks` is an ordered list, not a dependency graph: the real requirement is
//! "this one needs the previous one to have run", which declared order already
//! expresses. Hooks are executed by the caller that owns the daemon
//! environment, never by a model, and never without the explicit
//! authorization this module returns as a typed decision. They inherit the
//! full ambient environment — which is what makes ecosystem support
//! unnecessary, and which also hands provider credentials to a script the
//! repository declares, so the authorization request names every command and
//! states the inheritance. A hook exports environment to the hooks that follow
//! it by appending `KEY=value` lines to the file named by
//! `AGENS_WORKTREE_ENV`, and only names the caller allowed are accepted: an
//! export is a hook writing the next hook's environment, and `PATH` or
//! `LD_PRELOAD` written there would decide what the next command even is. The
//! exports are reported, and nothing carries them into the worker's own
//! session yet.
//!
//! A hook failure is never resolved here. The caller decides between
//! continuing — in which case the failure is recorded so the worker can be
//! told it is starting in a half-built environment — and aborting, which
//! removes the worktree and its branch.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Deserialize;

use crate::{
    PROCESS_POLL_INTERVAL, ToolExecutionContext,
    git_read::harden_environment,
    kill_process_group, read_capped, terminate_process_group, wait_for_readers,
    worktrees::{HARDENING_ARGUMENTS, SessionWorktrees, WorktreeError},
};

/// The contract inside `.agens/`, preferred because it keeps the repository
/// root free of one more dotfile.
const CONTRACT_IN_AGENS_DIRECTORY: &str = ".agens/worktree.toml";
/// The same contract at the repository root, for a repository that does not
/// want an `.agens/` directory at all.
const CONTRACT_AT_ROOT: &str = ".agens-worktree.toml";

const MAX_CONTRACT_BYTES: u64 = 256 * 1024;
const MAX_HOOKS: usize = 32;
const MAX_HOOK_NAME_CHARS: usize = 64;
const MAX_HOOK_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How many files one provisioning may create. An inclusion list that selects
/// more than this is a mistake in the declaration, and copying is the step
/// that has to stay cheap enough to be unremarkable.
const MAX_PROVISIONED_FILES: usize = 4_096;
/// How many entries the ignored-and-untracked listing may hold before the
/// scan is abandoned. It exists so a pathological repository cannot make the
/// scan run without end; the matched paths are bounded separately.
const MAX_SCANNED_ENTRIES: usize = 1_000_000;

const MAX_EXPORTED_ENVIRONMENT_BYTES: u64 = 256 * 1024;
const MAX_EXPORTED_VARIABLES: usize = 256;

/// How much of a failing hook's output is kept for the record the worker is
/// eventually shown.
const MAX_RECORDED_HOOK_OUTPUT: usize = 4 * 1024;

/// The worktree a hook is running against.
const WORKTREE_VARIABLE: &str = "AGENS_WORKTREE";
/// The file a hook appends `KEY=value` lines to in order to export environment
/// to the hooks after it and to the worker session.
const EXPORT_VARIABLE: &str = "AGENS_WORKTREE_ENV";

/// Applies a repository's own provisioning contract to a session worktree that
/// git has already created.
#[derive(Clone, Debug)]
pub struct WorktreeProvisioner {
    worktrees: SessionWorktrees,
    hook_timeout: Duration,
    /// The environment names a hook may export. Empty accepts none, which is
    /// what a caller that has declared no policy means: an export changes what
    /// the following hooks execute, so it is granted rather than assumed.
    export_allowlist: Vec<String>,
    /// The allowlist entries this provisioner refused to carry, in the order
    /// the operator wrote them. Dropping one silently leaves an operator with
    /// an allowlist that admits nothing and no way to see why.
    rejected_export_patterns: Vec<String>,
    execution_context: Option<ToolExecutionContext>,
}

/// Which worktree is being provisioned, and from where.
#[derive(Clone, Copy, Debug)]
pub struct ProvisioningRequest<'a> {
    /// The checkout the worktree was created from, and the only source files
    /// are copied out of.
    pub repository: &'a Path,
    /// The repository's identifier under the worktree data directory.
    pub repository_id: &'a str,
    /// The worktree's name under that directory.
    pub name: &'a str,
    /// The branch created with the worktree, removed together with it when a
    /// hook failure is aborted on.
    pub branch: &'a str,
}

/// The decisions provisioning is not allowed to make for itself.
///
/// Both are returned to whoever owns the interaction: this module never
/// prompts, and the absence of a surface is deliberate.
pub trait ProvisioningDecisions {
    /// Whether the declared hooks may run at all. Called once, before the
    /// first hook, and only when the contract declares at least one.
    fn authorize(&self, request: &HookAuthorizationRequest<'_>) -> HookAuthorization;

    /// What to do about a hook that did not succeed. Called once per failure,
    /// in hook order.
    fn on_hook_failure(&self, failure: &HookFailure) -> HookFailureResponse;
}

/// Everything the caller needs to describe what it is being asked to allow.
#[derive(Clone, Copy, Debug)]
pub struct HookAuthorizationRequest<'a> {
    /// The contract file that declared these hooks.
    pub contract: &'a Path,
    /// The worktree the hooks will run in.
    pub worktree: &'a Path,
    /// Every hook, in the order it will run, with the exact command line.
    pub hooks: &'a [HookPlan],
    /// Always true, and stated rather than implied: a hook receives the whole
    /// ambient environment, provider credentials included.
    pub inherits_credentials: bool,
}

/// One hook as it will actually be executed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookPlan {
    /// The name the contract gave it, used in every later record.
    pub name: String,
    /// The command and its arguments. No shell is interposed.
    pub command: Vec<String>,
    /// Where it runs, relative to the worktree root.
    pub working_directory: Option<PathBuf>,
    /// The bound this hook will be killed at.
    pub timeout: Duration,
}

/// The caller's answer to [`ProvisioningDecisions::authorize`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookAuthorization {
    Allow,
    Deny,
}

/// The caller's answer to [`ProvisioningDecisions::on_hook_failure`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookFailureResponse {
    /// Keep provisioning; the failure is recorded so the worker can be told.
    Continue,
    /// Give up on the worktree entirely, removing it and its branch.
    Abort,
}

/// Why one hook did not succeed, and what it printed while failing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookFailure {
    /// The hook's declared name.
    pub name: String,
    /// Its one-based position in the declared order.
    pub position: usize,
    /// How it failed, separated from an ordinary non-zero exit.
    pub reason: HookFailureReason,
    /// A bounded rendering of the reason and the hook's own output.
    pub output: String,
}

/// The failure modes a hook is distinguished by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookFailureReason {
    /// The hook ran and exited non-zero, or was killed by a signal.
    Exited { code: Option<i32> },
    /// The hook outlived its bound and its process group was killed.
    TimedOut,
    /// The command could not be started at all.
    NotStarted,
}

/// What provisioning did to one worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProvisioningOutcome {
    /// The repository declares no contract. Nothing was copied and nothing ran.
    NotDeclared,
    /// A hook failed and the caller chose to abort. The worktree and its
    /// branch are gone.
    Aborted(HookFailure),
    /// The contract was applied. Hooks may have been declined or may have
    /// failed; the report says so.
    Applied(ProvisioningReport),
}

/// Exactly what provisioning created, exported, and could not complete.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProvisioningReport {
    /// The contract file that was applied.
    pub contract: PathBuf,
    /// The paths, relative to the worktree, that this run actually created.
    /// A destination that already existed is not here, because provisioning
    /// did not create it.
    pub copied: Vec<PathBuf>,
    /// What the hooks exported, after the allowlist.
    pub environment: BTreeMap<String, String>,
    /// The names a hook tried to export that the allowlist does not admit, in
    /// the order they were seen. A worker that finds its environment missing
    /// something the repository declared is looking at this list.
    pub dropped_exports: Vec<String>,
    /// The allowlist entries the provisioner refused to carry, in the order the
    /// operator wrote them. An entry that admits every name is one, and an
    /// allowlist made only of those admits nothing at all — which reads exactly
    /// like a policy nobody configured unless this list says otherwise.
    pub rejected_export_patterns: Vec<String>,
    /// Whether the hooks were allowed to run. A contract without hooks is
    /// authorized trivially, having asked nothing.
    pub hooks_authorized: bool,
    /// Every hook failure the caller chose to continue past, in hook order.
    /// A non-empty list means the worker is starting in an environment that
    /// is not what the repository declared, and has to be told.
    pub failures: Vec<HookFailure>,
}

/// A repository's parsed and validated provisioning contract.
#[derive(Clone, Debug)]
pub struct ProvisioningContract {
    path: PathBuf,
    include: Gitignore,
    has_include_patterns: bool,
    hooks: Vec<HookDeclaration>,
}

#[derive(Clone, Debug)]
struct HookDeclaration {
    name: String,
    command: Vec<String>,
    working_directory: Option<PathBuf>,
    timeout: Option<Duration>,
}

#[derive(Debug)]
pub enum ProvisioningError {
    /// The contract is declared in both supported places, so neither can be
    /// trusted to be the one in force.
    AmbiguousContract { first: PathBuf, second: PathBuf },
    /// The contract could not be read, parsed, or validated.
    Contract { path: PathBuf, detail: String },
    /// The worktree named by the request is not on disk.
    Missing,
    /// The inclusion list selects more files than one provisioning may create.
    TooManyFiles { limit: usize },
    /// A filesystem step of provisioning failed.
    Storage {
        action: &'static str,
        detail: String,
    },
    /// Git could not report which files are eligible.
    Worktree(WorktreeError),
    /// The calling turn was cancelled.
    Cancelled,
}

impl std::fmt::Display for ProvisioningError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AmbiguousContract { first, second } => write!(
                formatter,
                "the worktree contract is declared twice: {} and {}",
                first.display(),
                second.display()
            ),
            Self::Contract { path, detail } => {
                write!(formatter, "{}: {detail}", path.display())
            }
            Self::Missing => formatter.write_str("no such worktree"),
            Self::TooManyFiles { limit } => write!(
                formatter,
                "the inclusion list selects more than {limit} files"
            ),
            Self::Storage { action, detail } => write!(formatter, "{action}: {detail}"),
            Self::Worktree(error) => write!(formatter, "{error}"),
            Self::Cancelled => formatter.write_str("cancelled"),
        }
    }
}

impl std::error::Error for ProvisioningError {}

impl From<WorktreeError> for ProvisioningError {
    fn from(error: WorktreeError) -> Self {
        match error {
            WorktreeError::Cancelled => Self::Cancelled,
            WorktreeError::Missing => Self::Missing,
            other => Self::Worktree(other),
        }
    }
}

impl ProvisioningContract {
    /// Reads the contract `repository` declares, if it declares one.
    ///
    /// A contract present in both supported places is an error rather than a
    /// silent preference, because the wrong one being in force is exactly the
    /// surprise this whole path exists to avoid.
    pub fn load(repository: &Path) -> Result<Option<Self>, ProvisioningError> {
        let in_directory = repository.join(CONTRACT_IN_AGENS_DIRECTORY);
        let at_root = repository.join(CONTRACT_AT_ROOT);

        let path = match (in_directory.is_file(), at_root.is_file()) {
            (true, true) => {
                return Err(ProvisioningError::AmbiguousContract {
                    first: in_directory,
                    second: at_root,
                });
            }
            (true, false) => in_directory,
            (false, true) => at_root,
            (false, false) => return Ok(None),
        };

        let document = read_contract(&path)?;
        Self::from_document(repository, path, document).map(Some)
    }

    /// Where the contract was read from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The hooks as they would run, with `default_timeout` filled in wherever
    /// the contract declared none.
    #[must_use]
    pub fn hook_plans(&self, default_timeout: Duration) -> Vec<HookPlan> {
        self.hooks
            .iter()
            .map(|hook| HookPlan {
                name: hook.name.clone(),
                command: hook.command.clone(),
                working_directory: hook.working_directory.clone(),
                timeout: hook
                    .timeout
                    .unwrap_or(default_timeout)
                    .min(MAX_HOOK_TIMEOUT),
            })
            .collect()
    }

    fn from_document(
        repository: &Path,
        path: PathBuf,
        document: ContractDocument,
    ) -> Result<Self, ProvisioningError> {
        let reject = |detail: String| ProvisioningError::Contract {
            path: path.clone(),
            detail,
        };

        let mut builder = GitignoreBuilder::new(repository);
        let mut has_include_patterns = false;
        for line in document.include.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            builder
                .add_line(None, trimmed)
                .map_err(|error| reject(format!("invalid include pattern: {error}")))?;
            has_include_patterns = true;
        }
        let include = builder
            .build()
            .map_err(|error| reject(format!("invalid inclusion list: {error}")))?;

        if document.hooks.len() > MAX_HOOKS {
            return Err(reject(format!("more than {MAX_HOOKS} hooks are declared")));
        }

        let mut hooks: Vec<HookDeclaration> = Vec::with_capacity(document.hooks.len());
        for hook in document.hooks {
            let declaration = HookDeclaration::validate(hook).map_err(reject)?;
            if hooks.iter().any(|other| other.name == declaration.name) {
                return Err(reject(format!(
                    "hook '{}' is declared more than once",
                    declaration.name
                )));
            }

            hooks.push(declaration);
        }

        Ok(Self {
            path,
            include,
            has_include_patterns,
            hooks,
        })
    }
}

impl HookDeclaration {
    fn validate(document: HookDocument) -> Result<Self, String> {
        let name = document.name.trim().to_owned();
        if name.is_empty() || name.chars().count() > MAX_HOOK_NAME_CHARS {
            return Err(format!(
                "a hook name must be non-empty and at most {MAX_HOOK_NAME_CHARS} characters"
            ));
        }

        if document
            .command
            .first()
            .is_none_or(|program| program.trim().is_empty())
        {
            return Err(format!("hook '{name}' declares no command"));
        }

        let timeout = match document.timeout_seconds {
            None => None,
            Some(0) => return Err(format!("hook '{name}' declares a zero timeout")),
            Some(seconds) => Some(Duration::from_secs(seconds).min(MAX_HOOK_TIMEOUT)),
        };

        let working_directory = match document.working_directory {
            None => None,
            Some(relative) => Some(relative_directory(&name, &relative)?),
        };

        Ok(Self {
            name,
            command: document.command,
            working_directory,
            timeout,
        })
    }
}

/// A hook's working directory has to stay inside the worktree: it is the one
/// place provisioning knows is its own.
fn relative_directory(hook: &str, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    let contained = path
        .components()
        .all(|component| matches!(component, Component::Normal(_)));

    if contained && !relative.trim().is_empty() {
        Ok(path.to_path_buf())
    } else {
        Err(format!(
            "hook '{hook}' declares a working directory outside the worktree"
        ))
    }
}

fn read_contract(path: &Path) -> Result<ContractDocument, ProvisioningError> {
    let reject = |detail: String| ProvisioningError::Contract {
        path: path.to_path_buf(),
        detail,
    };

    let length = std::fs::metadata(path)
        .map_err(|error| reject(error.to_string()))?
        .len();
    if length > MAX_CONTRACT_BYTES {
        return Err(reject(format!(
            "the contract is larger than {MAX_CONTRACT_BYTES} bytes"
        )));
    }

    let text = std::fs::read_to_string(path).map_err(|error| reject(error.to_string()))?;

    toml::from_str(&text).map_err(|error| reject(error.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContractDocument {
    #[serde(default)]
    include: String,
    #[serde(default)]
    hooks: Vec<HookDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookDocument {
    name: String,
    command: Vec<String>,
    timeout_seconds: Option<u64>,
    working_directory: Option<String>,
}

impl WorktreeProvisioner {
    /// Creates a provisioner over the worktree service that owns the layout.
    #[must_use]
    pub fn new(worktrees: SessionWorktrees) -> Self {
        Self {
            worktrees,
            hook_timeout: DEFAULT_HOOK_TIMEOUT,
            export_allowlist: Vec::new(),
            rejected_export_patterns: Vec::new(),
            execution_context: None,
        }
    }

    /// Accepts exactly these exported names, and drops every other one.
    ///
    /// A trailing `*` makes an entry a prefix, so `CARGO_*` admits
    /// `CARGO_TARGET_DIR` without admitting `CARGOX`. A dropped export is
    /// recorded rather than refused: a contract that exports one name the
    /// operator has not granted is still a contract worth applying.
    ///
    /// An entry that would admit every name is rejected here rather than
    /// carried, and recorded on the report as
    /// [`ProvisioningReport::rejected_export_patterns`]: an allowlist that
    /// suddenly admits nothing is otherwise indistinguishable from one nobody
    /// wrote.
    ///
    /// Rejecting those entries is not what keeps `PATH` and the loader
    /// variables out — [`Self::admits_export`] refuses those by name, whatever
    /// the allowlist says.
    #[must_use]
    pub fn with_export_allowlist(mut self, names: Vec<String>) -> Self {
        let (rejected, allowed): (Vec<String>, Vec<String>) = names
            .into_iter()
            .partition(|name| admits_every_name(name));

        self.export_allowlist = allowed;
        self.rejected_export_patterns = rejected;
        self
    }

    /// Bounds every hook that declares no timeout of its own, never above the
    /// module's ceiling.
    #[must_use]
    pub fn with_hook_timeout(mut self, timeout: Duration) -> Self {
        self.hook_timeout = timeout.min(MAX_HOOK_TIMEOUT);
        self
    }

    /// Makes provisioning observe the calling turn's cancellation, so an
    /// abandoned turn does not leave a hook running.
    #[must_use]
    pub fn with_execution_context(mut self, context: ToolExecutionContext) -> Self {
        self.execution_context = Some(context);
        self
    }

    /// Applies the repository's contract to the worktree named by `request`.
    ///
    /// Copying happens first and is never gated: an inclusion list is declared
    /// data, not execution. Hooks run only after `decisions` authorizes them,
    /// and a failure is resolved by `decisions` rather than here.
    pub fn provision(
        &self,
        request: &ProvisioningRequest<'_>,
        decisions: &dyn ProvisioningDecisions,
    ) -> Result<ProvisioningOutcome, ProvisioningError> {
        let worktree = self
            .worktrees
            .path(request.repository_id, request.name)
            .map_err(ProvisioningError::from)?;
        if !worktree.is_dir() {
            return Err(ProvisioningError::Missing);
        }

        let Some(contract) = ProvisioningContract::load(request.repository)? else {
            return Ok(ProvisioningOutcome::NotDeclared);
        };

        let copied = if contract.has_include_patterns {
            self.copy_included(request.repository, &worktree, &contract.include)?
        } else {
            Vec::new()
        };

        let plans = contract.hook_plans(self.hook_timeout);
        let mut report = ProvisioningReport {
            contract: contract.path.clone(),
            copied,
            environment: BTreeMap::new(),
            dropped_exports: Vec::new(),
            rejected_export_patterns: self.rejected_export_patterns.clone(),
            hooks_authorized: true,
            failures: Vec::new(),
        };

        if plans.is_empty() {
            return Ok(ProvisioningOutcome::Applied(report));
        }

        let authorization = decisions.authorize(&HookAuthorizationRequest {
            contract: &contract.path,
            worktree: &worktree,
            hooks: &plans,
            inherits_credentials: true,
        });
        if authorization == HookAuthorization::Deny {
            report.hooks_authorized = false;
            return Ok(ProvisioningOutcome::Applied(report));
        }

        self.run_hooks(request, &worktree, &plans, decisions, &mut report)
    }

    fn run_hooks(
        &self,
        request: &ProvisioningRequest<'_>,
        worktree: &Path,
        plans: &[HookPlan],
        decisions: &dyn ProvisioningDecisions,
        report: &mut ProvisioningReport,
    ) -> Result<ProvisioningOutcome, ProvisioningError> {
        let export_file = self
            .worktrees
            .repository_directory(request.repository_id)
            .map_err(ProvisioningError::from)?
            .join(format!("{}.hook-env", request.name));

        let outcome = self.drive_hooks(worktree, plans, decisions, report, &export_file);
        let _ = std::fs::remove_file(&export_file);

        match outcome? {
            None => Ok(ProvisioningOutcome::Applied(std::mem::take(report))),
            Some(failure) => {
                self.discard(request)?;
                Ok(ProvisioningOutcome::Aborted(failure))
            }
        }
    }

    /// Runs every hook in order, returning the failure the caller wants to
    /// abort on, if any.
    fn drive_hooks(
        &self,
        worktree: &Path,
        plans: &[HookPlan],
        decisions: &dyn ProvisioningDecisions,
        report: &mut ProvisioningReport,
        export_file: &Path,
    ) -> Result<Option<HookFailure>, ProvisioningError> {
        for (index, plan) in plans.iter().enumerate() {
            if self.is_cancelled() {
                return Err(ProvisioningError::Cancelled);
            }

            truncate_export_file(export_file)?;
            let failure =
                self.run_hook(worktree, plan, index + 1, &report.environment, export_file)?;

            let exported = read_exported_environment(export_file)?;
            let (admitted, dropped) = self.split_exports(exported);
            report.environment.extend(admitted);
            report.dropped_exports.extend(dropped);

            if let Some(failure) = failure {
                if decisions.on_hook_failure(&failure) == HookFailureResponse::Abort {
                    return Ok(Some(failure));
                }

                report.failures.push(failure);
            }
        }

        Ok(None)
    }

    /// Separates what a hook exported into what the allowlist admits and what
    /// it does not.
    fn split_exports(
        &self,
        exported: BTreeMap<String, String>,
    ) -> (BTreeMap<String, String>, Vec<String>) {
        let mut admitted = BTreeMap::new();
        let mut dropped = Vec::new();

        for (name, value) in exported {
            if self.admits_export(&name) {
                admitted.insert(name, value);
            } else {
                dropped.push(name);
            }
        }

        (admitted, dropped)
    }

    /// Whether the allowlist admits an exported name.
    ///
    /// [`NEVER_EXPORTED`] is checked first and answers on its own. An allowlist
    /// is an operator's statement about which values the following hooks may
    /// read, and those names are not values: they choose which programs run and
    /// which code is linked into them, so a hook that sets one has taken every
    /// hook after it whatever the operator meant to grant.
    fn admits_export(&self, name: &str) -> bool {
        if NEVER_EXPORTED.contains(&name) {
            return false;
        }

        self.export_allowlist
            .iter()
            .any(|allowed| match allowed.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == allowed,
            })
    }

    fn discard(&self, request: &ProvisioningRequest<'_>) -> Result<(), ProvisioningError> {
        self.worktrees
            .discard(
                request.repository,
                request.repository_id,
                request.name,
                request.branch,
            )
            .map_err(ProvisioningError::from)
    }

    fn run_hook(
        &self,
        worktree: &Path,
        plan: &HookPlan,
        position: usize,
        exported: &BTreeMap<String, String>,
        export_file: &Path,
    ) -> Result<Option<HookFailure>, ProvisioningError> {
        let Some((program, arguments)) = plan.command.split_first() else {
            return Ok(Some(HookFailure::not_started(plan, position, "no command")));
        };

        let directory = match &plan.working_directory {
            Some(relative) => worktree.join(relative),
            None => worktree.to_path_buf(),
        };

        let mut command = Command::new(program);
        command
            .args(arguments)
            .current_dir(directory)
            .envs(exported)
            .env(WORKTREE_VARIABLE, worktree)
            .env(EXPORT_VARIABLE, export_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let Ok(mut child) = command.spawn() else {
            return Ok(Some(HookFailure::not_started(
                plan,
                position,
                "the command could not be started",
            )));
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = terminate_process_group(&mut child);
            return Ok(Some(HookFailure::not_started(
                plan,
                position,
                "the command's output could not be captured",
            )));
        };

        let stdout_reader = read_capped(stdout);
        let stderr_reader = read_capped(stderr);
        let deadline = Instant::now() + self.hook_budget(plan);

        let status = loop {
            let cancelled = self.is_cancelled();
            if cancelled || Instant::now() >= deadline {
                let _ = terminate_process_group(&mut child);
                let output = wait_for_readers(stdout_reader, stderr_reader).ok();
                if cancelled {
                    return Err(ProvisioningError::Cancelled);
                }

                return Ok(Some(HookFailure::timed_out(plan, position, output)));
            }

            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = kill_process_group(child.id());
                    break status;
                }
                Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
                Err(_) => {
                    let _ = terminate_process_group(&mut child);
                    let _ = wait_for_readers(stdout_reader, stderr_reader);
                    return Ok(Some(HookFailure::not_started(
                        plan,
                        position,
                        "the command could not be waited on",
                    )));
                }
            }
        };

        let output = wait_for_readers(stdout_reader, stderr_reader).ok();
        if status.success() {
            return Ok(None);
        }

        Ok(Some(HookFailure::exited(
            plan,
            position,
            status.code(),
            output,
        )))
    }

    fn hook_budget(&self, plan: &HookPlan) -> Duration {
        match self.execution_context.as_ref() {
            Some(context) => plan.timeout.min(context.remaining().unwrap_or_default()),
            None => plan.timeout,
        }
    }

    fn is_cancelled(&self) -> bool {
        self.execution_context
            .as_ref()
            .is_some_and(ToolExecutionContext::is_cancelled)
    }

    /// Copies the eligible files the inclusion list selects, returning only
    /// the destinations this call actually created.
    fn copy_included(
        &self,
        repository: &Path,
        worktree: &Path,
        include: &Gitignore,
    ) -> Result<Vec<PathBuf>, ProvisioningError> {
        let eligible = self.eligible_paths(repository, include)?;
        let mut copied = Vec::new();

        for relative in eligible {
            if copy_one(
                &repository.join(&relative),
                &worktree.join(&relative),
                worktree,
            )? {
                copied.push(relative);
            }
        }

        Ok(copied)
    }

    /// Asks git which paths are ignored and untracked at once, keeping only
    /// the ones the inclusion list selects.
    ///
    /// The listing is filtered as it is read rather than collected first: a
    /// repository can hold tens of thousands of ignored files, and the point
    /// of the inclusion list is that almost none of them are wanted.
    fn eligible_paths(
        &self,
        repository: &Path,
        include: &Gitignore,
    ) -> Result<Vec<PathBuf>, ProvisioningError> {
        let mut command = Command::new("git");
        command
            .args(HARDENING_ARGUMENTS)
            .args([
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
            ])
            .current_dir(repository)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        harden_environment(&mut command);

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let Ok(mut child) = command.spawn() else {
            return Err(WorktreeError::GitUnavailable.into());
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = terminate_process_group(&mut child);
            return Err(WorktreeError::GitUnavailable.into());
        };

        let matcher = include.clone();
        let selector = thread::spawn(move || select_paths(stdout, &matcher));
        let stderr_reader = read_capped(stderr);

        let status = loop {
            if self.is_cancelled() {
                let _ = terminate_process_group(&mut child);
                let _ = selector.join();
                let _ = stderr_reader.join();
                return Err(ProvisioningError::Cancelled);
            }

            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = kill_process_group(child.id());
                    break status;
                }
                Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
                Err(_) => {
                    let _ = terminate_process_group(&mut child);
                    let _ = selector.join();
                    let _ = stderr_reader.join();
                    return Err(WorktreeError::GitUnavailable.into());
                }
            }
        };

        let selected = selector
            .join()
            .map_err(|_| ProvisioningError::from(WorktreeError::GitUnavailable))??;
        let stderr = stderr_reader
            .join()
            .map_err(|_| ProvisioningError::from(WorktreeError::GitUnavailable))?
            .map_err(|_| ProvisioningError::from(WorktreeError::GitUnavailable))?;

        if !status.success() {
            return Err(WorktreeError::Git {
                operation: "ls-files",
                detail: String::from_utf8_lossy(&stderr.bytes).trim().to_owned(),
            }
            .into());
        }

        Ok(selected)
    }
}

/// Reads a NUL-separated listing, keeping the paths the inclusion list selects.
fn select_paths(stdout: impl Read, include: &Gitignore) -> Result<Vec<PathBuf>, ProvisioningError> {
    let mut reader = BufReader::new(stdout);
    let mut entry = Vec::new();
    let mut scanned = 0usize;
    let mut selected = Vec::new();

    loop {
        entry.clear();
        let read = reader
            .read_until(0, &mut entry)
            .map_err(|_| ProvisioningError::from(WorktreeError::GitUnavailable))?;
        if read == 0 {
            return Ok(selected);
        }

        scanned += 1;
        if scanned > MAX_SCANNED_ENTRIES {
            return Err(ProvisioningError::TooManyFiles {
                limit: MAX_SCANNED_ENTRIES,
            });
        }

        if entry.last() == Some(&0) {
            entry.pop();
        }
        let Ok(path) = std::str::from_utf8(&entry) else {
            continue;
        };
        let relative = Path::new(path);
        if !include
            .matched_path_or_any_parents(relative, false)
            .is_ignore()
        {
            continue;
        }

        if selected.len() == MAX_PROVISIONED_FILES {
            return Err(ProvisioningError::TooManyFiles {
                limit: MAX_PROVISIONED_FILES,
            });
        }

        selected.push(relative.to_path_buf());
    }
}

/// Copies one eligible path, reporting whether the destination was created.
///
/// Nothing here follows a link. The source entry is reproduced as whatever it
/// is, a destination that already exists is left alone, and a parent that has
/// become a symlink stops the copy rather than letting it write outside the
/// worktree.
fn copy_one(source: &Path, destination: &Path, worktree: &Path) -> Result<bool, ProvisioningError> {
    if destination.symlink_metadata().is_ok() {
        return Ok(false);
    }

    let Ok(metadata) = source.symlink_metadata() else {
        return Ok(false);
    };

    if !create_parents(destination, worktree)? {
        return Ok(false);
    }

    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(source).map_err(|error| ProvisioningError::Storage {
            action: "read a provisioned link",
            detail: error.to_string(),
        })?;

        return symlink(&target, destination).map(|()| true);
    }

    if !metadata.file_type().is_file() {
        return Ok(false);
    }

    std::fs::copy(source, destination).map_err(|error| ProvisioningError::Storage {
        action: "copy a provisioned file",
        detail: error.to_string(),
    })?;
    std::fs::set_permissions(destination, metadata.permissions()).map_err(|error| {
        ProvisioningError::Storage {
            action: "preserve a provisioned file's mode",
            detail: error.to_string(),
        }
    })?;

    Ok(true)
}

/// Creates the destination's parent directories, refusing to write through one
/// that is a symlink or is not a directory at all.
fn create_parents(destination: &Path, worktree: &Path) -> Result<bool, ProvisioningError> {
    let Some(parent) = destination.parent() else {
        return Ok(false);
    };
    let Ok(relative) = parent.strip_prefix(worktree) else {
        return Ok(false);
    };

    let mut current = worktree.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Ok(false);
        };

        current.push(name);
        match current.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return Ok(false),
            Err(_) => {
                std::fs::create_dir(&current).map_err(|error| ProvisioningError::Storage {
                    action: "create a provisioned directory",
                    detail: error.to_string(),
                })?;
            }
        }
    }

    Ok(true)
}

#[cfg(unix)]
fn symlink(target: &Path, destination: &Path) -> Result<(), ProvisioningError> {
    std::os::unix::fs::symlink(target, destination).map_err(|error| ProvisioningError::Storage {
        action: "recreate a provisioned link",
        detail: error.to_string(),
    })
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _destination: &Path) -> Result<(), ProvisioningError> {
    Err(ProvisioningError::Storage {
        action: "recreate a provisioned link",
        detail: "links are not provisioned on this platform".to_owned(),
    })
}

/// Empties the export file before a hook runs, so what is read afterwards is
/// that hook's own contribution.
fn truncate_export_file(path: &Path) -> Result<(), ProvisioningError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    options
        .open(path)
        .and_then(|mut file| file.flush())
        .map_err(|error| ProvisioningError::Storage {
            action: "prepare the hook environment file",
            detail: error.to_string(),
        })
}

/// Reads the `KEY=value` lines a hook exported.
///
/// The format is deliberately not shell: nothing is expanded, quoted, or
/// continued across lines, because the file is written by a script that runs
/// with the daemon's credentials and is read back into a session's
/// environment.
fn read_exported_environment(path: &Path) -> Result<BTreeMap<String, String>, ProvisioningError> {
    let reject = |detail: String| ProvisioningError::Storage {
        action: "read the hook environment file",
        detail,
    };

    let length = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(_) => return Ok(BTreeMap::new()),
    };
    if length > MAX_EXPORTED_ENVIRONMENT_BYTES {
        return Err(reject(format!(
            "a hook exported more than {MAX_EXPORTED_ENVIRONMENT_BYTES} bytes"
        )));
    }

    let text = std::fs::read_to_string(path).map_err(|error| reject(error.to_string()))?;
    let mut exported = BTreeMap::new();

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }

        let Some((name, value)) = line.split_once('=') else {
            return Err(reject(
                "a hook exported a line that is not KEY=value".into(),
            ));
        };
        if !is_environment_name(name) {
            return Err(reject(format!("a hook exported an invalid name: {name}")));
        }
        if exported.len() == MAX_EXPORTED_VARIABLES && !exported.contains_key(name) {
            return Err(reject(format!(
                "a hook exported more than {MAX_EXPORTED_VARIABLES} variables"
            )));
        }

        exported.insert(name.to_owned(), value.to_owned());
    }

    Ok(exported)
}

/// Whether an allowlist entry selects every exported name, which is what a
/// bare `*` and an empty entry both do once the `*` is stripped off.
fn admits_every_name(entry: &str) -> bool {
    entry.strip_suffix('*').unwrap_or(entry).is_empty()
}

/// The names no allowlist admits, however it is written.
///
/// `PATH` decides which program each following hook runs, and the loader
/// variables decide which code is linked into it. An entry naming one of these
/// outright, or a prefix reaching one, would hand every hook after the first to
/// whoever wrote that hook.
const NEVER_EXPORTED: [&str; 3] = ["PATH", "LD_PRELOAD", "LD_LIBRARY_PATH"];

fn is_environment_name(name: &str) -> bool {
    let mut characters = name.chars();

    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

impl HookFailure {
    fn not_started(plan: &HookPlan, position: usize, detail: &str) -> Self {
        Self {
            name: plan.name.clone(),
            position,
            reason: HookFailureReason::NotStarted,
            output: format!("hook '{}' did not run: {detail}\n", plan.name),
        }
    }

    fn timed_out(plan: &HookPlan, position: usize, output: Option<crate::CappedOutput>) -> Self {
        let headline = format!(
            "hook '{}' timed out after {} seconds\n",
            plan.name,
            plan.timeout.as_secs()
        );

        Self {
            name: plan.name.clone(),
            position,
            reason: HookFailureReason::TimedOut,
            output: render_failure(&headline, output.as_ref()),
        }
    }

    fn exited(
        plan: &HookPlan,
        position: usize,
        code: Option<i32>,
        output: Option<crate::CappedOutput>,
    ) -> Self {
        let headline = match code {
            Some(code) => format!("hook '{}' exited with status {code}\n", plan.name),
            None => format!("hook '{}' was killed by a signal\n", plan.name),
        };

        Self {
            name: plan.name.clone(),
            position,
            reason: HookFailureReason::Exited { code },
            output: render_failure(&headline, output.as_ref()),
        }
    }
}

fn render_failure(headline: &str, output: Option<&crate::CappedOutput>) -> String {
    let mut rendered = String::from(headline);
    if let Some(output) = output {
        rendered.push_str(&String::from_utf8_lossy(&output.stdout));
        rendered.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    if rendered.len() > MAX_RECORDED_HOOK_OUTPUT {
        let end = rendered.floor_char_boundary(MAX_RECORDED_HOOK_OUTPUT);
        rendered.truncate(end);
        rendered.push_str("\n[hook output truncated]\n");
    }

    rendered
}

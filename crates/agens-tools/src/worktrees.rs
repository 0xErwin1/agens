//! Git worktrees owned by one session.
//!
//! Every invocation here reaches git the way [`crate::git_read`] does: a fixed
//! argv, an environment stripped of the variables that would redirect the
//! command at another repository, a bounded wait, and a process group that is
//! killed rather than left behind. The difference is that this path writes, so
//! it also has to stop the execution git reaches through a checkout: hooks and
//! the programs a repository can name in its own configuration.

use std::{
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::{
    CappedOutput, PROCESS_POLL_INTERVAL, ToolExecutionContext, git_read::harden_environment,
    kill_process_group, read_capped, terminate_process_group, wait_for_readers,
};

/// How long one git invocation may run when the caller carries no deadline of
/// its own. A checkout of a large repository is slow, a hung one is not
/// distinguishable from it except by waiting, and the session is blocked
/// either way.
const DEFAULT_WORKTREE_GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Options that hold each invocation to what it is supposed to do.
///
/// `core.hooksPath` is emptied because `worktree add` checks the new tree out
/// and a checkout runs `post-checkout` from the repository's own hook
/// directory. `core.fsmonitor` and `core.attributesFile` are emptied for the
/// same reason: both name a program or a file the repository chooses, and
/// neither is needed to create or inspect a worktree. `--no-replace-objects`
/// keeps the earlier invocation's guarantee that a replacement ref cannot
/// substitute the commit the worktree is created from. Content filters
/// configured inside the repository still run, so the tool's own description
/// says the checkout executes what the repository configures.
pub(crate) const HARDENING_ARGUMENTS: [&str; 9] = [
    "--no-pager",
    "--no-replace-objects",
    "--no-optional-locks",
    "-c",
    "core.fsmonitor=",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.attributesFile=/dev/null",
];

/// Git worktrees owned by daemon sessions under one Agens data directory.
#[derive(Clone, Debug)]
pub struct SessionWorktrees {
    data_directory: PathBuf,
    timeout: Duration,
    execution_context: Option<ToolExecutionContext>,
}

impl SessionWorktrees {
    /// Creates a worktree service rooted at `data_directory`.
    pub fn new(data_directory: impl AsRef<Path>) -> Self {
        Self {
            data_directory: data_directory.as_ref().to_path_buf(),
            timeout: DEFAULT_WORKTREE_GIT_TIMEOUT,
            execution_context: None,
        }
    }

    /// Bounds every later invocation by `timeout`, never raising it above the
    /// service's own bound.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = self.timeout.min(timeout);
        self
    }

    /// Makes every later invocation observe the calling tool's cancellation
    /// and deadline, so a cancelled turn does not leave git running.
    #[must_use]
    pub fn with_execution_context(mut self, context: ToolExecutionContext) -> Self {
        self.execution_context = Some(context);
        self
    }

    /// Creates `branch` from `start_point` at `worktrees/<repository_id>/<name>`.
    pub fn create(
        &self,
        repository: &Path,
        repository_id: &str,
        name: &str,
        branch: &str,
        start_point: &str,
    ) -> Result<PathBuf, WorktreeError> {
        validate_component(repository_id, "repository id")?;
        validate_component(name, "worktree name")?;
        validate_git_argument(branch, "branch")?;
        validate_git_argument(start_point, "start point")?;

        let repository_directory = self.repository_directory_unchecked(repository_id);
        std::fs::create_dir_all(&repository_directory).map_err(|error| WorktreeError::Storage {
            action: "create worktree directory",
            detail: error.to_string(),
        })?;

        let path = repository_directory.join(name);
        self.run_checked(
            repository,
            "worktree add",
            &[
                "worktree".into(),
                "add".into(),
                "-b".into(),
                branch.into(),
                path.clone().into_os_string(),
                start_point.into(),
            ],
        )?;

        Ok(path)
    }

    /// Re-derives ancestry and working-tree dirtiness from Git.
    pub fn status(
        &self,
        repository_id: &str,
        name: &str,
        merged_into: &str,
    ) -> Result<WorktreeStatus, WorktreeError> {
        validate_component(repository_id, "repository id")?;
        validate_component(name, "worktree name")?;
        validate_git_argument(merged_into, "merge target")?;

        let path = self.worktree_path(repository_id, name);
        self.ensure_present(&path)?;
        let merged = self.is_merged(&path, merged_into)?;
        let dirty = !self
            .run_checked(
                &path,
                "status",
                &[
                    "status".into(),
                    "--porcelain=v1".into(),
                    "--untracked-files=all".into(),
                ],
            )?
            .is_empty();

        Ok(WorktreeStatus { merged, dirty })
    }

    /// Force-removes a session worktree together with the branch it was
    /// created on.
    ///
    /// This is the exit for a worktree whose creation never completed: nothing
    /// in it is the user's yet, so the untracked files provisioning may have
    /// copied are not a reason to refuse. Use [`Self::remove`] for a worktree
    /// that has been handed to a session.
    pub fn discard(
        &self,
        repository: &Path,
        repository_id: &str,
        name: &str,
        branch: &str,
    ) -> Result<(), WorktreeError> {
        validate_component(repository_id, "repository id")?;
        validate_component(name, "worktree name")?;
        validate_git_argument(branch, "branch")?;

        let path = self.worktree_path(repository_id, name);
        self.ensure_present(&path)?;
        self.run_checked(
            repository,
            "worktree remove",
            &[
                "worktree".into(),
                "remove".into(),
                "--force".into(),
                path.into_os_string(),
            ],
        )?;
        self.run_checked(
            repository,
            "branch delete",
            &["branch".into(), "-D".into(), branch.into()],
        )?;

        Ok(())
    }

    /// Removes a clean session worktree while leaving its branch intact.
    pub fn remove(
        &self,
        repository: &Path,
        repository_id: &str,
        name: &str,
    ) -> Result<(), WorktreeError> {
        validate_component(repository_id, "repository id")?;
        validate_component(name, "worktree name")?;

        let path = self.worktree_path(repository_id, name);
        self.ensure_present(&path)?;
        self.run_checked(
            repository,
            "worktree remove",
            &["worktree".into(), "remove".into(), path.into_os_string()],
        )?;

        Ok(())
    }

    /// The names of the worktrees this repository already has on disk, so a
    /// caller can hold a session to a budget it can count.
    ///
    /// A missing directory is not an error: a session that has created
    /// nothing has no worktrees.
    pub fn names(&self, repository_id: &str) -> Result<Vec<String>, WorktreeError> {
        validate_component(repository_id, "repository id")?;

        let directory = self.repository_directory_unchecked(repository_id);
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(WorktreeError::Storage {
                    action: "list worktrees",
                    detail: error.to_string(),
                });
            }
        };

        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();

        Ok(names)
    }

    /// The commit `repository` currently has checked out, as the target a
    /// reclaim sweep measures a worktree's branch against.
    pub fn head_revision(&self, repository: &Path) -> Result<String, WorktreeError> {
        let output = self.run_checked(
            repository,
            "rev-parse",
            &["rev-parse".into(), "HEAD".into()],
        )?;

        Ok(String::from_utf8_lossy(&output).trim().to_owned())
    }

    /// The two values a repository's identity is derived from: the git common
    /// directory every one of its worktrees shares, and the URL of its
    /// `origin` when it has one.
    ///
    /// Both are read here rather than by whoever derives the identity, so a
    /// caller never reaches git outside this module's hardened invocation. The
    /// derivation itself belongs to the control plane, which is what the
    /// identity means something to.
    pub fn repository_identity(
        &self,
        repository: &Path,
    ) -> Result<RepositoryIdentity, WorktreeError> {
        let common_directory = self.run_checked(
            repository,
            "rev-parse",
            &[
                "rev-parse".into(),
                "--path-format=absolute".into(),
                "--git-common-dir".into(),
            ],
        )?;

        // A repository with no `origin` is identified by its common directory
        // alone. Two clones of the same upstream with no remote configured are
        // then distinct repositories, which is the correct reading: without an
        // origin there is no evidence that they are the same one.
        let origin = self.run_git(
            repository,
            &["remote".into(), "get-url".into(), "origin".into()],
        )?;
        let remote_url = origin
            .success
            .then(|| {
                String::from_utf8_lossy(&origin.output.stdout)
                    .trim()
                    .to_owned()
            })
            .filter(|url| !url.is_empty());

        Ok(RepositoryIdentity {
            common_directory: PathBuf::from(
                String::from_utf8_lossy(&common_directory).trim().to_owned(),
            ),
            remote_url,
        })
    }

    /// Where one named worktree lives, once its components are known to be
    /// safe.
    pub fn path(&self, repository_id: &str, name: &str) -> Result<PathBuf, WorktreeError> {
        validate_component(repository_id, "repository id")?;
        validate_component(name, "worktree name")?;

        Ok(self.worktree_path(repository_id, name))
    }

    /// Re-derives everything a coordinator gate compares against, in one pass
    /// over the worktree git currently has on disk.
    ///
    /// The gate never reads a stored flag, so every field here is derived at
    /// the moment of the call: the branch the worktree is on, its merge base
    /// against the target as the target stands now, the tree an approval is
    /// bound to, whether the branch already landed, whether anything is
    /// uncommitted, and the paths the branch touches.
    ///
    /// `worktree` is an absolute path because the control plane records one per
    /// run and that record is where a run's work lives. It is still held to
    /// this service's own directory: a path outside it is refused rather than
    /// derived from.
    pub fn derive(
        &self,
        worktree: &Path,
        merged_into: &str,
    ) -> Result<GateDerivation, WorktreeError> {
        validate_git_argument(merged_into, "merge target")?;
        let path = self.resolve_session_worktree(worktree)?;

        let branch = self.optional_line(
            &path,
            "symbolic-ref",
            &[
                "symbolic-ref".into(),
                "--quiet".into(),
                "--short".into(),
                "HEAD".into(),
            ],
        )?;
        let merge_base = self.optional_line(
            &path,
            "merge-base",
            &["merge-base".into(), "HEAD".into(), merged_into.into()],
        )?;

        let head_tree = String::from_utf8_lossy(&self.run_checked(
            &path,
            "rev-parse",
            &["rev-parse".into(), "HEAD^{tree}".into()],
        )?)
        .trim()
        .to_owned();

        let merged = self.is_merged(&path, merged_into)?;
        let dirty = !self
            .run_checked(
                &path,
                "status",
                &[
                    "status".into(),
                    "--porcelain=v1".into(),
                    "--untracked-files=all".into(),
                ],
            )?
            .is_empty();

        let changed_paths = match merge_base.as_deref() {
            Some(base) => self.changed_paths(&path, base)?,
            None => Vec::new(),
        };

        Ok(GateDerivation {
            branch,
            merge_base,
            head_tree,
            merged,
            dirty,
            changed_paths,
        })
    }

    /// Integrates `branch` into whatever `repository` has checked out.
    ///
    /// A merge that does not apply cleanly is aborted before returning, so a
    /// refused integration never leaves the checkout holding conflict markers
    /// for a decision the coordinator is not allowed to make.
    pub fn merge(&self, repository: &Path, branch: &str) -> Result<MergeOutcome, WorktreeError> {
        validate_git_argument(branch, "branch")?;

        let outcome = self.run_git(
            repository,
            &[
                "merge".into(),
                "--no-ff".into(),
                "--no-edit".into(),
                branch.into(),
            ],
        )?;

        if !outcome.success {
            let detail = String::from_utf8_lossy(&outcome.output.stderr)
                .trim()
                .to_owned();
            self.run_git(repository, &["merge".into(), "--abort".into()])?;

            return Ok(MergeOutcome::Conflicted { detail });
        }

        Ok(MergeOutcome::Merged {
            commit: self.head_revision(repository)?,
        })
    }

    /// The paths a branch touches between `base` and its head, as the gate
    /// compares them against the frozen genesis paths.
    fn changed_paths(&self, worktree: &Path, base: &str) -> Result<Vec<String>, WorktreeError> {
        let output = self.run_checked(
            worktree,
            "diff",
            &[
                "diff".into(),
                "--no-ext-diff".into(),
                "--name-only".into(),
                "-z".into(),
                base.into(),
                "HEAD".into(),
            ],
        )?;

        let mut paths: Vec<String> = output
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| String::from_utf8_lossy(entry).into_owned())
            .collect();
        paths.sort();
        paths.dedup();

        Ok(paths)
    }

    /// The single line an invocation prints, or `None` when git reports there
    /// is nothing to print: a detached head has no branch name, and unrelated
    /// histories have no merge base.
    fn optional_line(
        &self,
        directory: &Path,
        operation: &'static str,
        arguments: &[OsString],
    ) -> Result<Option<String>, WorktreeError> {
        let outcome = self.run_git(directory, arguments)?;

        match outcome.code {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&outcome.output.stdout)
                    .trim()
                    .to_owned(),
            )),
            Some(1) => Ok(None),
            _ => Err(outcome.failure(operation)),
        }
    }

    /// Holds an absolute worktree path to the directory this service owns.
    ///
    /// Both sides are canonicalized before they are compared, so neither a
    /// symlink nor a `..` segment can present a path outside the data
    /// directory as one inside it.
    fn resolve_session_worktree(&self, worktree: &Path) -> Result<PathBuf, WorktreeError> {
        let root = self
            .data_directory
            .join("worktrees")
            .canonicalize()
            .map_err(|_| WorktreeError::Missing)?;
        let path = worktree
            .canonicalize()
            .map_err(|_| WorktreeError::Missing)?;

        if path == root || !path.starts_with(&root) {
            return Err(WorktreeError::InvalidInput {
                field: "worktree path",
                detail: "must be inside this data directory's worktrees",
            });
        }
        self.ensure_present(&path)?;

        Ok(path)
    }

    /// Where one repository's session worktrees live, once `repository_id` is
    /// known to be a single path component.
    pub fn repository_directory(&self, repository_id: &str) -> Result<PathBuf, WorktreeError> {
        validate_component(repository_id, "repository id")?;

        Ok(self.repository_directory_unchecked(repository_id))
    }

    fn repository_directory_unchecked(&self, repository_id: &str) -> PathBuf {
        self.data_directory.join("worktrees").join(repository_id)
    }

    fn worktree_path(&self, repository_id: &str, name: &str) -> PathBuf {
        self.repository_directory_unchecked(repository_id)
            .join(name)
    }

    /// Separates a worktree that is no longer there from a git that cannot be
    /// started, which the spawn failure alone reports identically.
    fn ensure_present(&self, path: &Path) -> Result<(), WorktreeError> {
        if path.is_dir() {
            Ok(())
        } else {
            Err(WorktreeError::Missing)
        }
    }

    fn is_merged(&self, worktree: &Path, target: &str) -> Result<bool, WorktreeError> {
        let outcome = self.run_git(
            worktree,
            &[
                "merge-base".into(),
                "--is-ancestor".into(),
                "HEAD".into(),
                target.into(),
            ],
        )?;

        match outcome.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(outcome.failure("merge-base")),
        }
    }

    fn run_checked(
        &self,
        directory: &Path,
        operation: &'static str,
        arguments: &[OsString],
    ) -> Result<Vec<u8>, WorktreeError> {
        let outcome = self.run_git(directory, arguments)?;
        if !outcome.success {
            return Err(outcome.failure(operation));
        }

        Ok(outcome.output.stdout)
    }

    fn run_git(
        &self,
        directory: &Path,
        arguments: &[OsString],
    ) -> Result<GitOutcome, WorktreeError> {
        let mut command = Command::new("git");
        command
            .args(HARDENING_ARGUMENTS.map(OsStr::new))
            .args(arguments)
            .current_dir(directory)
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
            return Err(WorktreeError::GitUnavailable);
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = terminate_process_group(&mut child);
            return Err(WorktreeError::GitUnavailable);
        };
        let stdout_reader = read_capped(stdout);
        let stderr_reader = read_capped(stderr);
        let deadline = Instant::now() + self.remaining_budget();

        let status = loop {
            let cancelled = self
                .execution_context
                .as_ref()
                .is_some_and(ToolExecutionContext::is_cancelled);

            if cancelled || Instant::now() >= deadline {
                let _ = terminate_process_group(&mut child);
                let _ = wait_for_readers(stdout_reader, stderr_reader);
                return Err(if cancelled {
                    WorktreeError::Cancelled
                } else {
                    WorktreeError::TimedOut
                });
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
                    return Err(WorktreeError::GitUnavailable);
                }
            }
        };

        let output = wait_for_readers(stdout_reader, stderr_reader)
            .map_err(|_| WorktreeError::GitUnavailable)?;

        Ok(GitOutcome {
            success: status.success(),
            code: status.code(),
            output,
        })
    }

    /// The invocation's own bound, lowered to whatever the calling tool has
    /// left. An already-expired caller still gets one attempt at a
    /// zero-length budget, which fails as a timeout rather than as a silent
    /// success.
    fn remaining_budget(&self) -> Duration {
        match self.execution_context.as_ref() {
            Some(context) => self.timeout.min(context.remaining().unwrap_or_default()),
            None => self.timeout,
        }
    }
}

struct GitOutcome {
    success: bool,
    code: Option<i32>,
    output: CappedOutput,
}

impl GitOutcome {
    fn failure(&self, operation: &'static str) -> WorktreeError {
        WorktreeError::Git {
            operation,
            detail: String::from_utf8_lossy(&self.output.stderr)
                .trim()
                .to_owned(),
        }
    }
}

/// What a repository's identity is derived from.
///
/// Every worktree of a repository shares both, and only `--show-toplevel`
/// separates them — which is exactly why these two are the identity for
/// grouping and the path is the identity for confinement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryIdentity {
    pub common_directory: PathBuf,
    pub remote_url: Option<String>,
}

/// Everything a coordinator gate re-derives from git in one pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateDerivation {
    /// `None` on a detached head, which has no branch to merge or reclaim.
    pub branch: Option<String>,
    /// `None` when the worktree and the target share no history.
    pub merge_base: Option<String>,
    /// `HEAD^{tree}`: the identity an approval's receipt is bound to.
    pub head_tree: String,
    /// Whether `HEAD` is already an ancestor of the target.
    pub merged: bool,
    /// Whether tracked, staged, or untracked changes are present.
    pub dirty: bool,
    /// The paths changed between the merge base and `HEAD`, sorted and unique.
    pub changed_paths: Vec<String>,
}

/// How an integration ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeOutcome {
    Merged {
        commit: String,
    },
    /// The merge did not apply and was aborted, leaving the checkout as it was.
    Conflicted {
        detail: String,
    },
}

/// Git-derived facts about one session worktree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorktreeStatus {
    /// Whether the worktree's `HEAD` is an ancestor of the requested target.
    pub merged: bool,
    /// Whether tracked, staged, or untracked changes are present.
    pub dirty: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorktreeError {
    /// A path component or revision-like argument is unsafe or empty.
    InvalidInput {
        field: &'static str,
        detail: &'static str,
    },
    /// The worktree's data-directory layout could not be prepared.
    Storage {
        action: &'static str,
        detail: String,
    },
    /// The worktree the caller named is not on disk.
    Missing,
    /// Git could not be started.
    GitUnavailable,
    /// Git ran longer than the invocation's budget and was killed.
    TimedOut,
    /// The calling turn was cancelled while git was running.
    Cancelled,
    /// Git rejected an operation.
    Git {
        operation: &'static str,
        detail: String,
    },
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput { field, detail } => write!(formatter, "invalid {field}: {detail}"),
            Self::Storage { action, detail } => write!(formatter, "{action}: {detail}"),
            Self::Missing => formatter.write_str("no such worktree"),
            Self::GitUnavailable => formatter.write_str("git is unavailable"),
            Self::TimedOut => formatter.write_str("git timed out"),
            Self::Cancelled => formatter.write_str("cancelled"),
            Self::Git { operation, detail } => {
                write!(formatter, "git {operation} failed: {detail}")
            }
        }
    }
}

impl std::error::Error for WorktreeError {}

fn validate_component(value: &str, field: &'static str) -> Result<(), WorktreeError> {
    let mut components = Path::new(value).components();
    let valid =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();

    if valid {
        Ok(())
    } else {
        Err(WorktreeError::InvalidInput {
            field,
            detail: "must be one non-empty path component",
        })
    }
}

fn validate_git_argument(value: &str, field: &'static str) -> Result<(), WorktreeError> {
    if !value.is_empty() && !value.starts_with('-') {
        Ok(())
    } else {
        Err(WorktreeError::InvalidInput {
            field,
            detail: "must be non-empty and must not start with '-'",
        })
    }
}

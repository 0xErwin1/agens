use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
    process::{Command, Output},
};

/// Git worktrees owned by daemon sessions under one Agens data directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionWorktrees {
    data_directory: PathBuf,
}

impl SessionWorktrees {
    /// Creates a worktree service rooted at `data_directory`.
    pub fn new(data_directory: impl AsRef<Path>) -> Self {
        Self {
            data_directory: data_directory.as_ref().to_path_buf(),
        }
    }

    /// Creates `branch` from `start_point` at `worktrees/<repo_id>/<name>`.
    pub fn create(
        &self,
        repository: &Path,
        repo_id: &str,
        name: &str,
        branch: &str,
        start_point: &str,
    ) -> Result<PathBuf, WorktreeError> {
        validate_component(repo_id, "repository id")?;
        validate_component(name, "worktree name")?;
        validate_git_argument(branch, "branch")?;
        validate_git_argument(start_point, "start point")?;

        let repository_directory = self.repository_directory(repo_id);
        std::fs::create_dir_all(&repository_directory).map_err(|error| WorktreeError::Storage {
            action: "create worktree directory",
            detail: error.to_string(),
        })?;

        let path = repository_directory.join(name);
        run_checked(
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
        repo_id: &str,
        name: &str,
        merged_into: &str,
    ) -> Result<WorktreeStatus, WorktreeError> {
        validate_component(repo_id, "repository id")?;
        validate_component(name, "worktree name")?;
        validate_git_argument(merged_into, "merge target")?;

        let path = self.worktree_path(repo_id, name);
        let merged = is_merged(&path, merged_into)?;
        let dirty = !run_checked(
            &path,
            "status",
            &[
                "--no-optional-locks".into(),
                "-c".into(),
                "core.fsmonitor=".into(),
                "status".into(),
                "--porcelain=v1".into(),
                "--untracked-files=all".into(),
            ],
        )?
        .is_empty();

        Ok(WorktreeStatus { merged, dirty })
    }

    /// Removes a clean session worktree while leaving its branch intact.
    pub fn remove(
        &self,
        repository: &Path,
        repo_id: &str,
        name: &str,
    ) -> Result<(), WorktreeError> {
        validate_component(repo_id, "repository id")?;
        validate_component(name, "worktree name")?;

        let path = self.worktree_path(repo_id, name);
        run_checked(
            repository,
            "worktree remove",
            &["worktree".into(), "remove".into(), path.into_os_string()],
        )?;

        Ok(())
    }

    fn repository_directory(&self, repo_id: &str) -> PathBuf {
        self.data_directory.join("worktrees").join(repo_id)
    }

    fn worktree_path(&self, repo_id: &str, name: &str) -> PathBuf {
        self.repository_directory(repo_id).join(name)
    }
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
    /// Git could not be started.
    GitUnavailable,
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
            Self::GitUnavailable => formatter.write_str("git is unavailable"),
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

fn is_merged(worktree: &Path, target: &str) -> Result<bool, WorktreeError> {
    let output = run_git(
        worktree,
        &[
            "merge-base".into(),
            "--is-ancestor".into(),
            "HEAD".into(),
            target.into(),
        ],
    )?;

    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure("merge-base", output)),
    }
}

fn run_checked(
    directory: &Path,
    operation: &'static str,
    arguments: &[OsString],
) -> Result<Vec<u8>, WorktreeError> {
    let output = run_git(directory, arguments)?;
    if !output.status.success() {
        return Err(git_failure(operation, output));
    }

    Ok(output.stdout)
}

fn run_git(directory: &Path, arguments: &[OsString]) -> Result<Output, WorktreeError> {
    Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|_| WorktreeError::GitUnavailable)
}

fn git_failure(operation: &'static str, output: Output) -> WorktreeError {
    WorktreeError::Git {
        operation,
        detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

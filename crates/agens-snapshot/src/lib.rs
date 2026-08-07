//! Point-in-time snapshots of a project's working tree.
//!
//! A snapshot is a git tree object written into a repository that lives beside
//! the session data rather than inside the project. The project's own history,
//! index, branches and stash are never touched: the only thing shared with it
//! is the object database, borrowed read-only so hashing a large tree does not
//! start from scratch.
//!
//! Capturing costs one `add` plus one `write-tree`, which is why a snapshot can
//! be taken at every turn boundary. Restoring is per-path and never wholesale,
//! so a caller that only wants back the files one turn touched cannot
//! accidentally roll the rest of the tree with them.
//!
//! git reaches write and execution behaviour through configuration as well as
//! through subcommand names, so every invocation here fixes its own argv, runs
//! with no shell, and starts from a neutral configuration — see
//! [`harden_environment`] for which mechanism closes which path.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Files larger than this are left out of a snapshot, so one vendored binary or
/// build artifact cannot make every turn boundary expensive.
const MAX_SNAPSHOT_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// A git tree hash naming one captured state of the working tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// Rebuilds an id a caller stored earlier. The hash names an object in the
    /// snapshot repository, so an id from elsewhere simply resolves to nothing.
    pub fn from_hash(hash: String) -> Self {
        Self(hash)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotError {
    /// git is not installed, or could not be started.
    Unavailable,
    /// A git invocation failed; the message is git's own, already bounded.
    Command { operation: String, detail: String },
    /// The snapshot repository could not be prepared under the data directory.
    Storage(String),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("git is unavailable"),
            Self::Command { operation, detail } => {
                write!(formatter, "snapshot {operation} failed: {detail}")
            }
            Self::Storage(detail) => write!(formatter, "snapshot storage: {detail}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// What a restore actually did, so a caller can report it instead of guessing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RestoreReport {
    /// Paths whose earlier content was put back.
    pub restored: Vec<String>,
    /// Paths that did not exist in the snapshot and were therefore removed.
    pub removed: Vec<String>,
    /// Paths git refused to restore. Left exactly as they were.
    pub failed: Vec<String>,
}

/// The snapshot repository for one working tree.
pub struct WorkspaceSnapshots {
    git_dir: PathBuf,
    worktree: PathBuf,
}

impl WorkspaceSnapshots {
    /// Prepares the snapshot repository for `worktree`, or reports that this
    /// project cannot be snapshotted.
    ///
    /// Returns `Ok(None)` when `worktree` is not inside a git repository. That
    /// is a fact about the project, not a failure: the caller's job is to say
    /// so, not to retry.
    pub fn open(data_directory: &Path, worktree: &Path) -> Result<Option<Self>, SnapshotError> {
        let worktree = worktree.to_path_buf();
        if !is_git_worktree(&worktree)? {
            return Ok(None);
        }

        let git_dir = data_directory
            .join("snapshots")
            .join(worktree_key(&worktree));
        std::fs::create_dir_all(&git_dir)
            .map_err(|error| SnapshotError::Storage(error.to_string()))?;

        let repository = Self { git_dir, worktree };
        repository.initialize()?;
        Ok(Some(repository))
    }

    /// Captures the working tree as it stands and returns its tree hash.
    pub fn capture(&self) -> Result<SnapshotId, SnapshotError> {
        self.stage()?;
        let output = self.git(&["write-tree"], None)?;
        let hash = output.trim().to_owned();
        if hash.is_empty() {
            return Err(SnapshotError::Command {
                operation: "write-tree".into(),
                detail: "git produced no tree hash".into(),
            });
        }
        Ok(SnapshotId(hash))
    }

    /// Repository-relative paths that differ between `snapshot` and the working
    /// tree as it stands now.
    pub fn changed_since(&self, snapshot: &SnapshotId) -> Result<Vec<String>, SnapshotError> {
        self.stage()?;
        let output = self.git(
            &[
                "diff",
                "--cached",
                "--no-ext-diff",
                "--name-only",
                "-z",
                snapshot.as_str(),
            ],
            None,
        )?;
        Ok(split_nul(&output))
    }

    /// Puts `paths` back to their content in `snapshot`.
    ///
    /// A path absent from the snapshot did not exist then, so restoring it
    /// means deleting it now. Every path is addressed as a literal top-level
    /// pathspec, so a file whose name looks like a pattern or a flag is still
    /// only ever itself.
    pub fn restore(
        &self,
        snapshot: &SnapshotId,
        paths: &[String],
    ) -> Result<RestoreReport, SnapshotError> {
        let mut report = RestoreReport::default();

        for path in paths {
            if self.path_in_snapshot(snapshot, path)? {
                match self.git(
                    &["checkout", snapshot.as_str(), "--", &literal_pathspec(path)],
                    None,
                ) {
                    Ok(_) => report.restored.push(path.clone()),
                    Err(_) => report.failed.push(path.clone()),
                }
                continue;
            }

            match std::fs::remove_file(self.worktree.join(path)) {
                Ok(()) => report.removed.push(path.clone()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    report.removed.push(path.clone());
                }
                Err(_) => report.failed.push(path.clone()),
            }
        }

        Ok(report)
    }

    fn path_in_snapshot(&self, snapshot: &SnapshotId, path: &str) -> Result<bool, SnapshotError> {
        let output = self.git(
            &["ls-tree", snapshot.as_str(), "--", &literal_pathspec(path)],
            None,
        )?;
        Ok(!output.trim().is_empty())
    }

    /// Creates the repository on first use and points it at the project's own
    /// object database, so the first capture reuses hashes git already computed
    /// instead of re-hashing the whole tree.
    fn initialize(&self) -> Result<(), SnapshotError> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }

        self.git(&["init", "--quiet"], None)?;
        for (key, value) in [
            ("core.autocrlf", "false"),
            ("core.longpaths", "true"),
            ("core.symlinks", "true"),
            ("core.fsmonitor", "false"),
            ("core.untrackedCache", "true"),
            ("feature.manyFiles", "true"),
        ] {
            self.git(&["config", key, value], None)?;
        }

        self.borrow_project_objects();
        self.seed_index();
        Ok(())
    }

    /// Copies the project's index so the snapshot starts knowing every tracked
    /// file.
    ///
    /// Without this the snapshot index is empty, and a capture of an unmodified
    /// tree would write the empty tree — an undo against which would delete the
    /// whole project rather than restore it.
    fn seed_index(&self) {
        let Ok(common) =
            self.project_git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        else {
            return;
        };
        let source = PathBuf::from(common.trim()).join("index");
        if source.is_file() {
            let _ = std::fs::copy(source, self.git_dir.join("index"));
        }
    }

    /// Best-effort: without the alternate the first capture is merely slower.
    fn borrow_project_objects(&self) {
        let Ok(common) =
            self.project_git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        else {
            return;
        };
        let objects = PathBuf::from(common.trim()).join("objects");
        if !objects.is_dir() {
            return;
        }

        let info = self.git_dir.join("objects").join("info");
        if std::fs::create_dir_all(&info).is_err() {
            return;
        }
        let _ = std::fs::write(info.join("alternates"), format!("{}\n", objects.display()));
    }

    /// Brings the snapshot index in line with the working tree.
    ///
    /// Only files the project itself would track are staged: `add --all` over
    /// the candidate set git reports, minus what the project's own ignore rules
    /// exclude and minus anything past the size cap.
    fn stage(&self) -> Result<(), SnapshotError> {
        let candidates = self.candidate_paths()?;
        if candidates.is_empty() {
            return Ok(());
        }

        let staged = candidates
            .into_iter()
            .filter(|path| self.within_size_cap(path))
            .collect::<Vec<_>>();
        if staged.is_empty() {
            return Ok(());
        }

        self.git(
            &[
                "add",
                "--all",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            Some(&encode_pathspecs(&staged)),
        )?;
        Ok(())
    }

    /// Everything modified or untracked in the project, as the project's own
    /// repository sees it — which is what applies its `.gitignore`.
    fn candidate_paths(&self) -> Result<Vec<String>, SnapshotError> {
        // A repository with no commits has no HEAD to diff against, and its
        // whole content is untracked anyway.
        let tracked = self
            .project_git(&["diff", "--name-only", "-z", "HEAD"])
            .unwrap_or_default();
        let untracked = self.project_git(&[
            "ls-files",
            "--full-name",
            "--others",
            "--exclude-standard",
            "-z",
        ])?;

        let mut paths = split_nul(&tracked);
        for path in split_nul(&untracked) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    fn within_size_cap(&self, path: &str) -> bool {
        std::fs::metadata(self.worktree.join(path))
            .map(|metadata| !metadata.is_file() || metadata.len() <= MAX_SNAPSHOT_FILE_BYTES)
            .unwrap_or(true)
    }

    /// Runs git against the snapshot repository over the project's working tree.
    fn git(&self, arguments: &[&str], stdin: Option<&[u8]>) -> Result<String, SnapshotError> {
        let mut full = vec![
            "--git-dir".to_owned(),
            self.git_dir.display().to_string(),
            "--work-tree".to_owned(),
            self.worktree.display().to_string(),
        ];
        full.extend(arguments.iter().map(|argument| (*argument).to_owned()));
        run_git(&self.worktree, &full, stdin)
    }

    /// Runs git against the project's own repository. Read-only by construction:
    /// only the two listing operations `candidate_paths` needs reach this.
    fn project_git(&self, arguments: &[&str]) -> Result<String, SnapshotError> {
        let owned = arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>();
        run_git(&self.worktree, &owned, None)
    }
}

fn is_git_worktree(worktree: &Path) -> Result<bool, SnapshotError> {
    match run_git(
        worktree,
        &["rev-parse".to_owned(), "--is-inside-work-tree".to_owned()],
        None,
    ) {
        Ok(output) => Ok(output.trim() == "true"),
        Err(SnapshotError::Unavailable) => Err(SnapshotError::Unavailable),
        Err(_) => Ok(false),
    }
}

/// A stable directory name for one working tree, so two projects never share a
/// snapshot repository and the same project always finds its own.
fn worktree_key(worktree: &Path) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(worktree.display().to_string().as_bytes());
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Pathspec magic that stops a file name from being read as a pattern, and
/// anchors it to the top of the working tree rather than the current directory.
fn literal_pathspec(path: &str) -> String {
    format!(":(top,literal){path}")
}

fn encode_pathspecs(paths: &[String]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for path in paths {
        encoded.extend_from_slice(literal_pathspec(path).as_bytes());
        encoded.push(0);
    }
    encoded
}

fn split_nul(output: &str) -> Vec<String> {
    output
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

const HARDENING_ARGUMENTS: [&str; 4] =
    ["--no-pager", "--no-optional-locks", "-c", "core.fsmonitor="];

fn run_git(
    directory: &Path,
    arguments: &[String],
    stdin: Option<&[u8]>,
) -> Result<String, SnapshotError> {
    let mut command = Command::new("git");
    command
        .args(HARDENING_ARGUMENTS)
        .args(arguments)
        .current_dir(directory)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    harden_environment(&mut command);

    let mut child = command.spawn().map_err(|_| SnapshotError::Unavailable)?;

    if let Some(bytes) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let _ = pipe.write_all(bytes);
    }

    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                return Err(SnapshotError::Command {
                    operation: operation_name(arguments),
                    detail: "git did not finish in time".into(),
                });
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => {
                return Err(SnapshotError::Command {
                    operation: operation_name(arguments),
                    detail: error.to_string(),
                });
            }
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|error| SnapshotError::Command {
            operation: operation_name(arguments),
            detail: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(SnapshotError::Command {
            operation: operation_name(arguments),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The subcommand, for an error message. The leading `--git-dir`/`--work-tree`
/// pair carries paths, which have no place in a message a person reads.
fn operation_name(arguments: &[String]) -> String {
    arguments
        .iter()
        .find(|argument| !argument.starts_with('-'))
        .map_or_else(|| "git".to_owned(), |argument| argument.clone())
        .replace(
            |character: char| !character.is_ascii_alphanumeric() && character != '-',
            "",
        )
}

fn harden_environment(command: &mut Command) {
    for variable in [
        "GIT_EXTERNAL_DIFF",
        "GIT_PAGER",
        "PAGER",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
    ] {
        command.env_remove(variable);
    }

    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "agens")
        .env("GIT_AUTHOR_EMAIL", "agens@localhost")
        .env("GIT_COMMITTER_NAME", "agens")
        .env("GIT_COMMITTER_EMAIL", "agens@localhost");
}

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
//! Every degraded path here has to fail rather than proceed. A snapshot that
//! does not describe the working tree is worse than no snapshot at all: undoing
//! against it deletes files instead of restoring them. So the index seed is a
//! precondition rather than an optimisation, a capture that would produce the
//! empty tree for a non-empty project is an error, and a path the snapshot
//! could not cover is recorded so a restore never mistakes it for a file the
//! turn created.
//!
//! git reaches write and execution behaviour through configuration as well as
//! through subcommand names, so every invocation here fixes its own argv, runs
//! with no shell, and starts from a neutral configuration — see
//! [`harden_environment`] for which mechanism closes which path.

use std::{
    collections::HashSet,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread::ScopedJoinHandle,
    time::{Duration, Instant},
};

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Files larger than this are left out of a snapshot, so one vendored binary or
/// build artifact cannot make every turn boundary expensive.
const MAX_SNAPSHOT_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// The hash git gives a tree with no entries.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// How much argv one git invocation may carry before the path list is split,
/// well under the smallest argument limit any supported platform imposes.
const MAX_ARGUMENT_BATCH_BYTES: usize = 96 * 1024;

/// Where the paths a capture could not represent are recorded, keyed by tree.
const UNCOVERED_DIRECTORY: &str = "agens-uncovered";

/// Which project a snapshot repository belongs to, so an abandoned one can be
/// recognised without guessing from its name.
const WORKTREE_MARKER: &str = "agens-worktree";

/// A git tree hash naming one captured state of the working tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// Rebuilds an id a caller stored earlier. The hash names an object in the
    /// snapshot repository, so an id from elsewhere simply resolves to nothing.
    ///
    /// The value is not trusted at construction: every use resolves it through
    /// a check that it really is an object hash, because it reaches git in
    /// argument position and is used as a file name.
    pub fn from_hash(hash: String) -> Self {
        Self(hash)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validated(&self) -> Result<&str, SnapshotError> {
        let hash = self.0.trim();
        let is_object_hash = !hash.is_empty()
            && hash.len() <= 64
            && hash.chars().all(|character| character.is_ascii_hexdigit());
        if is_object_hash {
            return Ok(hash);
        }

        Err(SnapshotError::Command {
            operation: "resolve".into(),
            detail: "a snapshot id must be a git object hash".into(),
        })
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
    /// Paths the snapshot never covered — too large, or ignored when it was
    /// taken. Absent from the tree without ever having been created by the
    /// turn, so they were left exactly as they are.
    pub uncovered: Vec<String>,
    /// Paths git refused to restore. Left exactly as they were.
    pub failed: Vec<String>,
}

/// The snapshot repository for one working tree.
///
/// Each instance owns its own index inside the shared repository, so two
/// sessions on the same project cannot stage over one another.
pub struct WorkspaceSnapshots {
    git_dir: PathBuf,
    worktree: PathBuf,
    index_file: PathBuf,
}

impl WorkspaceSnapshots {
    /// Prepares the snapshot repository for `worktree`, or reports that this
    /// project cannot be snapshotted.
    ///
    /// Returns `Ok(None)` when `worktree` is not inside a git repository. That
    /// is a fact about the project, not a failure: the caller's job is to say
    /// so, not to retry. A repository git cannot read is a failure, and says so
    /// as one.
    pub fn open(data_directory: &Path, worktree: &Path) -> Result<Option<Self>, SnapshotError> {
        let worktree = std::fs::canonicalize(worktree)
            .map_err(|error| SnapshotError::Storage(error.to_string()))?;
        if !is_git_worktree(&worktree)? {
            return Ok(None);
        }

        let root = data_directory.join("snapshots");
        let git_dir = root.join(worktree_key(&worktree));
        create_private_directory(&root)?;
        create_private_directory(&git_dir)?;

        let index_file = git_dir.join(format!("index-{}", unique_suffix()));
        let repository = Self {
            git_dir,
            worktree,
            index_file,
        };
        repository.initialize()?;
        Ok(Some(repository))
    }

    /// Captures the working tree as it stands and returns its tree hash.
    ///
    /// Fails rather than returning the empty tree for a project that has
    /// tracked files: an empty tree names a state in which every file is gone,
    /// and restoring to it would delete the project.
    pub fn capture(&self) -> Result<SnapshotId, SnapshotError> {
        let over_cap = self.stage()?;
        let hash = self
            .git("write-tree", &["write-tree"], None)?
            .trim()
            .to_owned();
        if hash.is_empty() {
            return Err(SnapshotError::Command {
                operation: "write-tree".into(),
                detail: "git produced no tree hash".into(),
            });
        }

        let snapshot = SnapshotId(hash);
        let hash = snapshot.validated()?;
        if hash == EMPTY_TREE && !self.project_is_empty()? {
            return Err(SnapshotError::Command {
                operation: "write-tree".into(),
                detail: "the snapshot index is empty while the project tracks files".into(),
            });
        }

        let mut uncovered = over_cap;
        extend_unique(&mut uncovered, self.ignored_entries()?);
        self.record_uncovered(hash, &uncovered)?;
        Ok(snapshot)
    }

    /// Repository-relative paths that differ between `snapshot` and the working
    /// tree as it stands now.
    pub fn changed_since(&self, snapshot: &SnapshotId) -> Result<Vec<String>, SnapshotError> {
        let hash = snapshot.validated()?;
        self.stage()?;
        let output = self.git(
            "diff",
            &[
                "diff",
                "--cached",
                "--no-ext-diff",
                "--name-only",
                "-z",
                hash,
                "--",
            ],
            None,
        )?;
        Ok(split_nul(&output))
    }

    /// Paths that existed when `snapshot` was taken but are not represented in
    /// it, because they were larger than the size cap or ignored by the
    /// project. A caller reporting an undo cannot claim to have covered them.
    ///
    /// Directory entries end in `/` and stand for everything beneath them.
    pub fn uncovered(&self, snapshot: &SnapshotId) -> Result<Vec<String>, SnapshotError> {
        let hash = snapshot.validated()?;
        match std::fs::read(self.git_dir.join(UNCOVERED_DIRECTORY).join(hash)) {
            Ok(bytes) => Ok(split_nul(&String::from_utf8_lossy(&bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(SnapshotError::Storage(error.to_string())),
        }
    }

    /// Puts `paths` back to their content in `snapshot`.
    ///
    /// A path absent from the snapshot did not exist then, so restoring it
    /// means deleting it now — but only when the snapshot could have held it.
    /// A path the snapshot never covered is absent for a reason that has
    /// nothing to do with the turn, and deleting it would destroy work no
    /// agent did. Every path is addressed as a literal top-level pathspec, so a
    /// file whose name looks like a pattern or a flag is still only ever
    /// itself.
    pub fn restore(
        &self,
        snapshot: &SnapshotId,
        paths: &[String],
    ) -> Result<RestoreReport, SnapshotError> {
        let hash = snapshot.validated()?;
        let mut report = RestoreReport::default();

        let present = self.paths_in_snapshot(hash, paths)?;
        let (in_snapshot, absent): (Vec<String>, Vec<String>) = paths
            .iter()
            .cloned()
            .partition(|path| present.contains(path));

        self.checkout_paths(hash, &in_snapshot, &mut report);

        let uncovered = self.uncovered(snapshot)?;
        let (hidden, removable): (Vec<String>, Vec<String>) = absent
            .into_iter()
            .partition(|path| is_uncovered(&uncovered, path));
        report.uncovered = hidden;
        self.remove_paths(&removable, &mut report)?;

        Ok(report)
    }

    /// Removes snapshot repositories whose project directory is gone.
    ///
    /// Snapshot objects outlive the session that wrote them, so without this
    /// the data directory keeps copies of files from projects that no longer
    /// exist. A directory this crate did not write, or did not finish writing,
    /// carries no project marker and is left alone.
    pub fn prune_orphans(data_directory: &Path) -> Result<Vec<PathBuf>, SnapshotError> {
        let root = data_directory.join("snapshots");
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(SnapshotError::Storage(error.to_string())),
        };

        let mut removed = Vec::new();
        for entry in entries {
            let directory = entry
                .map_err(|error| SnapshotError::Storage(error.to_string()))?
                .path();
            let Some(worktree) = recorded_worktree(&directory)? else {
                continue;
            };
            if worktree.exists() {
                continue;
            }

            std::fs::remove_dir_all(&directory)
                .map_err(|error| SnapshotError::Storage(error.to_string()))?;
            removed.push(directory);
        }
        Ok(removed)
    }

    fn checkout_paths(&self, hash: &str, paths: &[String], report: &mut RestoreReport) {
        for batch in batches(paths) {
            if self.checkout(hash, batch).is_ok() {
                report.restored.extend(batch.iter().cloned());
                continue;
            }

            for path in batch {
                match self.checkout(hash, std::slice::from_ref(path)) {
                    Ok(()) => report.restored.push(path.clone()),
                    Err(_) => report.failed.push(path.clone()),
                }
            }
        }
    }

    fn checkout(&self, hash: &str, paths: &[String]) -> Result<(), SnapshotError> {
        let mut arguments = vec!["checkout".to_owned(), hash.to_owned(), "--".to_owned()];
        arguments.extend(paths.iter().map(|path| literal_pathspec(path)));
        self.git_arguments("checkout", arguments, None)?;
        Ok(())
    }

    /// Deletes `paths` and drops them from the index in the same step.
    ///
    /// A path left in the index after its file is gone is invisible to every
    /// later listing, so the next capture would keep describing a file that no
    /// longer exists and a redo would find nothing to put back.
    fn remove_paths(
        &self,
        paths: &[String],
        report: &mut RestoreReport,
    ) -> Result<(), SnapshotError> {
        let mut removed = Vec::new();
        for path in paths {
            match std::fs::remove_file(self.worktree.join(path)) {
                Ok(()) => removed.push(path.clone()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    removed.push(path.clone());
                }
                Err(_) => report.failed.push(path.clone()),
            }
        }

        self.forget_paths(&removed)?;
        report.removed.extend(removed);
        Ok(())
    }

    fn forget_paths(&self, paths: &[String]) -> Result<(), SnapshotError> {
        for batch in batches(paths) {
            let mut arguments = vec![
                "update-index".to_owned(),
                "--force-remove".to_owned(),
                "--".to_owned(),
            ];
            arguments.extend(batch.iter().cloned());
            self.git_arguments("update-index", arguments, None)?;
        }
        Ok(())
    }

    fn paths_in_snapshot(
        &self,
        hash: &str,
        paths: &[String],
    ) -> Result<HashSet<String>, SnapshotError> {
        let mut present = HashSet::new();
        for batch in batches(paths) {
            let mut arguments = vec![
                "ls-tree".to_owned(),
                "-r".to_owned(),
                "--name-only".to_owned(),
                "-z".to_owned(),
                hash.to_owned(),
                "--".to_owned(),
            ];
            arguments.extend(batch.iter().map(|path| literal_pathspec(path)));
            let output = self.git_arguments("ls-tree", arguments, None)?;
            present.extend(split_nul(&output));
        }
        Ok(present)
    }

    fn record_uncovered(&self, hash: &str, entries: &[String]) -> Result<(), SnapshotError> {
        let directory = self.git_dir.join(UNCOVERED_DIRECTORY);
        create_private_directory(&directory)?;

        let temporary = directory.join(format!("{hash}.{}", unique_suffix()));
        let mut encoded = Vec::new();
        for entry in entries {
            encoded.extend_from_slice(entry.as_bytes());
            encoded.push(0);
        }

        std::fs::write(&temporary, encoded)
            .map_err(|error| SnapshotError::Storage(error.to_string()))?;
        std::fs::rename(&temporary, directory.join(hash))
            .map_err(|error| SnapshotError::Storage(error.to_string()))
    }

    /// Creates the repository on first use, points it at the project's own
    /// object database, and gives this session an index that already knows
    /// every tracked file.
    fn initialize(&self) -> Result<(), SnapshotError> {
        self.create_repository()?;
        self.borrow_project_objects()?;
        self.seed_index()?;
        std::fs::write(
            self.git_dir.join(WORKTREE_MARKER),
            format!("{}\n", self.worktree.display()),
        )
        .map_err(|error| SnapshotError::Storage(error.to_string()))
    }

    /// Builds the repository beside its final place and moves it there whole,
    /// so a second session opening the same project either finds a repository
    /// it can use or builds its own — never a half-initialised one.
    fn create_repository(&self) -> Result<(), SnapshotError> {
        if self.git_dir.join("HEAD").is_file() {
            return Ok(());
        }

        let staging = staging_directory(&self.git_dir)?;
        let outcome = self.populate_repository(&staging);
        if outcome.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
            return outcome;
        }

        if std::fs::rename(&staging, &self.git_dir).is_ok() {
            return Ok(());
        }
        let _ = std::fs::remove_dir_all(&staging);

        if self.git_dir.join("HEAD").is_file() {
            return Ok(());
        }
        Err(SnapshotError::Storage(format!(
            "could not create the snapshot repository at {}",
            self.git_dir.display()
        )))
    }

    fn populate_repository(&self, staging: &Path) -> Result<(), SnapshotError> {
        create_private_directory(staging)?;
        run_checked(&GitCall {
            directory: &self.worktree,
            operation: "init",
            arguments: &[
                "--git-dir".to_owned(),
                staging.display().to_string(),
                "--work-tree".to_owned(),
                self.worktree.display().to_string(),
                "init".to_owned(),
                "--quiet".to_owned(),
            ],
            stdin: None,
            index_file: None,
            configuration: GitConfiguration::Isolated,
        })?;

        for (key, value) in [
            ("core.autocrlf", "false"),
            ("core.longpaths", "true"),
            ("core.symlinks", "true"),
            ("core.fsmonitor", "false"),
            ("core.untrackedCache", "true"),
            ("feature.manyFiles", "true"),
        ] {
            run_checked(&GitCall {
                directory: &self.worktree,
                operation: "config",
                arguments: &[
                    "--git-dir".to_owned(),
                    staging.display().to_string(),
                    "config".to_owned(),
                    key.to_owned(),
                    value.to_owned(),
                ],
                stdin: None,
                index_file: None,
                configuration: GitConfiguration::Isolated,
            })?;
        }
        Ok(())
    }

    /// Gives this session an index that already knows every tracked file, by
    /// copying the project's own or, failing that, reading its history.
    ///
    /// Without one the snapshot index is empty and a capture of an unmodified
    /// tree writes the empty tree, an undo against which deletes the project
    /// rather than restoring it — which is why every way of ending up with no
    /// index is either recovered from or reported, never passed over.
    ///
    /// The index is per worktree. A linked worktree keeps its own next to its
    /// git directory, and the main repository's index describes a different
    /// checkout entirely.
    fn seed_index(&self) -> Result<(), SnapshotError> {
        let source = self.project_git_directory()?.join("index");
        if source.is_file() {
            std::fs::copy(&source, &self.index_file)
                .map_err(|error| SnapshotError::Storage(error.to_string()))?;
            return Ok(());
        }

        if let Some(tree) = self.project_head_tree()? {
            self.git_arguments("read-tree", vec!["read-tree".to_owned(), tree], None)?;
            return Ok(());
        }
        if self.project_is_empty()? {
            return Ok(());
        }

        Err(SnapshotError::Storage(format!(
            "the project's git index is missing at {}",
            source.display()
        )))
    }

    /// Lends the snapshot repository the project's object database.
    ///
    /// Required, not merely faster: the seeded index names blobs that live in
    /// the project's store, and a tree written over objects git cannot reach is
    /// a snapshot nothing can be restored from.
    fn borrow_project_objects(&self) -> Result<(), SnapshotError> {
        let objects = self.project_common_directory()?.join("objects");
        if !objects.is_dir() {
            return Err(SnapshotError::Storage(format!(
                "the project's object database is missing at {}",
                objects.display()
            )));
        }

        let info = self.git_dir.join("objects").join("info");
        std::fs::create_dir_all(&info)
            .map_err(|error| SnapshotError::Storage(error.to_string()))?;
        std::fs::write(info.join("alternates"), format!("{}\n", objects.display()))
            .map_err(|error| SnapshotError::Storage(error.to_string()))
    }

    /// Brings the snapshot index in line with the working tree and reports the
    /// candidate paths the size cap kept out of it.
    ///
    /// A path past the cap is also dropped from the index, so that the tree and
    /// the index agree that it is outside the snapshot. Leaving its earlier
    /// entry in place would make a restore write stale content over a file the
    /// snapshot never looked at.
    fn stage(&self) -> Result<Vec<String>, SnapshotError> {
        let candidates = self.candidate_paths()?;
        let (staged, over_cap): (Vec<String>, Vec<String>) = candidates
            .into_iter()
            .partition(|path| self.within_size_cap(path));

        if !staged.is_empty() {
            self.git(
                "add",
                &[
                    "add",
                    "--all",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ],
                Some(&encode_pathspecs(&staged)),
            )?;
        }
        self.forget_paths(&over_cap)?;
        Ok(over_cap)
    }

    /// Every path whose working-tree content may differ from what the snapshot
    /// index holds.
    ///
    /// The project's own view — modified against its `HEAD`, plus untracked
    /// files it does not ignore — is not enough on its own: a file committed,
    /// checked out or pulled after the last capture is clean against the new
    /// `HEAD` while still differing from the snapshot. The snapshot index has
    /// to be asked about itself as well, or the stale entry it keeps would be
    /// restored over content nobody asked to roll back.
    fn candidate_paths(&self) -> Result<Vec<String>, SnapshotError> {
        let mut paths = Vec::new();

        if self.project_head_tree()?.is_some() {
            let tracked = self.project_git("diff", &["diff", "--name-only", "-z", "HEAD"])?;
            extend_unique(&mut paths, split_nul(&tracked));
        }

        let untracked = self.project_git(
            "ls-files",
            &[
                "ls-files",
                "--full-name",
                "--others",
                "--exclude-standard",
                "-z",
            ],
        )?;
        extend_unique(&mut paths, split_nul(&untracked));

        let against_snapshot = self.git("diff", &["diff", "--name-only", "-z"], None)?;
        extend_unique(&mut paths, split_nul(&against_snapshot));

        Ok(paths)
    }

    /// What the project ignores and therefore keeps out of every snapshot.
    /// Whole directories are reported as one entry, so the listing stays small
    /// on a tree with a large build output.
    fn ignored_entries(&self) -> Result<Vec<String>, SnapshotError> {
        let output = self.project_git(
            "ls-files",
            &[
                "ls-files",
                "--full-name",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "--no-empty-directory",
                "-z",
            ],
        )?;
        Ok(split_nul(&output))
    }

    /// The tree the project's `HEAD` points at, or `None` when it has no
    /// commits yet.
    fn project_head_tree(&self) -> Result<Option<String>, SnapshotError> {
        let output = self.project_git_output(
            "rev-parse",
            &["rev-parse", "--verify", "--quiet", "HEAD^{tree}"],
        )?;
        if !output.success {
            return Ok(None);
        }
        Ok(Some(output.stdout.trim().to_owned()))
    }

    /// Whether the project holds no file a snapshot could describe, which is
    /// the only state in which the empty tree is an honest snapshot of it.
    fn project_is_empty(&self) -> Result<bool, SnapshotError> {
        let tracked = self.project_git("ls-files", &["ls-files", "-z"])?;
        if !split_nul(&tracked).is_empty() {
            return Ok(false);
        }
        match self.project_head_tree()? {
            Some(tree) => Ok(tree == EMPTY_TREE),
            None => Ok(true),
        }
    }

    fn project_git_directory(&self) -> Result<PathBuf, SnapshotError> {
        let output = self.project_git("rev-parse", &["rev-parse", "--absolute-git-dir"])?;
        Ok(PathBuf::from(output.trim()))
    }

    /// The directory holding the objects every worktree of the project shares.
    /// git reports it relative to the working tree unless it belongs to another
    /// one.
    fn project_common_directory(&self) -> Result<PathBuf, SnapshotError> {
        let output = self.project_git("rev-parse", &["rev-parse", "--git-common-dir"])?;
        let reported = PathBuf::from(output.trim());
        Ok(if reported.is_absolute() {
            reported
        } else {
            self.worktree.join(reported)
        })
    }

    /// A path that cannot be measured counts as within the cap, because the
    /// usual reason is that it no longer exists and its removal is exactly what
    /// the snapshot has to record.
    fn within_size_cap(&self, path: &str) -> bool {
        std::fs::metadata(self.worktree.join(path))
            .map(|metadata| !metadata.is_file() || metadata.len() <= MAX_SNAPSHOT_FILE_BYTES)
            .unwrap_or(true)
    }

    /// Runs git against the snapshot repository over the project's working tree.
    fn git(
        &self,
        operation: &str,
        arguments: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<String, SnapshotError> {
        let owned = arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>();
        self.git_arguments(operation, owned, stdin)
    }

    fn git_arguments(
        &self,
        operation: &str,
        arguments: Vec<String>,
        stdin: Option<&[u8]>,
    ) -> Result<String, SnapshotError> {
        let mut full = vec![
            "--git-dir".to_owned(),
            self.git_dir.display().to_string(),
            "--work-tree".to_owned(),
            self.worktree.display().to_string(),
        ];
        full.extend(arguments);

        run_checked(&GitCall {
            directory: &self.worktree,
            operation,
            arguments: &full,
            stdin,
            index_file: Some(&self.index_file),
            configuration: GitConfiguration::Isolated,
        })
    }

    /// Runs git against the project's own repository. Read-only by
    /// construction: only listing operations reach this.
    ///
    /// The reader's own git configuration stays in force here, because it is
    /// what decides which files the project ignores and which repositories git
    /// is willing to read at all.
    fn project_git(&self, operation: &str, arguments: &[&str]) -> Result<String, SnapshotError> {
        let output = self.project_git_output(operation, arguments)?;
        checked(operation, output)
    }

    fn project_git_output(
        &self,
        operation: &str,
        arguments: &[&str],
    ) -> Result<GitOutput, SnapshotError> {
        let owned = arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect::<Vec<_>>();
        run_git(&GitCall {
            directory: &self.worktree,
            operation,
            arguments: &owned,
            stdin: None,
            index_file: None,
            configuration: GitConfiguration::Project,
        })
    }
}

impl Drop for WorkspaceSnapshots {
    /// The per-session index is scratch state. Failing to remove it costs a
    /// stale file in the snapshot directory and must not surface as, or hide,
    /// the outcome of whatever the caller was doing.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.index_file);
    }
}

fn is_git_worktree(worktree: &Path) -> Result<bool, SnapshotError> {
    let output = run_git(&GitCall {
        directory: worktree,
        operation: "rev-parse",
        arguments: &["rev-parse".to_owned(), "--is-inside-work-tree".to_owned()],
        stdin: None,
        index_file: None,
        configuration: GitConfiguration::Project,
    })?;

    if output.success {
        return Ok(output.stdout.trim() == "true");
    }
    if output
        .stderr
        .to_lowercase()
        .contains("not a git repository")
    {
        return Ok(false);
    }

    Err(SnapshotError::Command {
        operation: "rev-parse".into(),
        detail: output.stderr.trim().to_owned(),
    })
}

/// A stable directory name for one working tree, so two projects never share a
/// snapshot repository and the same project always finds its own. The caller
/// resolves the path first, so a symlinked or unnormalised route to the same
/// tree keys to the same repository.
fn worktree_key(worktree: &Path) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(worktree.display().to_string().as_bytes());
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn staging_directory(git_dir: &Path) -> Result<PathBuf, SnapshotError> {
    let parent = git_dir.parent().ok_or_else(|| {
        SnapshotError::Storage(format!(
            "the snapshot repository has no parent directory: {}",
            git_dir.display()
        ))
    })?;
    let name = git_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            SnapshotError::Storage(format!(
                "the snapshot repository has no usable name: {}",
                git_dir.display()
            ))
        })?;
    Ok(parent.join(format!("{name}.new-{}", unique_suffix())))
}

fn recorded_worktree(directory: &Path) -> Result<Option<PathBuf>, SnapshotError> {
    match std::fs::read_to_string(directory.join(WORKTREE_MARKER)) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            Ok(Some(PathBuf::from(trimmed)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => Ok(None),
        Err(error) => Err(SnapshotError::Storage(error.to_string())),
    }
}

fn create_private_directory(path: &Path) -> Result<(), SnapshotError> {
    std::fs::create_dir_all(path).map_err(|error| SnapshotError::Storage(error.to_string()))?;
    restrict_permissions(path)
}

/// Snapshot repositories hold copies of project file content, so they are the
/// owner's business alone.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), SnapshotError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| SnapshotError::Storage(error.to_string()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), SnapshotError> {
    Ok(())
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{sequence}", std::process::id())
}

/// Whether `path` falls inside something a capture recorded as outside it.
/// Directory entries stand for their whole subtree.
fn is_uncovered(uncovered: &[String], path: &str) -> bool {
    uncovered
        .iter()
        .any(|entry| entry == path || (entry.ends_with('/') && path.starts_with(entry.as_str())))
}

/// Splits `paths` into groups small enough to pass as arguments in one go.
fn batches(paths: &[String]) -> Vec<&[String]> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut bytes = 0;

    for (index, path) in paths.iter().enumerate() {
        let cost = path.len() + 32;
        if bytes + cost > MAX_ARGUMENT_BATCH_BYTES && index > start {
            groups.push(&paths[start..index]);
            start = index;
            bytes = 0;
        }
        bytes += cost;
    }
    if start < paths.len() {
        groups.push(&paths[start..]);
    }
    groups
}

fn extend_unique(paths: &mut Vec<String>, additions: Vec<String>) {
    for path in additions {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
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

/// Whether an invocation reads the reader's git configuration.
#[derive(Clone, Copy, Eq, PartialEq)]
enum GitConfiguration {
    /// Neutral configuration, for everything that writes through the snapshot
    /// repository.
    Isolated,
    /// The reader's own configuration, for read-only questions about their
    /// project whose answers that configuration defines.
    Project,
}

struct GitCall<'a> {
    directory: &'a Path,
    operation: &'a str,
    arguments: &'a [String],
    stdin: Option<&'a [u8]>,
    index_file: Option<&'a Path>,
    configuration: GitConfiguration,
}

struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run_checked(call: &GitCall<'_>) -> Result<String, SnapshotError> {
    let output = run_git(call)?;
    checked(call.operation, output)
}

fn checked(operation: &str, output: GitOutput) -> Result<String, SnapshotError> {
    if output.success {
        return Ok(output.stdout);
    }
    Err(SnapshotError::Command {
        operation: operation.to_owned(),
        detail: output.stderr.trim().to_owned(),
    })
}

/// Runs one git invocation to completion within the timeout.
///
/// Both output streams are drained while git runs, because a pipe holds only a
/// pageful before the writer blocks and a listing of a large tree is far more
/// than that; a git left blocked on a full pipe would only ever end in a
/// timeout. Input is written from its own thread for the same reason, so a
/// child that stops reading cannot hold the caller past the deadline.
///
/// A failed write is reported as a failure of the invocation: a partially
/// written pathspec list makes git succeed over fewer paths than it was asked
/// for, which is indistinguishable from a snapshot of a smaller tree.
fn run_git(call: &GitCall<'_>) -> Result<GitOutput, SnapshotError> {
    let mut child = spawn_git(call)?;

    std::thread::scope(|scope| {
        let writer = match (call.stdin, child.stdin.take()) {
            (Some(bytes), Some(mut pipe)) => Some(scope.spawn(move || {
                pipe.write_all(bytes)?;
                pipe.flush()
            })),
            _ => None,
        };
        let stdout = child
            .stdout
            .take()
            .map(|pipe| scope.spawn(move || read_stream(pipe)));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| scope.spawn(move || read_stream(pipe)));

        let status = wait_for_exit(&mut child, call.operation);
        let stdout = join_stream(stdout, call.operation);
        let stderr = join_stream(stderr, call.operation);
        let written = join_write(writer, call.operation);

        let status = status?;
        let stdout = stdout?;
        let stderr = stderr?;
        if !status.success() {
            return Ok(GitOutput {
                success: false,
                stdout,
                stderr,
            });
        }

        written?;
        Ok(GitOutput {
            success: true,
            stdout,
            stderr,
        })
    })
}

fn spawn_git(call: &GitCall<'_>) -> Result<Child, SnapshotError> {
    let mut command = Command::new("git");
    command
        .args(HARDENING_ARGUMENTS)
        .args(call.arguments)
        .current_dir(call.directory)
        .stdin(if call.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    harden_environment(&mut command, call.configuration);
    if let Some(index) = call.index_file {
        command.env("GIT_INDEX_FILE", index);
    }

    command.spawn().map_err(|_| SnapshotError::Unavailable)
}

fn read_stream(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    pipe.read_to_end(&mut buffer)?;
    Ok(buffer)
}

fn wait_for_exit(child: &mut Child, operation: &str) -> Result<ExitStatus, SnapshotError> {
    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                return Err(terminate(child, operation, "git did not finish in time"));
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => {
                let detail = error.to_string();
                return Err(terminate(child, operation, &detail));
            }
        }
    }
}

/// Ends a git that will not be waited for again, and reaps it: an unwaited
/// child keeps its end of the pipes open, and the threads draining them would
/// never see end of file. Killing an already dead child is one of the ways this
/// succeeds.
fn terminate(child: &mut Child, operation: &str, detail: &str) -> SnapshotError {
    let _ = child.kill();
    let _ = child.wait();
    SnapshotError::Command {
        operation: operation.to_owned(),
        detail: detail.to_owned(),
    }
}

fn join_stream(
    handle: Option<ScopedJoinHandle<'_, std::io::Result<Vec<u8>>>>,
    operation: &str,
) -> Result<String, SnapshotError> {
    let Some(handle) = handle else {
        return Ok(String::new());
    };
    match handle.join() {
        Ok(Ok(bytes)) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Ok(Err(error)) => Err(SnapshotError::Command {
            operation: operation.to_owned(),
            detail: error.to_string(),
        }),
        Err(_) => Err(SnapshotError::Command {
            operation: operation.to_owned(),
            detail: "reading git output did not finish".into(),
        }),
    }
}

fn join_write(
    handle: Option<ScopedJoinHandle<'_, std::io::Result<()>>>,
    operation: &str,
) -> Result<(), SnapshotError> {
    let Some(handle) = handle else {
        return Ok(());
    };
    match handle.join() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(SnapshotError::Command {
            operation: operation.to_owned(),
            detail: error.to_string(),
        }),
        Err(_) => Err(SnapshotError::Command {
            operation: operation.to_owned(),
            detail: "writing git input did not finish".into(),
        }),
    }
}

fn harden_environment(command: &mut Command, configuration: GitConfiguration) {
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

    if configuration == GitConfiguration::Isolated {
        command
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device());
    }

    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "agens")
        .env("GIT_AUTHOR_EMAIL", "agens@localhost")
        .env("GIT_COMMITTER_NAME", "agens")
        .env("GIT_COMMITTER_EMAIL", "agens@localhost");
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

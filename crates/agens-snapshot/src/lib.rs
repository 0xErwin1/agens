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
//! No degraded path here may proceed: each is either repaired or reported. A
//! snapshot that does not describe the working tree is worse than no snapshot at
//! all, because undoing against it deletes files instead of restoring them. So
//! the index seed is a precondition rather than an optimisation and is
//! re-established before every capture that finds it gone, a capture that would
//! produce the empty tree for a non-empty project is an error, and a path the
//! snapshot could not cover is recorded so a restore never mistakes it for a
//! file the turn created.
//!
//! The same asymmetry decides what this crate is willing to delete. Removing a
//! project's snapshots throws away the last copy of content it may no longer
//! have, so absence is never inferred from a question the filesystem failed to
//! answer, and a finished record of what a capture could not cover is never
//! removed at all, because nothing visible from here proves the snapshot it
//! belongs to will not be restored to.
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
    time::{Duration, Instant, SystemTime},
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

/// Where the paths a capture could not represent are recorded, one file per
/// capture named for the tree it describes and for its own content.
const UNCOVERED_DIRECTORY: &str = "agens-uncovered";

/// The name a record carries while it is still being written, chosen so that it
/// cannot be mistaken for a finished one: a tree hash is hexadecimal and can
/// never begin with this.
const PENDING_RECORD_PREFIX: &str = "tmp-";

/// Which project a snapshot repository belongs to, so an abandoned one can be
/// recognised without guessing from its name.
const WORKTREE_MARKER: &str = "agens-worktree";

/// Where the first moment a project was found missing is recorded. The file's
/// modification time is the whole of its content.
const MISSING_MARKER: &str = "agens-missing-since";

/// What a session's private index file is called, before the part that makes it
/// this session's.
const INDEX_PREFIX: &str = "index-";

/// How long a session index must have gone untouched before another session
/// treats it as abandoned and removes it.
///
/// Age is the only usable evidence. A pid proves nothing, because the operating
/// system reuses it, so a name that looks live may belong to something else
/// entirely; whereas a session that is still running rewrites its index at
/// every capture. The threshold is set far past any plausible gap between two
/// turns of one session.
const STALE_INDEX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long a record still carrying the pending name must have gone untouched
/// before it is treated as the debris of an interrupted capture. A capture
/// renames its record into place immediately, so the only reason to wait at all
/// is that another session may be inside those few instructions right now.
const STALE_RECORD_AGE: Duration = Duration::from_secs(60 * 60);

/// How long a project must have been missing before its snapshots are removed.
///
/// At any single moment a path that cannot be reached — an unmounted share, a
/// drive that is not plugged in, an automount that has not come up — is
/// indistinguishable from one that was deleted. Time is the only thing that
/// separates them, and the snapshots are the last copy of files the project may
/// still have, so the wait is long.
const MISSING_PROJECT_GRACE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

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
        sweep_abandoned_indexes(&git_dir);
        sweep_pending_records(&git_dir);

        let index_file = git_dir.join(format!("{INDEX_PREFIX}{}", unique_suffix()));
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

        let mut uncovered = UniquePaths::default();
        uncovered.extend(over_cap);
        uncovered.extend(self.ignored_entries()?);
        self.record_uncovered(hash, &uncovered.into_vec())?;
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
    ///
    /// The answer is the union of every record written for this tree, which is
    /// what makes it safe for more than one capture to describe the same tree:
    /// see [`Self::record_uncovered`].
    pub fn uncovered(&self, snapshot: &SnapshotId) -> Result<Vec<String>, SnapshotError> {
        let hash = snapshot.validated()?;
        let directory = self.git_dir.join(UNCOVERED_DIRECTORY);
        let records = match std::fs::read_dir(&directory) {
            Ok(records) => records,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(SnapshotError::Storage(error.to_string())),
        };

        let prefix = format!("{hash}-");
        let mut entries = UniquePaths::default();
        for record in records {
            let record = record.map_err(|error| SnapshotError::Storage(error.to_string()))?;
            let name = record.file_name();
            let describes_this_tree = name
                .to_str()
                .is_some_and(|name| name == hash || name.starts_with(&prefix));
            if !describes_this_tree {
                continue;
            }

            let bytes = std::fs::read(record.path())
                .map_err(|error| SnapshotError::Storage(error.to_string()))?;
            entries.extend(split_nul(&String::from_utf8_lossy(&bytes)));
        }
        Ok(entries.into_vec())
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

    /// Removes snapshot repositories whose project directory has been gone long
    /// enough to be believed gone.
    ///
    /// Snapshot objects outlive the session that wrote them, so without this
    /// the data directory keeps copies of files from projects that no longer
    /// exist. A directory this crate did not write, or did not finish writing,
    /// carries no project marker and is left alone.
    ///
    /// Removal is the one irreversible thing this crate does, and what it
    /// removes is the only remaining copy of content the project may not have
    /// any more. So an absence is not enough on its own: a path that cannot
    /// currently be reached is left alone entirely, and one that is genuinely
    /// missing has to stay missing across [`MISSING_PROJECT_GRACE`] before its
    /// snapshots go. A project that comes back clears the record of its absence,
    /// so intermittent storage never accumulates towards a deletion.
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

            match project_presence(&worktree) {
                ProjectPresence::Present => forget_absence(&directory)?,
                ProjectPresence::Unreachable => continue,
                ProjectPresence::Absent if !absent_beyond_the_grace_period(&directory)? => continue,
                ProjectPresence::Absent => {
                    std::fs::remove_dir_all(&directory)
                        .map_err(|error| SnapshotError::Storage(error.to_string()))?;
                    removed.push(directory);
                }
            }
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

    /// Writes down what this capture could not represent, under a name carrying
    /// both the tree and the content of the record.
    ///
    /// A tree hash alone does not identify a capture. Two captures reach the
    /// same tree constantly — one turn's `after` is the next turn's `before`,
    /// and a turn that changes nothing repeats the tree it started from — while
    /// disagreeing about what lies outside it, because what a project ignores is
    /// decided by configuration that differs between sessions and changes
    /// between captures. Keyed by tree alone, the later record would replace the
    /// earlier one; a record that lost an entry that way would leave the earlier
    /// snapshot free to delete a file it was never allowed to touch.
    ///
    /// So a record is never replaced. Each distinct one is kept beside the
    /// others and [`Self::uncovered`] answers with their union, which is the
    /// only direction that is safe to be wrong in: an entry that does not apply
    /// costs a file left alone, while a missing one costs a file deleted.
    /// Naming a record for its content keeps one tree's repeats from
    /// multiplying, since the captures that agree write the same name.
    ///
    /// Across trees the set still grows, roughly one record per turn, and
    /// nothing removes a finished one. That is deliberate: the caller holds
    /// snapshot ids for as long as it offers an undo, and it may offer one taken
    /// far earlier, so no age or count this crate can see distinguishes a record
    /// nobody will ask for from a record whose loss would let a restore delete a
    /// file it was never allowed to touch. Only a capture with nothing outside
    /// it is skipped, which says the same as the empty record it would write.
    ///
    /// The record is written under a name no reader looks at and moved into
    /// place, so a union taken while a capture is running sees whole records or
    /// nothing, never a truncated one. A write that dies between the two leaves
    /// the pending name behind, which [`sweep_pending_records`] collects.
    fn record_uncovered(&self, hash: &str, entries: &[String]) -> Result<(), SnapshotError> {
        if entries.is_empty() {
            return Ok(());
        }

        let directory = self.git_dir.join(UNCOVERED_DIRECTORY);
        create_private_directory(&directory)?;

        let mut encoded = Vec::new();
        for entry in entries {
            encoded.extend_from_slice(entry.as_bytes());
            encoded.push(0);
        }

        let name = format!("{hash}-{}", content_key(&encoded));
        let temporary = directory.join(format!("{PENDING_RECORD_PREFIX}{}", unique_suffix()));

        std::fs::write(&temporary, encoded)
            .map_err(|error| SnapshotError::Storage(error.to_string()))?;
        std::fs::rename(&temporary, directory.join(name))
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

    /// Re-establishes the seeded index before anything stages through it.
    ///
    /// [`Self::seed_index`] runs once at open, but the file it produces can be
    /// gone by the next capture: another session's sweep, a temporary-directory
    /// reaper or a reader clearing the data directory all remove it, and none of
    /// them announce it. Staging into a fresh empty index is the one degraded
    /// path that stays silent — git refuses an index it cannot parse, but an
    /// absent one it simply creates — and the tree that comes out names only the
    /// handful of paths a listing of the working tree offered. Every file it
    /// leaves out reads as a file that did not exist, which a restore to that
    /// tree acts on by deleting them.
    ///
    /// Seeding is defined by the project's own state rather than by the
    /// session's history, so running it again produces exactly what the first
    /// capture staged through: this restores the precondition rather than
    /// papering over its loss.
    fn ensure_index(&self) -> Result<(), SnapshotError> {
        if self.index_file.is_file() {
            return Ok(());
        }
        self.seed_index()
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
        self.ensure_index()?;

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
        let mut paths = UniquePaths::default();

        if self.project_head_tree()?.is_some() {
            let tracked = self.project_git("diff", &["diff", "--name-only", "-z", "HEAD"])?;
            paths.extend(split_nul(&tracked));
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
        paths.extend(split_nul(&untracked));

        let against_snapshot = self.git("diff", &["diff", "--name-only", "-z"], None)?;
        paths.extend(split_nul(&against_snapshot));

        Ok(paths.into_vec())
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
    content_key(worktree.display().to_string().as_bytes())
}

/// A short, stable, file-name-safe name for a run of bytes.
fn content_key(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    Sha256::digest(bytes)
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

/// What is known about a project directory, kept apart because only one of the
/// three answers means the project was deleted.
enum ProjectPresence {
    Present,
    /// Nothing is at the path, and the filesystem holding it answered.
    Absent,
    /// The filesystem could not answer. Unmounted, disconnected, or closed to
    /// this process.
    Unreachable,
}

/// Asks about `worktree` in a way that keeps "there is nothing here" apart from
/// "this could not be looked up", which `Path::exists` folds into one answer.
///
/// The link itself is what is asked about: a dangling symlink is a project path
/// that still exists and points somewhere the reader chose.
fn project_presence(worktree: &Path) -> ProjectPresence {
    match std::fs::symlink_metadata(worktree) {
        Ok(_) => ProjectPresence::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProjectPresence::Absent,
        Err(_) => ProjectPresence::Unreachable,
    }
}

/// Whether the project belonging to `directory` has been missing for longer than
/// the grace period, starting the clock on the first prune that finds it gone.
fn absent_beyond_the_grace_period(directory: &Path) -> Result<bool, SnapshotError> {
    let marker = directory.join(MISSING_MARKER);
    let first_missed = match std::fs::metadata(&marker) {
        Ok(metadata) => metadata
            .modified()
            .map_err(|error| SnapshotError::Storage(error.to_string()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&marker, [])
                .map_err(|error| SnapshotError::Storage(error.to_string()))?;
            return Ok(false);
        }
        Err(error) => return Err(SnapshotError::Storage(error.to_string())),
    };

    Ok(SystemTime::now()
        .duration_since(first_missed)
        .is_ok_and(|absence| absence >= MISSING_PROJECT_GRACE))
}

/// Restarts the clock for a project that is there again.
///
/// A failure here is reported rather than passed over. A marker left behind
/// carries an absence that has already ended, and the next prune to find the
/// project missing would read it as an absence stretching back to then and
/// remove the snapshots on the strength of it.
fn forget_absence(directory: &Path) -> Result<(), SnapshotError> {
    match std::fs::remove_file(directory.join(MISSING_MARKER)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
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

/// Removes the index files of sessions that ended without running their
/// cleanup, so a repository does not collect one per crash forever.
///
/// A file this process owns is kept whatever its age, since another
/// [`WorkspaceSnapshots`] alive in this process may be between two of its own
/// git calls. Everything else is judged by [`STALE_INDEX_AGE`].
///
/// Getting that judgement wrong costs a seed, not a file: a session whose index
/// is taken from underneath it rebuilds it from the project before its next
/// capture stages through it. See [`WorkspaceSnapshots::ensure_index`], which is
/// what makes this sweep a matter of housekeeping rather than of correctness.
///
/// Best effort throughout: a directory that cannot be read or a file that
/// cannot be removed is left for the next session rather than failing an open
/// that has nothing else wrong with it.
fn sweep_abandoned_indexes(git_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(git_dir) else {
        return;
    };

    let own = format!("{INDEX_PREFIX}{}-", std::process::id());
    let now = SystemTime::now();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_other_session = name
            .to_str()
            .is_some_and(|name| name.starts_with(INDEX_PREFIX) && !name.starts_with(&own));
        if !is_other_session {
            continue;
        }

        if untouched_for(&entry, now, STALE_INDEX_AGE) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Removes the records of captures that died between writing one and moving it
/// into place, so an interrupted session does not leave a file nothing will ever
/// read or replace.
///
/// Only the pending name is swept. A finished record is what stops a restore
/// from deleting a file the capture could not hold, and the caller may still
/// hold the snapshot id it belongs to, so nothing here decides that one has
/// outlived its use.
///
/// Best effort, and by age: a capture running right now is between the write and
/// the rename of a file with exactly this name.
fn sweep_pending_records(git_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(git_dir.join(UNCOVERED_DIRECTORY)) else {
        return;
    };

    let now = SystemTime::now();
    for entry in entries.flatten() {
        let is_pending = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(PENDING_RECORD_PREFIX));
        if is_pending && untouched_for(&entry, now, STALE_RECORD_AGE) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether `entry` was last written at least `age` ago. An entry whose age
/// cannot be established counts as recent, so an unreadable modification time
/// leaves the file where it is.
fn untouched_for(entry: &std::fs::DirEntry, now: SystemTime, age: Duration) -> bool {
    entry
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|elapsed| elapsed >= age)
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

/// Paths in the order they were first seen, with repeats dropped.
///
/// Membership is answered by a set rather than by scanning what has been kept,
/// because a capture runs at every turn boundary and a large tree offers it
/// thousands of candidates.
#[derive(Default)]
struct UniquePaths {
    order: Vec<String>,
    seen: HashSet<String>,
}

impl UniquePaths {
    fn extend(&mut self, additions: Vec<String>) {
        for path in additions {
            if self.seen.insert(path.clone()) {
                self.order.push(path);
            }
        }
    }

    fn into_vec(self) -> Vec<String> {
        self.order
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

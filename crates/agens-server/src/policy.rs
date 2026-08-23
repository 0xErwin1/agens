//! What the operator has decided about the repositories a daemon serves.
//!
//! One daemon serves N projects and every one of them arrives by name, from a
//! client, over a socket that authenticates nobody. Three decisions therefore
//! cannot be derived from the request:
//!
//! - **Which checkouts are servable at all.** A `repo_root` is a path a caller
//!   chose, so the daemon compares its canonical form against roots the
//!   operator wrote down rather than trusting the name it was handed.
//! - **Whose provisioning hooks may run.** A hook is repository code executed
//!   with the daemon's whole environment, provider credentials included. It
//!   runs when the operator has said so about that repository, and never
//!   because a request asked nicely.
//! - **What a hook may export.** An exported name lands in the environment of
//!   every hook after it, so an unrestricted export is a hook rewriting the
//!   next hook's `PATH`.
//!
//! The two halves have different writers, so they live in different places.
//! The roots and the export allowlist are the operator's alone and are read
//! from the configuration file (`team.project_roots`, `team.hook_exports`),
//! where nothing the daemon runs can add to them. Trust in a repository's hooks
//! moves while the daemon runs — a durable question is answered, or the
//! operator runs `agens serve trust` — so it is recorded in the control plane,
//! not in a document a run's own worktree could append its fingerprint to.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agens_store::{
    RepositoryPolicyStore, StoredHookTrust, StoredPendingTrust, unified_database_path,
};
use agens_tools::SessionWorktrees;
use serde::{Deserialize, Serialize};

use crate::api::{PortError, WorktreeGate};
use crate::ports::GitWorktreeGate;

/// The branch a gate built only to fingerprint a repository is pointed at.
///
/// Identity is derived from the git common directory and the `origin` URL, so
/// nothing this gate does reads it. It is named rather than left empty so the
/// value is not mistaken for configuration somebody has to keep in step.
const MAIN_REF_FOR_IDENTITY: &str = "main";

/// The file earlier versions kept this policy in.
///
/// It is refused rather than ignored: an operator who wrote their project roots
/// into it would otherwise get a daemon that serves nothing and says only that
/// the checkout is not served.
const RETIRED_POLICY_FILE: &str = "worktree-policy.toml";

/// What the operator has said about one repository's provisioning hooks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookTrust {
    /// The operator has authorized this repository's hooks.
    Granted,
    /// The operator has refused them. Asking again would be nagging about a
    /// decision that was already made.
    Refused,
    /// Nothing has been decided, so the first run of this repository asks.
    Unknown,
    /// The register could not be read, so nothing is known and nothing is
    /// permitted.
    ///
    /// It is separate from [`Self::Refused`] because the two are refusals for
    /// opposite reasons: one is the operator's decision, and the other is the
    /// daemon unable to reach it. Collapsing them made a poisoned mutex or a
    /// SQLite error look exactly like a repository somebody had said no to,
    /// permanently and with nothing written down.
    Unreadable(TrustReadFailure),
}

/// Why the hook-trust register could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustReadFailure {
    /// The register's lock was left poisoned by a panicking operation.
    Poisoned,
    /// The query itself failed.
    Storage,
}

impl TrustReadFailure {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Poisoned => "poisoned",
            Self::Storage => "storage",
        }
    }
}

/// The half of the policy the operator writes by hand.
///
/// It arrives from the configuration rather than being read here because this
/// crate composes a daemon and does not own the configuration surface: the
/// keys, their validation and their file are `agens-config`'s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicySettings {
    /// The checkouts, or the directories containing them, that runs may be
    /// created against. Empty admits nothing: a daemon reachable by any local
    /// client is not a place for a permissive default.
    pub project_roots: Vec<PathBuf>,
    /// The environment names a provisioning hook may export. Empty exports
    /// nothing, which is what a repository that never asked for an export
    /// expects.
    pub hook_exports: Vec<String>,
    /// The configuration file the roots are written in, named in the refusal a
    /// request against an unserved checkout gets. `None` when the caller
    /// composed the policy without a file behind it.
    pub config_path: Option<PathBuf>,
}

/// The operator's decisions, as the service core reads them.
///
/// A trait rather than the concrete store because the core is built and tested
/// before a data directory exists, and because a suite proving what the core
/// does with a refusal should not have to write a file to produce one.
pub trait RepositoryPolicy: Send + Sync {
    /// Whether a canonical checkout path is one this daemon serves.
    fn admits(&self, repository: &Path) -> bool;

    /// A sentence naming what the operator would have to write down for
    /// [`Self::admits`] to accept this path. Returned rather than composed by
    /// the caller so the refusal names the real key and the real file.
    fn admission_remedy(&self) -> String;

    fn hook_trust(&self, repo_id: &str) -> HookTrust;

    /// The environment names a hook may export.
    fn hook_exports(&self) -> Vec<String>;

    /// Records that a repository's hooks are pending an operator's answer to
    /// `question_id`, so the answer can be applied without the question having
    /// to carry the repository's identity in its prose.
    fn record_pending(&self, pending: &PendingHookTrust) -> Result<(), PortError>;

    /// Whether this question is one whose answer decides a repository's hooks.
    fn is_pending(&self, question_id: i64) -> bool;

    /// Applies an answer to a question [`Self::record_pending`] recorded,
    /// reporting whether that question was one of them.
    fn resolve_pending(&self, question_id: i64, granted: bool) -> Result<bool, PortError>;
}

/// A repository whose hooks are waiting on one durable question.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct PendingHookTrust {
    pub question_id: i64,
    pub repo_id: String,
    /// The canonical checkout, kept so a person reading the question knows what
    /// they are being asked about.
    pub repository: PathBuf,
    pub asked_at: i64,
}

/// The operator's configuration, over the control plane's trust register.
///
/// The register itself is not shown: it is a database handle, and what a caller
/// would want to read is the settings behind the decisions.
pub struct PolicyStore {
    settings: PolicySettings,
    register: Register,
}

/// Where hook decisions are kept.
///
/// The in-memory arm exists for a caller that composes a core without a data
/// directory behind it. It is not a cache of the other one: a store has exactly
/// one of the two.
enum Register {
    Database(Mutex<RepositoryPolicyStore>),
    Memory(Mutex<MemoryRegister>),
}

#[derive(Default)]
struct MemoryRegister {
    decided: Vec<(String, bool)>,
    pending: Vec<PendingHookTrust>,
}

impl std::fmt::Debug for PolicyStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PolicyStore")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl PolicyStore {
    /// Opens the trust register the operator's configuration is read against.
    ///
    /// The register's file is checked for ownership and mode before it is
    /// opened, because opening it restores the mode the store's open contract
    /// asks for and would erase the evidence. Every other table is recoverable
    /// from the work it describes; this one decides whose code runs with the
    /// daemon's credentials, so a file the daemon's user does not own, or that
    /// anyone beyond that user can reach, stops the daemon instead of being
    /// quietly repaired.
    pub fn open(data_directory: &Path, settings: PolicySettings) -> Result<Self, PolicyError> {
        let retired = data_directory.join(RETIRED_POLICY_FILE);
        if retired.exists() {
            return Err(PolicyError::new(
                &retired,
                "this file no longer configures anything: move its project_roots and hook_exports \
                 into team.project_roots and team.hook_exports, grant hook trust with \
                 `agens serve trust <repository>`, and delete it",
            ));
        }

        let database = unified_database_path(data_directory);
        verify_private(&database)?;

        let store = RepositoryPolicyStore::open(data_directory)
            .map_err(|error| PolicyError::new(&database, error))?;

        Ok(Self {
            settings,
            register: Register::Database(Mutex::new(store)),
        })
    }

    /// A policy held in memory, for a caller that composes a core without a
    /// data directory behind it.
    #[must_use]
    pub fn in_memory(project_roots: Vec<PathBuf>, hook_exports: Vec<String>) -> Self {
        Self {
            settings: PolicySettings {
                project_roots,
                hook_exports,
                config_path: None,
            },
            register: Register::Memory(Mutex::new(MemoryRegister::default())),
        }
    }

    /// Records the operator's own decision about a repository's hooks, taken
    /// without a question having been asked.
    pub fn decide(
        &self,
        repo_id: &str,
        repository: &Path,
        granted: bool,
        now: i64,
    ) -> Result<(), PortError> {
        match &self.register {
            Register::Database(store) => lock(store)?
                .decide(repo_id, repository, granted, now)
                .map_err(|error| PortError::new("policy", error.to_string())),
            Register::Memory(memory) => {
                let mut memory = lock(memory)?;
                memory.decided.retain(|(known, _)| known != repo_id);
                memory.decided.push((repo_id.to_owned(), granted));

                Ok(())
            }
        }
    }
}

fn lock<T>(held: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, PortError> {
    held.lock()
        .map_err(|_| PortError::new("policy", "the repository policy is unusable"))
}

impl RepositoryPolicy for PolicyStore {
    fn admits(&self, repository: &Path) -> bool {
        self.settings
            .project_roots
            .iter()
            .any(|root| is_within(repository, root))
    }

    fn admission_remedy(&self) -> String {
        let Some(config) = &self.settings.config_path else {
            return "the daemon serves no configured project root".to_owned();
        };

        format!(
            "add the checkout, or a directory above it, to team.project_roots in {}, \
             then restart the daemon",
            config.display()
        )
    }

    fn hook_trust(&self, repo_id: &str) -> HookTrust {
        let stored =
            match &self.register {
                Register::Database(store) => lock(store)
                    .map_err(|_| TrustReadFailure::Poisoned)
                    .and_then(|store| {
                        store
                            .hook_trust(repo_id)
                            .map_err(|_| TrustReadFailure::Storage)
                    }),
                Register::Memory(memory) => lock(memory)
                    .map_err(|_| TrustReadFailure::Poisoned)
                    .map(|memory| {
                        memory
                            .decided
                            .iter()
                            .find(|(known, _)| known == repo_id)
                            .map_or(StoredHookTrust::Unknown, |(_, granted)| {
                                if *granted {
                                    StoredHookTrust::Granted
                                } else {
                                    StoredHookTrust::Refused
                                }
                            })
                    }),
            };

        // A register that could not be read has decided nothing, and nothing
        // decided is not permission: an unreadable policy refuses hooks rather
        // than falling through to the question that would ask about them again.
        // It says which of the two it is, because a caller that cannot tell
        // them apart cannot say anything either.
        match stored {
            Ok(StoredHookTrust::Granted) => HookTrust::Granted,
            Ok(StoredHookTrust::Unknown) => HookTrust::Unknown,
            Ok(StoredHookTrust::Refused) => HookTrust::Refused,
            Err(failure) => HookTrust::Unreadable(failure),
        }
    }

    fn hook_exports(&self) -> Vec<String> {
        self.settings.hook_exports.clone()
    }

    fn record_pending(&self, pending: &PendingHookTrust) -> Result<(), PortError> {
        match &self.register {
            Register::Database(store) => lock(store)?
                .record_pending(&StoredPendingTrust {
                    question_id: pending.question_id,
                    repo_id: pending.repo_id.clone(),
                    repository: pending.repository.clone(),
                    asked_at: pending.asked_at,
                })
                .map_err(|error| PortError::new("policy", error.to_string())),
            Register::Memory(memory) => {
                let mut memory = lock(memory)?;
                memory
                    .pending
                    .retain(|other| other.question_id != pending.question_id);
                memory.pending.push(pending.clone());

                Ok(())
            }
        }
    }

    fn is_pending(&self, question_id: i64) -> bool {
        match &self.register {
            Register::Database(store) => lock(store)
                .ok()
                .and_then(|store| store.is_pending(question_id).ok())
                .unwrap_or_default(),
            Register::Memory(memory) => lock(memory).is_ok_and(|memory| {
                memory
                    .pending
                    .iter()
                    .any(|pending| pending.question_id == question_id)
            }),
        }
    }

    fn resolve_pending(&self, question_id: i64, granted: bool) -> Result<bool, PortError> {
        match &self.register {
            Register::Database(store) => lock(store)?
                .resolve_pending(question_id, granted)
                .map_err(|error| PortError::new("policy", error.to_string())),
            Register::Memory(memory) => {
                let mut memory = lock(memory)?;
                let Some(position) = memory
                    .pending
                    .iter()
                    .position(|pending| pending.question_id == question_id)
                else {
                    return Ok(false);
                };

                let pending = memory.pending.remove(position);
                memory
                    .decided
                    .retain(|(known, _)| *known != pending.repo_id);
                memory.decided.push((pending.repo_id, granted));

                Ok(true)
            }
        }
    }
}

/// The repository one `trust` grant landed on.
#[derive(Debug)]
pub struct TrustedRepository {
    pub repo_id: String,
    /// The checkout as the daemon resolved it, which is what the grant is
    /// recorded against.
    pub repository: PathBuf,
}

/// Grants a repository's provisioning hooks the operator's trust, without a
/// run having asked for it.
///
/// It is the operator's verb rather than the daemon's, so it writes the same
/// register the daemon reads and takes effect on the next run of that
/// repository: the daemon reads trust through to the control plane on every
/// request and has no cached document to invalidate.
///
/// The checkout has to be one the daemon serves. A grant against a repository
/// outside `team.project_roots` would be a decision that never applies to
/// anything, and the refusal names the same remedy a run against it would get.
pub fn trust_repository(
    data_directory: &Path,
    settings: PolicySettings,
    repository: &Path,
    now: i64,
) -> Result<TrustedRepository, PolicyError> {
    let canonical = repository
        .canonicalize()
        .map_err(|error| PolicyError::new(repository, error))?;

    let policy = PolicyStore::open(data_directory, settings)?;

    if !policy.admits(&canonical) {
        return Err(PolicyError::detail(format!(
            "the daemon does not serve {}: {}",
            canonical.display(),
            policy.admission_remedy()
        )));
    }

    let identity = GitWorktreeGate::new(
        SessionWorktrees::new(data_directory),
        MAIN_REF_FOR_IDENTITY,
        Vec::new(),
    )
    .identify(&canonical)
    .map_err(|error| PolicyError::new(&canonical, error.detail()))?;

    policy
        .decide(&identity.repo_id, &canonical, true, now)
        .map_err(|error| PolicyError::new(&canonical, error.detail()))?;

    Ok(TrustedRepository {
        repo_id: identity.repo_id,
        repository: canonical,
    })
}

/// Why the policy could not be read.
#[derive(Debug)]
pub struct PolicyError(String);

impl PolicyError {
    fn new(path: &Path, detail: impl std::fmt::Display) -> Self {
        Self(format!("{}: {detail}", path.display()))
    }

    fn detail(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PolicyError {}

/// Whether `repository` is `root` or lives under it.
///
/// Both sides are compared as whole path components, so a root of `/home/dev`
/// admits `/home/dev/agens` and refuses `/home/development`, which a prefix
/// comparison on the string would not.
fn is_within(repository: &Path, root: &Path) -> bool {
    let root = root.canonicalize();

    root.is_ok_and(|root| repository == root || repository.starts_with(&root))
}

/// Refuses a register file the daemon's user does not own, or that anyone
/// beyond that user can read or write.
#[cfg(unix)]
fn verify_private(path: &Path) -> Result<(), PolicyError> {
    use std::os::unix::fs::MetadataExt;

    let Ok(metadata) = std::fs::metadata(path) else {
        return Ok(());
    };

    // SAFETY: `geteuid` reads the calling process's effective user and cannot
    // fail or touch memory the caller owns.
    let effective = unsafe { libc::geteuid() };

    if metadata.uid() != effective {
        return Err(PolicyError::detail(format!(
            "{} holds the repository policy and is owned by another user",
            path.display()
        )));
    }

    if metadata.mode() & 0o077 != 0 {
        return Err(PolicyError::detail(format!(
            "{} holds the repository policy and is reachable beyond its owner",
            path.display()
        )));
    }

    Ok(())
}

#[cfg(not(unix))]
fn verify_private(_: &Path) -> Result<(), PolicyError> {
    Ok(())
}

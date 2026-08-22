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
//! The policy is a file rather than a section of the user's configuration for
//! one reason: the daemon writes to it. Trust in a repository is granted by
//! answering a durable question, and an answer that has to be transcribed into
//! a hand-edited configuration file is an answer that never takes effect.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::api::PortError;

/// The file, under the daemon's data directory, that holds all of it.
const POLICY_FILE: &str = "worktree-policy.toml";

/// A policy larger than this is a corrupted or hostile file rather than a
/// configuration somebody wrote.
const MAX_POLICY_BYTES: u64 = 256 * 1024;

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
    /// the caller so the refusal names the real file.
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
    /// The canonical checkout, kept so a person reading the file knows what
    /// they are being asked about.
    pub repository: PathBuf,
    pub asked_at: i64,
}

/// The policy file, read at start and written back whenever trust moves.
pub struct PolicyStore {
    path: PathBuf,
    document: Mutex<PolicyDocument>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct PolicyDocument {
    /// The checkouts, or the directories containing them, that runs may be
    /// created against. Empty admits nothing: a daemon reachable by any local
    /// client is not a place for a permissive default.
    project_roots: Vec<PathBuf>,
    /// The environment names a provisioning hook may export. Empty exports
    /// nothing, which is what a repository that never asked for an export
    /// expects.
    hook_exports: Vec<String>,
    /// Repositories whose hooks the operator authorized, by fingerprint.
    granted: BTreeMap<String, TrustRecord>,
    /// Repositories whose hooks the operator refused.
    refused: BTreeMap<String, TrustRecord>,
    /// Questions whose answers have not arrived yet.
    pending: Vec<PendingHookTrust>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TrustRecord {
    repository: PathBuf,
    decided_at: i64,
}

impl PolicyStore {
    /// Reads the policy the operator wrote, or an empty one when the daemon
    /// has never been configured.
    ///
    /// A file that cannot be parsed is an error rather than an empty policy:
    /// the empty policy admits nothing and refuses every hook, so silently
    /// falling back to it would turn a typo into a daemon that answers every
    /// request the same unhelpful way.
    pub fn open(data_directory: &Path) -> Result<Self, PolicyError> {
        let path = data_directory.join(POLICY_FILE);

        let document = match fs::metadata(&path) {
            Err(_) => PolicyDocument::default(),
            Ok(metadata) if metadata.len() > MAX_POLICY_BYTES => {
                return Err(PolicyError::new(
                    &path,
                    format!("the policy is larger than {MAX_POLICY_BYTES} bytes"),
                ));
            }
            Ok(_) => {
                let text =
                    fs::read_to_string(&path).map_err(|error| PolicyError::new(&path, error))?;

                toml::from_str(&text).map_err(|error| PolicyError::new(&path, error))?
            }
        };

        Ok(Self {
            path,
            document: Mutex::new(document),
        })
    }

    /// A policy held in memory, for a caller that composes a core without a
    /// data directory behind it.
    #[must_use]
    pub fn in_memory(project_roots: Vec<PathBuf>, hook_exports: Vec<String>) -> Self {
        Self {
            path: PathBuf::new(),
            document: Mutex::new(PolicyDocument {
                project_roots,
                hook_exports,
                ..PolicyDocument::default()
            }),
        }
    }

    /// Where the operator edits this policy.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn with_document<T>(&self, read: impl FnOnce(&PolicyDocument) -> T, fallback: T) -> T {
        self.document
            .lock()
            .map_or(fallback, |document| read(&document))
    }

    /// Applies a change and writes the whole document back.
    ///
    /// Written in full rather than appended to because the file is also the
    /// operator's to edit, and a merge of two writers over a hand-edited file
    /// is a problem this does not need to have: the daemon is the only process
    /// that writes it while it runs.
    fn mutate(&self, change: impl FnOnce(&mut PolicyDocument)) -> Result<(), PortError> {
        let mut document = self
            .document
            .lock()
            .map_err(|_| PortError::new("policy", "the repository policy is unusable"))?;

        change(&mut document);

        if self.path.as_os_str().is_empty() {
            return Ok(());
        }

        let text = toml::to_string_pretty(&*document)
            .map_err(|error| PortError::new("policy", error.to_string()))?;

        write_private(&self.path, &text).map_err(|error| PortError::new("policy", error))
    }
}

impl RepositoryPolicy for PolicyStore {
    fn admits(&self, repository: &Path) -> bool {
        self.with_document(
            |document| {
                document
                    .project_roots
                    .iter()
                    .any(|root| is_within(repository, root))
            },
            false,
        )
    }

    fn admission_remedy(&self) -> String {
        if self.path.as_os_str().is_empty() {
            return "the daemon serves no configured project root".to_owned();
        }

        format!(
            "add the checkout, or a directory above it, to project_roots in {}",
            self.path.display()
        )
    }

    fn hook_trust(&self, repo_id: &str) -> HookTrust {
        self.with_document(
            |document| {
                if document.granted.contains_key(repo_id) {
                    HookTrust::Granted
                } else if document.refused.contains_key(repo_id) {
                    HookTrust::Refused
                } else {
                    HookTrust::Unknown
                }
            },
            HookTrust::Refused,
        )
    }

    fn hook_exports(&self) -> Vec<String> {
        self.with_document(|document| document.hook_exports.clone(), Vec::new())
    }

    fn record_pending(&self, pending: &PendingHookTrust) -> Result<(), PortError> {
        self.mutate(|document| {
            document
                .pending
                .retain(|other| other.question_id != pending.question_id);
            document.pending.push(pending.clone());
        })
    }

    fn is_pending(&self, question_id: i64) -> bool {
        self.with_document(
            |document| {
                document
                    .pending
                    .iter()
                    .any(|pending| pending.question_id == question_id)
            },
            false,
        )
    }

    fn resolve_pending(&self, question_id: i64, granted: bool) -> Result<bool, PortError> {
        let mut resolved = false;

        self.mutate(|document| {
            let Some(position) = document
                .pending
                .iter()
                .position(|pending| pending.question_id == question_id)
            else {
                return;
            };

            let pending = document.pending.remove(position);
            let record = TrustRecord {
                repository: pending.repository,
                decided_at: pending.asked_at,
            };

            if granted {
                document.refused.remove(&pending.repo_id);
                document.granted.insert(pending.repo_id, record);
            } else {
                document.granted.remove(&pending.repo_id);
                document.refused.insert(pending.repo_id, record);
            }

            resolved = true;
        })?;

        Ok(resolved)
    }
}

/// Why the policy could not be read.
#[derive(Debug)]
pub struct PolicyError(String);

impl PolicyError {
    fn new(path: &Path, detail: impl std::fmt::Display) -> Self {
        Self(format!("{}: {detail}", path.display()))
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

/// Writes the policy so only its owner can read it.
///
/// It names every repository the daemon serves and every one whose code it is
/// willing to execute, which is a map of what a local attacker would want to
/// change rather than merely read.
fn write_private(path: &Path, text: &str) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path).map_err(|error| error.to_string())?;

    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())
}

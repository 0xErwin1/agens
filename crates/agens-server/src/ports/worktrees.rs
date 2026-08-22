//! The narrow git seam the `Cleaning` RPC goes through.
//!
//! It derives and it removes, and it applies no transition: the core moves the
//! worktree row through the machines it owns, and this only tells it what git
//! says and disposes of the directory once the move has landed. That is the
//! whole difference from [`crate::Gates`], whose `reclaim` is the complete
//! sweep the coordinator runs on its own — both end up moving the same rows
//! through the same machines.
//!
//! Every field of a derivation comes from one pass over the worktree that is
//! on disk right now, through the same code the pre-merge gate re-derives with,
//! so the two sides of a receipt comparison cannot disagree about what a digest
//! of one worktree is.

use std::path::{Path, PathBuf};

use agens_store::RunRow;
use agens_tools::{
    HookAuthorization, HookAuthorizationRequest, HookFailure, HookFailureResponse,
    ProvisioningDecisions, ProvisioningOutcome, ProvisioningRequest, SessionWorktrees,
    WorktreeProvisioner,
};

use crate::api::{
    PortError, ProvisionedWorktree, RepositoryIdentity, WorktreeDerivation, WorktreeGate,
    WorktreeRequest,
};
use crate::gates::receipt_of;

/// How many hexadecimal characters of the repository digest the fingerprint
/// keeps. Sixteen, as the design fixes it: enough that two repositories on one
/// machine never collide, short enough to read in a path and a log line.
const FINGERPRINT_CHARS: usize = 16;

/// Git derivation and disposal over the daemon's own worktree service.
pub struct GitWorktreeGate {
    worktrees: SessionWorktrees,
    /// The branch a run's work is measured against. Configuration, so it is
    /// held here rather than read from a request: a caller that named its own
    /// target could have a branch declared merged into something nobody
    /// integrates.
    main_ref: String,
}

impl GitWorktreeGate {
    #[must_use]
    pub fn new(worktrees: SessionWorktrees, main_ref: impl Into<String>) -> Self {
        Self {
            worktrees,
            main_ref: main_ref.into(),
        }
    }
}

impl WorktreeGate for GitWorktreeGate {
    fn derive(&self, run: &RunRow) -> Result<WorktreeDerivation, PortError> {
        let path = worktree_path(run)?;

        let derivation = self
            .worktrees
            .derive(&path, &self.main_ref)
            .map_err(|error| PortError::new("worktrees", error.to_string()))?;

        let receipt = receipt_of(&derivation);

        Ok(WorktreeDerivation {
            branch_merged: derivation.merged,
            worktree_clean: !derivation.dirty,
            tree_hash: receipt.tree_hash,
            paths_digest: receipt.paths_digest,
        })
    }

    /// The fingerprint every worktree of one repository shares.
    ///
    /// Derived from the git common directory and the `origin` URL, which is
    /// what a worktree shares with its checkout: only `--show-toplevel`
    /// separates the two, and that is the identity confinement uses, not this
    /// one.
    fn identify(&self, repository: &Path) -> Result<RepositoryIdentity, PortError> {
        use sha2::{Digest, Sha256};

        let identity = self
            .worktrees
            .repository_identity(repository)
            .map_err(|error| PortError::new("worktrees", error.to_string()))?;

        let mut digest = Sha256::new();
        digest.update(identity.common_directory.display().to_string().as_bytes());
        if let Some(remote_url) = &identity.remote_url {
            digest.update([0x1f]);
            digest.update(remote_url.as_bytes());
        }

        let repo_id = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            .chars()
            .take(FINGERPRINT_CHARS)
            .collect();

        Ok(RepositoryIdentity {
            repo_id,
            remote_url: identity.remote_url,
        })
    }

    fn provision(&self, request: &WorktreeRequest<'_>) -> Result<ProvisionedWorktree, PortError> {
        let path = self
            .worktrees
            .create(
                request.repository,
                request.repo_id,
                request.name,
                request.branch,
                request.start_point,
            )
            .map_err(|error| PortError::new("worktrees", error.to_string()))?;

        let outcome = WorktreeProvisioner::new(self.worktrees.clone())
            .provision(
                &ProvisioningRequest {
                    repository: request.repository,
                    repository_id: request.repo_id,
                    name: request.name,
                    branch: request.branch,
                },
                &CoordinatorProvisioning,
            )
            .map_err(|error| PortError::new("worktrees", error.to_string()))?;

        match outcome {
            ProvisioningOutcome::NotDeclared => Ok(ProvisionedWorktree {
                path,
                hook_failures: Vec::new(),
            }),
            ProvisioningOutcome::Applied(report) => Ok(ProvisionedWorktree {
                path,
                hook_failures: report
                    .failures
                    .iter()
                    .map(|failure| format!("{}: {}", failure.name, failure.output))
                    .collect(),
            }),
            // The worktree and its branch are already gone, so the run has
            // nothing to be created against.
            ProvisioningOutcome::Aborted(failure) => Err(PortError::new(
                "worktrees",
                format!(
                    "the repository's provisioning hook {} did not succeed: {}",
                    failure.name, failure.output
                ),
            )),
        }
    }

    fn remove(&self, run: &RunRow) -> Result<(), PortError> {
        let path = worktree_path(run)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| PortError::new("worktrees", "the worktree path names no directory"))?;

        self.worktrees
            .remove(Path::new(&run.repo_root), &run.repo_id, name)
            .map_err(|error| PortError::new("worktrees", error.to_string()))
    }
}

/// Where the run's work lives, refused rather than guessed when the row carries
/// no path: a run with no worktree has nothing to derive and nothing to remove.
fn worktree_path(run: &RunRow) -> Result<PathBuf, PortError> {
    run.worktree_path
        .as_deref()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| PortError::new("worktrees", "the run records no worktree"))
}

/// The two decisions provisioning refuses to make for itself, as the daemon
/// answers them.
///
/// Hooks are allowed because a repository declares its contract in its own
/// tree, and there is nobody at a terminal for a daemon to ask. A failure is
/// continued past rather than aborted on, because a worktree thrown away over
/// one hook costs the run everything and tells the worker nothing: the failure
/// travels to it instead.
struct CoordinatorProvisioning;

impl ProvisioningDecisions for CoordinatorProvisioning {
    fn authorize(&self, _request: &HookAuthorizationRequest<'_>) -> HookAuthorization {
        HookAuthorization::Allow
    }

    fn on_hook_failure(&self, _failure: &HookFailure) -> HookFailureResponse {
        HookFailureResponse::Continue
    }
}

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
use agens_tools::SessionWorktrees;

use crate::api::{PortError, WorktreeDerivation, WorktreeGate};
use crate::gates::receipt_of;

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

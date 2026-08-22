//! Creating a run: the one operation that brings a run into existence.
//!
//! Every other Team operation moves a row that already exists. This one is
//! where work enters the control plane at all, which is why it does two things
//! nothing else does: it derives the repository's identity, and it provisions
//! the worktree the run will work in.
//!
//! **A run is created in `draft` and nowhere else.** Approving is what freezes
//! the scope, it is the user's alone, and the run machine has exactly one edge
//! into `queued`. A creation that landed a run straight in the queue would be a
//! second door to that state, and a second door past a guard is the shape this
//! design refuses everywhere else.

use std::path::PathBuf;

use agens_store::{EventClass, EventRow, RunRow, RunState, WorktreeStatus};

use super::{ApiCore, ApiError, Operation};
use crate::api::ports::WorktreeRequest;
use crate::fsm::Principal;

/// How many characters of a task's own words the worktree directory carries.
///
/// Long enough to recognize at a `cd`, short enough that the path stays one
/// terminal line beside the repository segment and the data directory.
const SLUG_CHARS: usize = 32;

/// A proposed execution, and the repository it is proposed against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateRun {
    /// The checkout the run's worktree is created from.
    pub repo_root: PathBuf,
    /// What the run is for, in the words of whoever asked for it.
    pub task: String,
    /// The declared scope, frozen when the user approves.
    pub scope: String,
    /// The definition of done, frozen with it.
    pub dod: String,
    /// Where an imported task came from, as provenance. Never followed back.
    pub external_ref: Option<String>,
    /// The run this one replans, when it replans one.
    pub parent_run_id: Option<i64>,
    /// The run whose work has to land before this one is eligible.
    pub dep_run_id: Option<i64>,
    pub provider: String,
    pub priority: i64,
    pub budget_tokens: Option<i64>,
    /// The commit the run's branch starts from.
    pub start_point: String,
    pub now: i64,
}

/// The run that was created, and where its work will live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedRun {
    pub run_id: i64,
    pub repo_id: String,
    pub worktree_path: PathBuf,
    /// Provisioning hooks that failed and were continued past. The run starts
    /// in an environment that is not what the repository declared, and the
    /// caller is the party that can say so.
    pub hook_failures: Vec<String>,
}

impl ApiCore {
    /// Creates a proposed execution and provisions the worktree it will run in.
    pub fn create_run(
        &mut self,
        principal: Principal,
        request: &CreateRun,
    ) -> Result<CreatedRun, ApiError> {
        self.authorize(Operation::CreateRun, principal, None, request.now)?;
        self.check_describable(principal, request)?;

        let identity = self.ports.worktrees.identify(&request.repo_root)?;
        let name = worktree_name(&request.task, &identity.repo_id, request.now);
        let branch = format!("agens/{name}");

        let provisioned = self.ports.worktrees.provision(&WorktreeRequest {
            repository: &request.repo_root,
            repo_id: &identity.repo_id,
            name: &name,
            branch: &branch,
            start_point: &request.start_point,
        })?;

        let row = RunRow {
            id: None,
            repo_id: identity.repo_id.clone(),
            repo_root: request.repo_root.display().to_string(),
            remote_url: identity.remote_url.clone(),
            external_ref: request.external_ref.clone(),
            parent_run_id: request.parent_run_id,
            task: request.task.clone(),
            scope: request.scope.clone(),
            dod: request.dod.clone(),
            genesis_paths: None,
            state: RunState::Draft,
            priority: request.priority,
            dep_run_id: request.dep_run_id,
            provider: request.provider.clone(),
            budget_tokens: request.budget_tokens,
            worktree_path: Some(provisioned.path.display().to_string()),
            worktree_status: Some(WorktreeStatus::Active),
            created_at: request.now,
            result: None,
        };

        let run_id = match self.machines.open_run(&row) {
            Ok(run_id) => run_id,
            Err(error) => {
                // The worktree is on disk and no row names it, so nothing else
                // will ever find it to clean up. Here is the only moment that
                // knowledge exists.
                let _ = self.ports.worktrees.remove(&row);

                return Err(ApiError::Storage(error.to_string()));
            }
        };

        self.journal_creation(run_id, &branch, &provisioned.hook_failures, request.now);

        Ok(CreatedRun {
            run_id,
            repo_id: identity.repo_id,
            worktree_path: provisioned.path,
            hook_failures: provisioned.hook_failures,
        })
    }

    /// Refuses a run nothing could measure: a scope and a definition of done
    /// are what the divergence detector and the gate compare against, and an
    /// empty one of either is a run that can never be shown to be off track.
    fn check_describable(
        &mut self,
        principal: Principal,
        request: &CreateRun,
    ) -> Result<(), ApiError> {
        let missing = [
            ("a task", request.task.trim().is_empty()),
            ("a scope", request.scope.trim().is_empty()),
            ("a definition of done", request.dod.trim().is_empty()),
            ("a provider", request.provider.trim().is_empty()),
        ]
        .into_iter()
        .filter_map(|(field, empty)| empty.then_some(field))
        .collect::<Vec<_>>();

        if missing.is_empty() {
            return Ok(());
        }

        Err(self.refuse(
            Operation::CreateRun,
            principal,
            None,
            request.now,
            format!("a run needs {}", missing.join(", ")),
        ))
    }

    /// The journal entry that says the run exists, carrying what a reader
    /// cannot recover from the row: which branch the worktree is on, and
    /// whether the environment it starts in is the declared one.
    fn journal_creation(&mut self, run_id: i64, branch: &str, hook_failures: &[String], now: i64) {
        let payload = serde_json::json!({
            "branch": branch,
            "hook_failures": hook_failures,
        });

        // A run whose creation is missing from the journal is still created:
        // the caller holds its id and the row is readable. Undoing the run over
        // the gap would cost more than the gap does.
        let _ = self.machines.journal(&[EventRow {
            id: None,
            run_id: Some(run_id),
            event_type: "run_created".to_owned(),
            class: EventClass::Infra,
            payload: payload.to_string(),
            ts: now,
        }]);
    }
}

/// A directory name that a person reads and two runs never share.
///
/// The words come from the task so the path means something at a `cd`; the
/// digest is what makes it unique, because two runs of the same task in the
/// same repository are ordinary.
fn worktree_name(task: &str, repo_id: &str, now: i64) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(format!("{repo_id}\u{1f}{task}\u{1f}{now}").as_bytes());
    let suffix = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let slug = slug(task);

    if slug.is_empty() {
        format!("run-{suffix}")
    } else {
        format!("{slug}-{suffix}")
    }
}

/// The task's own words, reduced to what a single path component may hold.
fn slug(task: &str) -> String {
    let mut slug = String::new();

    for character in task.chars() {
        if slug.chars().count() >= SLUG_CHARS {
            break;
        }

        if character.is_ascii_alphanumeric() {
            slug.extend(character.to_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }

    slug.trim_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worktree_name_is_readable_and_unique() {
        let first = worktree_name("Wire the daemon to the core", "a1b2c3d4", 10);
        let second = worktree_name("Wire the daemon to the core", "a1b2c3d4", 11);

        assert!(
            first.starts_with("wire-the-daemon-to-the-core-"),
            "the task's own words survive: {first}"
        );
        assert_ne!(
            first, second,
            "two runs of the same task do not share a directory"
        );
    }

    #[test]
    fn a_task_with_no_usable_words_still_names_a_directory() {
        assert!(
            worktree_name("!?", "a1b2c3d4", 10).starts_with("run-"),
            "a name is always a valid single path component"
        );
    }
}

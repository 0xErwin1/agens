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
//!
//! Creation is three steps rather than one because the middle one is slow.
//! [`ApiCore::prepare_run`] decides — who is asking, whether the daemon serves
//! this checkout, and whether the repository's hooks may run; provisioning
//! copies files and executes those hooks, which the repository bounds and the
//! daemon does not; [`ApiCore::open_run`] writes the row. The daemon holds the
//! core's lock across the first and the third and releases it for the second,
//! because a hook is allowed to take minutes and the admission loop, the timer
//! wheel and every other RPC wait behind that same lock.

use std::path::PathBuf;

use agens_store::{
    EventClass, EventRow, QuestionKind, QuestionRow, QuestionState, RunRow, RunState,
    WorktreeStatus,
};

use super::{ApiCore, ApiError, Operation};
use crate::api::ports::{HookPolicy, ProvisionedWorktree, WorktreeRequest};
use crate::fsm::{Principal, RunFacts, RunTrigger, WorktreeFacts, WorktreeTrigger};
use crate::policy::{HookTrust, PendingHookTrust, TrustReadFailure};

/// How many characters of a task's own words the worktree directory carries.
///
/// Long enough to recognize at a `cd`, short enough that the path stays one
/// terminal line beside the repository segment and the data directory.
const SLUG_CHARS: usize = 32;

/// The journal entry an unreadable hook-trust register is recorded as.
const HOOK_TRUST_UNREADABLE_EVENT: &str = "hook_trust_unreadable";

/// What the operator is asked when a repository's hooks have never been
/// decided on.
const HOOK_TRUST_DECISION: &str =
    "whether this repository's provisioning hooks may run with the daemon's environment";

/// The one answer that authorizes them. Anything else refuses, because an
/// answer that was meant to grant and was not understood has to fail towards
/// not executing the repository's code.
pub(crate) const HOOK_TRUST_GRANT: &str = "trust";

/// The answer that refuses them, offered so the question has a closed set.
pub(crate) const HOOK_TRUST_REFUSE: &str = "refuse";

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

/// Everything the decision step settled, and the provisioning step needs.
///
/// It carries the canonical repository rather than the requested one: what the
/// daemon admitted is the path it resolved, and provisioning has no business
/// resolving it a second time and possibly differently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedRun {
    pub repository: PathBuf,
    pub repo_id: String,
    pub remote_url: Option<String>,
    pub name: String,
    pub branch: String,
    pub hooks: HookPolicy,
}

impl PreparedRun {
    /// The worktree this preparation asks for.
    #[must_use]
    pub fn worktree_request<'a>(&'a self, start_point: &'a str) -> WorktreeRequest<'a> {
        WorktreeRequest {
            repository: &self.repository,
            repo_id: &self.repo_id,
            name: &self.name,
            branch: &self.branch,
            start_point,
            hooks: self.hooks,
        }
    }
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
    /// Whether the repository's declared hooks ran. False when it declared
    /// hooks the operator has not authorized, which is not a failure of the
    /// run but is a fact about the environment it starts in.
    pub hooks_ran: bool,
    /// The durable question asking the operator to decide about this
    /// repository's hooks, opened when nothing had been decided yet.
    pub hook_authorization_question: Option<i64>,
}

impl ApiCore {
    /// Creates a proposed execution and provisions the worktree it will run in.
    ///
    /// Provisioning happens inline here, so this is the path for a caller that
    /// holds no lock across the call. The daemon's facade does not use it: it
    /// drives [`Self::prepare_run`], the worktree port and [`Self::open_run`]
    /// itself, releasing the core between them.
    pub fn create_run(
        &mut self,
        principal: Principal,
        request: &CreateRun,
    ) -> Result<CreatedRun, ApiError> {
        let prepared = self.prepare_run(principal, request)?;
        let worktrees = std::sync::Arc::clone(&self.ports.worktrees);

        let provisioned = worktrees.provision(&prepared.worktree_request(&request.start_point))?;

        self.open_run(request, &prepared, provisioned)
    }

    /// Decides everything a run's creation turns on, and touches no disk.
    ///
    /// Three refusals live here rather than further down: a principal the
    /// operation does not admit, a request nothing could be measured against,
    /// and a checkout this daemon does not serve. The last one is why the
    /// repository is canonicalized before anything else looks at it — a path
    /// arrives from a client, and a client that could name any path could have
    /// the daemon execute any repository's hooks.
    pub fn prepare_run(
        &mut self,
        principal: Principal,
        request: &CreateRun,
    ) -> Result<PreparedRun, ApiError> {
        self.authorize(Operation::CreateRun, principal, None, request.now)?;
        self.check_describable(principal, request)?;

        let repository = self.admitted_repository(principal, request)?;
        let identity = self.ports.worktrees.identify(&repository)?;
        let name = worktree_name(&request.task, &identity.repo_id, request.now);
        let branch = format!("agens/{name}");
        let hooks = self.hook_policy(principal, &identity.repo_id, request.now);

        Ok(PreparedRun {
            repository,
            repo_id: identity.repo_id,
            remote_url: identity.remote_url,
            name,
            branch,
            hooks,
        })
    }

    /// Opens the run's row over a worktree that already exists.
    ///
    /// The principal was checked in [`Self::prepare_run`] and is not checked
    /// again: the two halves are one operation, and re-deriving authority from
    /// a preparation the caller holds would let a caller that kept one around
    /// replay it.
    pub fn open_run(
        &mut self,
        request: &CreateRun,
        prepared: &PreparedRun,
        provisioned: ProvisionedWorktree,
    ) -> Result<CreatedRun, ApiError> {
        let row = RunRow {
            id: None,
            repo_id: prepared.repo_id.clone(),
            repo_root: prepared.repository.display().to_string(),
            remote_url: prepared.remote_url.clone(),
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

        let mut row = row;
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

        row.id = Some(run_id);

        let question = match self.ask_about_hooks(run_id, prepared, &provisioned, request.now) {
            Ok(question) => question,
            Err(error) => {
                // The row exists and the worktree is on disk, and the caller is
                // about to be told the run was never created. Rolling both back
                // here is the only moment either can still be reached: a draft
                // whose creation failed is a run nothing will ever approve, and
                // its worktree is a directory no row would name.
                self.discard_created_run(run_id, &row, request.now);

                return Err(error);
            }
        };

        self.journal_creation(run_id, prepared, &provisioned, question, request.now);

        Ok(CreatedRun {
            run_id,
            repo_id: prepared.repo_id.clone(),
            worktree_path: provisioned.path,
            hook_failures: provisioned.hook_failures,
            hooks_ran: provisioned.hooks_ran,
            hook_authorization_question: question,
        })
    }

    /// Undoes a creation that failed after the run's row was written.
    ///
    /// Best effort by construction: it is already unwinding one failure, and a
    /// second one has nowhere to be reported to. The order matters more than
    /// the outcome — the directory goes first, so a rollback that only got
    /// halfway leaves a cancelled run naming a worktree the sweep can reclaim
    /// rather than a live draft holding one.
    ///
    /// The worktree is let go before the run is cancelled. `Discard` carries no
    /// effects, so a rollback that stopped after it left a `cancelled` row with
    /// `worktree_status = active` naming a directory that no longer exists:
    /// counted against the ceiling for the life of the daemon, and reported as
    /// a missing worktree on every boot.
    fn discard_created_run(&mut self, run_id: i64, row: &RunRow, now: i64) {
        let _ = self.ports.worktrees.remove(row);
        let _ = self.machines.apply_worktree(
            run_id,
            WorktreeTrigger::ManualDisposition,
            &WorktreeFacts {
                now,
                manual_disposition_confirmed: true,
                ..WorktreeFacts::default()
            },
        );
        let _ = self.machines.apply_run(
            run_id,
            RunTrigger::Discard,
            &RunFacts {
                now,
                principal: Principal::Coordinator,
                ..RunFacts::default()
            },
        );
    }

    /// The checkout the request named, resolved and checked against the roots
    /// the operator serves.
    fn admitted_repository(
        &mut self,
        principal: Principal,
        request: &CreateRun,
    ) -> Result<PathBuf, ApiError> {
        let canonical = request.repo_root.canonicalize().ok();

        let Some(repository) = canonical.filter(|path| self.policy.admits(path)) else {
            let remedy = self.policy.admission_remedy();

            return Err(self.refuse(
                Operation::CreateRun,
                principal,
                None,
                request.now,
                format!(
                    "the daemon does not serve {}: {remedy}",
                    request.repo_root.display()
                ),
            ));
        };

        Ok(repository)
    }

    /// Whether this run's provisioning hooks may run.
    ///
    /// Praetor never reaches [`HookPolicy::Allow`], whatever the policy says.
    /// A hook is repository code executed with the daemon's environment, and a
    /// run proposed by the manager is exactly the path where nobody looked at
    /// the repository first.
    fn hook_policy(&mut self, principal: Principal, repo_id: &str, now: i64) -> HookPolicy {
        if principal == Principal::Praetor {
            return HookPolicy::Deny;
        }

        match self.policy.hook_trust(repo_id) {
            HookTrust::Granted => HookPolicy::Allow,
            HookTrust::Refused => HookPolicy::Deny,
            HookTrust::Unknown => HookPolicy::Ask,
            HookTrust::Unreadable(failure) => {
                self.journal_unreadable_trust(repo_id, failure, now);

                HookPolicy::Deny
            }
        }
    }

    /// Records that the hook-trust register could not be read.
    ///
    /// The refusal it produces is indistinguishable from an operator saying no,
    /// and it lasts as long as the register stays unreadable: every repository
    /// this daemon serves becomes permanently untrusted, with nothing anywhere
    /// saying why. The entry is that record, and it hangs off no run because a
    /// register the daemon cannot read is not a fact about one.
    fn journal_unreadable_trust(&mut self, repo_id: &str, failure: TrustReadFailure, now: i64) {
        let _ = self.machines.journal(&[EventRow {
            id: None,
            run_id: None,
            event_type: HOOK_TRUST_UNREADABLE_EVENT.to_owned(),
            class: EventClass::Infra,
            payload: serde_json::json!({
                "repo_id": repo_id,
                "reason": failure.as_str(),
            })
            .to_string(),
            ts: now,
        }]);
    }

    /// Opens the durable question a repository whose hooks nobody has decided
    /// on has earned, and records what answering it will grant.
    ///
    /// It is durable rather than a prompt because there is nobody at a terminal
    /// when a daemon provisions: the request that started this has already been
    /// answered by the time an operator reads anything. The run itself is not
    /// held up — it exists, with its hooks unrun and said so — and the answer
    /// decides the repository's next run.
    fn ask_about_hooks(
        &mut self,
        run_id: i64,
        prepared: &PreparedRun,
        provisioned: &ProvisionedWorktree,
        now: i64,
    ) -> Result<Option<i64>, ApiError> {
        if prepared.hooks != HookPolicy::Ask || provisioned.declared_hooks.is_empty() {
            return Ok(None);
        }

        let question_id = self.machines.open_question(
            &QuestionRow {
                id: None,
                run_id,
                kind: QuestionKind::Question,
                blocked_decision: HOOK_TRUST_DECISION.to_owned(),
                options: serde_json::json!([HOOK_TRUST_GRANT, HOOK_TRUST_REFUSE]).to_string(),
                recommendation: Some(hook_recommendation(prepared, provisioned)),
                answer: None,
                author: None,
                expires_at: None,
                tree_hash: None,
                paths_digest: None,
                state: QuestionState::Open,
                created_at: now,
            },
            &[],
        )?;

        self.policy.record_pending(&PendingHookTrust {
            question_id,
            repo_id: prepared.repo_id.clone(),
            repository: prepared.repository.clone(),
            asked_at: now,
        })?;

        Ok(Some(question_id))
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
    fn journal_creation(
        &mut self,
        run_id: i64,
        prepared: &PreparedRun,
        provisioned: &ProvisionedWorktree,
        question: Option<i64>,
        now: i64,
    ) {
        let payload = serde_json::json!({
            "branch": prepared.branch,
            "hook_failures": provisioned.hook_failures,
            "hooks_ran": provisioned.hooks_ran,
            "declared_hooks": provisioned.declared_hooks,
            "hook_authorization_question": question,
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

/// What the operator is shown beside the question: the exact commands, and the
/// inheritance that makes them worth asking about.
fn hook_recommendation(prepared: &PreparedRun, provisioned: &ProvisionedWorktree) -> String {
    format!(
        "{} declares {}, which run with the daemon's whole environment, provider credentials \
         included: {}",
        prepared.repository.display(),
        plural_hooks(provisioned.declared_hooks.len()),
        provisioned.declared_hooks.join(", ")
    )
}

fn plural_hooks(count: usize) -> String {
    if count == 1 {
        "one provisioning hook".to_owned()
    } else {
        format!("{count} provisioning hooks")
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

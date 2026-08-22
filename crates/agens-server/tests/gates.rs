//! The two coordinator gates, driven against real git repositories.
//!
//! Every rule the gates enforce is re-derived from git at the moment it runs,
//! so there is no useful way to test them against a fake: a stub would let a
//! stored flag stand in for the derivation, which is the exact failure the
//! gates exist to prevent. Each test therefore builds a repository, a session
//! worktree and a control plane, and moves the real thing.

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_server::{
    GateRefusal, Gates, MergePath, PreMergeRequest, PreMergeVerdict, Receipt, ReclaimRequest,
    ReclaimVerdict, StateMachines, SubAgentKind, freeze_receipt,
};
use agens_store::{
    AttemptOutcome, AttemptRow, ControlPlaneStore, QuestionAuthor, QuestionKind, QuestionRow,
    QuestionState, RunRow, RunState, WorktreeStatus,
};
use agens_tools::SessionWorktrees;

const NOW: i64 = 1_700_000_500;
const REPOSITORY_ID: &str = "agens-a1b2c3d4";
const WORKTREE_NAME: &str = "agn-60";
const BRANCH: &str = "feature/agn-60";

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

/// A repository and one session worktree on a branch of it.
struct Fixture {
    root: PathBuf,
    checkout: PathBuf,
    worktree: PathBuf,
    data_directory: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let suffix = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "agens-server-gates-{}-{suffix}",
            std::process::id()
        ));
        let checkout = root.join("repository");
        let data_directory = root.join("data");

        std::fs::create_dir_all(&checkout).expect("create repository directory");
        std::fs::create_dir_all(&data_directory).expect("create data directory");
        git(&checkout, &["init", "--quiet", "--initial-branch=main"]);
        git(&checkout, &["config", "user.name", "Agens Test"]);
        git(&checkout, &["config", "user.email", "agens-test@localhost"]);
        write(&checkout, "tracked.txt", "initial\n");
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "--quiet", "-m", "initial"]);

        let worktree = SessionWorktrees::new(&data_directory)
            .create(&checkout, REPOSITORY_ID, WORKTREE_NAME, BRANCH, "main")
            .expect("create session worktree");

        Self {
            root,
            checkout,
            worktree,
            data_directory,
        }
    }

    /// Commits `contents` at `path` on the worktree's branch.
    fn commit(&self, path: &str, contents: &str) {
        write(&self.worktree, path, contents);
        git(&self.worktree, &["add", "."]);
        git(&self.worktree, &["commit", "--quiet", "-m", path]);
    }

    fn worktrees(&self) -> SessionWorktrees {
        SessionWorktrees::new(&self.data_directory)
    }

    fn store(&self) -> ControlPlaneStore {
        ControlPlaneStore::open(&self.data_directory).expect("open control plane")
    }

    /// The receipt an approval created right now would be bound to.
    fn receipt(&self) -> Receipt {
        freeze_receipt(&self.worktrees(), &self.worktree, "main").expect("freeze receipt")
    }

    /// A run whose worktree is this fixture's, active and ready to be gated.
    fn run(&self, genesis_paths: Option<&str>) -> RunRow {
        RunRow {
            id: None,
            repo_id: "a1b2c3d4e5f60718".to_owned(),
            repo_root: self.checkout.to_string_lossy().into_owned(),
            remote_url: None,
            external_ref: Some("agens/AGN-60".to_owned()),
            parent_run_id: None,
            task: "gates".to_owned(),
            scope: "crates/agens-server/src/gates".to_owned(),
            dod: "pre-merge and reclaim re-derive from git".to_owned(),
            genesis_paths: genesis_paths.map(ToOwned::to_owned),
            state: RunState::Done,
            priority: 5,
            dep_run_id: None,
            provider: "anthropic".to_owned(),
            budget_tokens: None,
            worktree_path: Some(self.worktree.to_string_lossy().into_owned()),
            worktree_status: Some(WorktreeStatus::Active),
            created_at: 1_700_000_000,
            result: None,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A fixture with one commit on its branch, a run, and a live user approval
/// bound to the bytes that commit produced.
struct Gated {
    fixture: Fixture,
    gates: Gates,
    run_id: i64,
    approval_id: i64,
}

impl Gated {
    fn new(genesis_paths: Option<&str>) -> Self {
        Self::with_approval(genesis_paths, QuestionState::Answered, Some(NOW + 600))
    }

    fn with_approval(
        genesis_paths: Option<&str>,
        state: QuestionState,
        expires_at: Option<i64>,
    ) -> Self {
        let fixture = Fixture::new();
        fixture.commit("feature.txt", "work\n");

        let mut store = fixture.store();
        let run_id = store
            .insert_run(&fixture.run(genesis_paths))
            .expect("insert run");
        let approval_id = store
            .insert_question(&approval(run_id, &fixture.receipt(), state, expires_at))
            .expect("insert approval");

        Self {
            gates: Gates::new(StateMachines::new(store), fixture.worktrees()),
            fixture,
            run_id,
            approval_id,
        }
    }

    fn request(&self, path: MergePath) -> PreMergeRequest {
        PreMergeRequest {
            run_id: self.run_id,
            approval_id: self.approval_id,
            path,
            main_ref: "main".to_owned(),
            attempt_cap: 3,
            now: NOW,
        }
    }

    fn pre_merge(&mut self, path: MergePath) -> PreMergeVerdict {
        let request = self.request(path);

        self.gates.pre_merge(&request).expect("gate runs")
    }

    fn reclaim(&mut self) -> ReclaimVerdict {
        self.gates
            .reclaim(&ReclaimRequest {
                run_id: self.run_id,
                main_ref: "main".to_owned(),
                now: NOW,
            })
            .expect("reclaim runs")
    }

    fn events(&self) -> Vec<String> {
        self.gates
            .machines()
            .store()
            .events_for_run(self.run_id)
            .expect("read events")
            .into_iter()
            .map(|event| event.event_type)
            .collect()
    }

    fn worktree_status(&self) -> Option<WorktreeStatus> {
        self.gates
            .machines()
            .store()
            .load_run(self.run_id)
            .expect("load run")
            .expect("the run exists")
            .worktree_status
    }
}

fn approval(
    run_id: i64,
    receipt: &Receipt,
    state: QuestionState,
    expires_at: Option<i64>,
) -> QuestionRow {
    let answered = state != QuestionState::Open;

    QuestionRow {
        id: None,
        run_id,
        kind: QuestionKind::Approval,
        blocked_decision: "merge the branch".to_owned(),
        options: "[\"yes\",\"no\"]".to_owned(),
        recommendation: None,
        answer: answered.then(|| "yes".to_owned()),
        author: answered.then_some(QuestionAuthor::User),
        expires_at,
        tree_hash: Some(receipt.tree_hash.clone()),
        paths_digest: Some(receipt.paths_digest.clone()),
        state,
        created_at: 1_700_000_100,
    }
}

fn attempt(run_id: i64, n: i64, outcome: AttemptOutcome) -> AttemptRow {
    AttemptRow {
        id: None,
        run_id,
        n,
        session_id: None,
        session_attempt_id: None,
        started_at: 1_700_000_200,
        ended_at: Some(1_700_000_300),
        outcome: Some(outcome),
        retry_trigger: None,
        tokens: None,
        cost_micros: None,
    }
}

fn write(directory: &Path, name: &str, contents: &str) {
    std::fs::write(directory.join(name), contents).expect("write file");
}

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");

    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout).expect("git output is UTF-8")
}

fn refusal(verdict: PreMergeVerdict) -> GateRefusal {
    match verdict {
        PreMergeVerdict::Refused(refusal) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_clean_merge_journals_its_verdict_before_the_worktree_is_released() {
    let mut gated = Gated::new(None);

    let verdict = gated.pre_merge(MergePath::Integrate);

    let PreMergeVerdict::Merged { commit, worktree } = verdict else {
        panic!("expected the merge to land, got {verdict:?}");
    };
    assert!(commit.is_some(), "an executed merge reports its commit");
    assert_eq!(
        worktree.expect("the worktree moved").to,
        WorktreeStatus::Reclaimable
    );
    assert_eq!(
        gated.events(),
        [
            "gate_result",
            "merged",
            "run_state_changed",
            "worktree_reclaimable"
        ],
        "the verdict and the merge are journaled before the worktree is released"
    );
    assert_eq!(
        git(&gated.fixture.checkout, &["log", "--oneline", "main"])
            .lines()
            .count(),
        3,
        "main carries the branch's commit and the merge commit"
    );
}

#[test]
fn an_approval_bound_to_an_older_tree_refuses_the_merge() {
    let mut gated = Gated::new(None);
    gated.fixture.commit("afterthought.txt", "one more\n");

    let verdict = gated.pre_merge(MergePath::Integrate);

    let GateRefusal::ReceiptStale { frozen, derived } = refusal(verdict) else {
        panic!("a commit after the approval makes its receipt stale");
    };
    assert_ne!(frozen.tree_hash, derived.tree_hash);
    assert_ne!(frozen.paths_digest, derived.paths_digest);
    assert_eq!(
        gated.events(),
        ["gate_result"],
        "a refusal is journaled too"
    );
    assert_eq!(gated.worktree_status(), Some(WorktreeStatus::Active));
}

#[test]
fn a_commit_that_only_moves_bytes_within_the_same_paths_still_refuses() {
    let mut gated = Gated::new(None);
    gated.fixture.commit("feature.txt", "work, revised\n");

    let GateRefusal::ReceiptStale { frozen, derived } =
        refusal(gated.pre_merge(MergePath::Integrate))
    else {
        panic!("the tree hash moves even when the path set does not");
    };
    assert_ne!(frozen.tree_hash, derived.tree_hash);
    assert_eq!(
        frozen.paths_digest, derived.paths_digest,
        "the same paths digest the same, which is why the tree hash is checked too"
    );
}

#[test]
fn an_expired_approval_authorizes_nothing() {
    let mut gated = Gated::with_approval(None, QuestionState::Answered, Some(NOW - 1));

    assert_eq!(
        refusal(gated.pre_merge(MergePath::Integrate)),
        GateRefusal::ApprovalExpired {
            expired_at: NOW - 1
        }
    );
}

#[test]
fn an_unanswered_approval_authorizes_nothing() {
    let mut gated = Gated::with_approval(None, QuestionState::Open, None);

    assert_eq!(
        refusal(gated.pre_merge(MergePath::Integrate)),
        GateRefusal::NotAuthorized { state: "open" }
    );
}

#[test]
fn a_consumed_approval_is_not_presented_twice() {
    let mut gated = Gated::with_approval(None, QuestionState::Delivered, None);

    assert_eq!(
        refusal(gated.pre_merge(MergePath::Integrate)),
        GateRefusal::NotAuthorized { state: "delivered" }
    );
}

#[test]
fn an_approval_belonging_to_another_run_is_not_an_approval_for_this_one() {
    let mut gated = Gated::new(None);
    let request = PreMergeRequest {
        approval_id: gated.approval_id + 1,
        ..gated.request(MergePath::Integrate)
    };

    let verdict = gated.gates.pre_merge(&request).expect("gate runs");

    assert_eq!(refusal(verdict), GateRefusal::ApprovalMissing);
}

#[test]
fn a_dirty_worktree_refuses_the_merge() {
    let mut gated = Gated::new(None);
    write(&gated.fixture.worktree, "scratch.txt", "uncommitted\n");

    assert_eq!(
        refusal(gated.pre_merge(MergePath::Integrate)),
        GateRefusal::WorktreeDirty
    );
    assert_eq!(gated.worktree_status(), Some(WorktreeStatus::Active));
}

#[test]
fn a_detached_head_has_no_branch_to_merge() {
    let mut gated = Gated::new(None);
    git(
        &gated.fixture.worktree,
        &["checkout", "--quiet", "--detach"],
    );

    assert_eq!(
        refusal(gated.pre_merge(MergePath::Integrate)),
        GateRefusal::DetachedHead
    );
}

#[test]
fn an_unrelated_history_has_no_merge_base() {
    let mut gated = Gated::new(None);
    git(
        &gated.fixture.worktree,
        &["checkout", "--quiet", "--orphan", "orphan/agn-60"],
    );
    git(&gated.fixture.worktree, &["rm", "-rq", "--cached", "."]);
    std::fs::remove_file(gated.fixture.worktree.join("tracked.txt")).expect("remove tracked file");
    std::fs::remove_file(gated.fixture.worktree.join("feature.txt")).expect("remove feature file");
    gated.fixture.commit("orphan.txt", "unrelated\n");

    assert_eq!(
        refusal(gated.pre_merge(MergePath::Integrate)),
        GateRefusal::UnrelatedHistory
    );
}

#[test]
fn attempts_past_the_cap_refuse_the_merge() {
    let mut gated = Gated::new(None);
    let mut store = gated.fixture.store();
    for n in 1..=4 {
        store
            .insert_attempt(&attempt(gated.run_id, n, AttemptOutcome::Failed))
            .expect("insert attempt");
    }
    drop(store);

    assert_eq!(
        refusal(gated.pre_merge(MergePath::Integrate)),
        GateRefusal::AttemptsExhausted { charged: 4, cap: 3 }
    );
}

#[test]
fn an_interrupted_attempt_does_not_count_against_the_cap() {
    let mut gated = Gated::new(None);
    let mut store = gated.fixture.store();
    for (n, outcome) in [
        (1, AttemptOutcome::Failed),
        (2, AttemptOutcome::Interrupted),
        (3, AttemptOutcome::Interrupted),
        (4, AttemptOutcome::Succeeded),
    ] {
        store
            .insert_attempt(&attempt(gated.run_id, n, outcome))
            .expect("insert attempt");
    }
    drop(store);

    assert!(
        matches!(
            gated.pre_merge(MergePath::Integrate),
            PreMergeVerdict::Merged { .. }
        ),
        "two parked attempts are infrastructure waits, not spent budget"
    );
}

#[test]
fn a_diff_reaching_outside_the_frozen_genesis_paths_refuses_the_merge() {
    let mut gated = Gated::new(Some("[\"docs\"]"));

    assert_eq!(
        refusal(gated.pre_merge(MergePath::Integrate)),
        GateRefusal::OutsideGenesisPaths {
            paths: vec!["feature.txt".to_owned()]
        }
    );
}

#[test]
fn a_genesis_directory_covers_the_files_under_it() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.worktree.join("crates/agens-server")).expect("create scope");
    fixture.commit("crates/agens-server/gates.rs", "// gates\n");

    let mut store = fixture.store();
    let run_id = store
        .insert_run(&fixture.run(Some("[\"crates/agens-server\"]")))
        .expect("insert run");
    let approval_id = store
        .insert_question(&approval(
            run_id,
            &fixture.receipt(),
            QuestionState::Answered,
            None,
        ))
        .expect("insert approval");
    let mut gates = Gates::new(StateMachines::new(store), fixture.worktrees());

    let verdict = gates
        .pre_merge(&PreMergeRequest {
            run_id,
            approval_id,
            path: MergePath::Integrate,
            main_ref: "main".to_owned(),
            attempt_cap: 3,
            now: NOW,
        })
        .expect("gate runs");

    assert!(
        matches!(verdict, PreMergeVerdict::Merged { .. }),
        "a genesis directory covers what is under it, got {verdict:?}"
    );
}

#[test]
fn a_genesis_path_never_covers_the_directory_beside_it() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.worktree.join("crates/agens-server-extra"))
        .expect("create scope");
    fixture.commit("crates/agens-server-extra/gates.rs", "// beside\n");

    let mut store = fixture.store();
    let run_id = store
        .insert_run(&fixture.run(Some("[\"crates/agens-server\"]")))
        .expect("insert run");
    let approval_id = store
        .insert_question(&approval(
            run_id,
            &fixture.receipt(),
            QuestionState::Answered,
            None,
        ))
        .expect("insert approval");
    let mut gates = Gates::new(StateMachines::new(store), fixture.worktrees());

    let verdict = gates
        .pre_merge(&PreMergeRequest {
            run_id,
            approval_id,
            path: MergePath::Integrate,
            main_ref: "main".to_owned(),
            attempt_cap: 3,
            now: NOW,
        })
        .expect("gate runs");

    assert_eq!(
        refusal(verdict),
        GateRefusal::OutsideGenesisPaths {
            paths: vec!["crates/agens-server-extra/gates.rs".to_owned()]
        },
        "the prefix is compared at a component boundary"
    );
}

#[test]
fn a_merge_that_does_not_apply_asks_for_an_integration_sub_agent() {
    let mut gated = Gated::new(None);
    write(&gated.fixture.checkout, "feature.txt", "conflicting\n");
    git(&gated.fixture.checkout, &["add", "."]);
    git(
        &gated.fixture.checkout,
        &["commit", "--quiet", "-m", "conflicting"],
    );

    let verdict = gated.pre_merge(MergePath::Integrate);

    let PreMergeVerdict::IntegrationRequired(request) = verdict else {
        panic!("a conflicting merge asks for a sub-agent, got {verdict:?}");
    };
    assert_eq!(request.kind, SubAgentKind::Integration);
    assert_eq!(request.branch.as_deref(), Some(BRANCH));
    assert_eq!(
        git(&gated.fixture.checkout, &["status", "--porcelain=v1"]),
        "",
        "the aborted merge leaves the checkout as it was"
    );
    assert_eq!(gated.worktree_status(), Some(WorktreeStatus::Active));
    assert_eq!(gated.events(), ["gate_result"]);
}

#[test]
fn an_attestation_is_verified_against_git_and_never_merges() {
    let mut gated = Gated::new(None);
    git(&gated.fixture.checkout, &["merge", "--quiet", BRANCH]);

    let verdict = gated.pre_merge(MergePath::Attested);

    let PreMergeVerdict::Merged { commit, worktree } = verdict else {
        panic!("the attestation holds, got {verdict:?}");
    };
    assert_eq!(commit, None, "the coordinator executed no merge of its own");
    assert_eq!(
        worktree.expect("the worktree moved").to,
        WorktreeStatus::Reclaimable
    );
    assert_eq!(
        gated.events(),
        [
            "gate_result",
            "merged",
            "run_state_changed",
            "worktree_reclaimable"
        ]
    );
}

#[test]
fn an_attestation_over_a_tree_that_moved_after_the_approval_is_refused() {
    let mut gated = Gated::new(None);
    gated.fixture.commit("afterthought.txt", "one more\n");
    git(&gated.fixture.checkout, &["merge", "--quiet", BRANCH]);

    let GateRefusal::ReceiptStale { frozen, derived } =
        refusal(gated.pre_merge(MergePath::Attested))
    else {
        panic!("landing the work first does not make a stale receipt current");
    };
    assert_ne!(frozen.tree_hash, derived.tree_hash);
    assert_eq!(gated.worktree_status(), Some(WorktreeStatus::Active));
}

#[test]
fn an_attestation_that_git_contradicts_is_refused() {
    let mut gated = Gated::new(None);

    assert_eq!(
        refusal(gated.pre_merge(MergePath::Attested)),
        GateRefusal::NotMerged
    );
    assert_eq!(gated.worktree_status(), Some(WorktreeStatus::Active));
}

#[test]
fn a_gate_that_runs_twice_reports_the_same_verdict() {
    let mut gated = Gated::new(None);

    assert!(matches!(
        gated.pre_merge(MergePath::Integrate),
        PreMergeVerdict::Merged { .. }
    ));

    let verdict = gated.pre_merge(MergePath::Integrate);

    let PreMergeVerdict::Merged { commit, worktree } = verdict else {
        panic!("the second run sees the branch already landed, got {verdict:?}");
    };
    assert_eq!(commit, None);
    assert_eq!(worktree, None, "the worktree was already reclaimable");
}

#[test]
fn reclaim_releases_a_worktree_whose_branch_landed() {
    let mut gated = Gated::new(None);
    git(&gated.fixture.checkout, &["merge", "--quiet", BRANCH]);

    let verdict = gated.reclaim();

    let ReclaimVerdict::Released(applied) = verdict else {
        panic!("a merged, clean worktree is released, got {verdict:?}");
    };
    assert_eq!(applied.to, WorktreeStatus::Reclaimable);
    assert_eq!(
        gated.events(),
        ["gate_result", "run_state_changed", "worktree_reclaimable"]
    );
}

#[test]
fn reclaim_refuses_a_worktree_git_says_is_not_merged() {
    let mut gated = Gated::new(None);

    let verdict = gated.reclaim();

    assert!(
        matches!(verdict, ReclaimVerdict::Refused(GateRefusal::NotMerged)),
        "merge state is re-derived, never read from the row, got {verdict:?}"
    );
    assert_eq!(gated.worktree_status(), Some(WorktreeStatus::Active));
}

#[test]
fn reclaim_asks_for_cleanup_before_releasing_a_dirty_worktree() {
    let mut gated = Gated::new(None);
    git(&gated.fixture.checkout, &["merge", "--quiet", BRANCH]);
    write(&gated.fixture.worktree, "scratch.txt", "uncommitted\n");

    let verdict = gated.reclaim();

    let ReclaimVerdict::CleanupRequired(request) = verdict else {
        panic!("a dirty worktree is cleaned before it is let go, got {verdict:?}");
    };
    assert_eq!(request.kind, SubAgentKind::Cleanup);
    assert_eq!(request.branch.as_deref(), Some(BRANCH));
    assert_eq!(gated.worktree_status(), Some(WorktreeStatus::Active));
}

#[test]
fn a_worktree_outside_the_data_directory_is_refused_rather_than_derived_from() {
    let fixture = Fixture::new();
    fixture.commit("feature.txt", "work\n");

    let mut store = fixture.store();
    let run_id = store
        .insert_run(&RunRow {
            worktree_path: Some(fixture.checkout.to_string_lossy().into_owned()),
            ..fixture.run(None)
        })
        .expect("insert run");
    let mut gates = Gates::new(StateMachines::new(store), fixture.worktrees());

    let error = gates
        .reclaim(&ReclaimRequest {
            run_id,
            main_ref: "main".to_owned(),
            now: NOW,
        })
        .expect_err("a path outside the data directory is not derived from");

    assert!(
        error.to_string().contains("worktree path"),
        "the refusal names the path, got {error}"
    );
}

use agens_core::SessionMetadata;
use agens_store::{CompactionStore, SessionStore};

struct Temporary(std::path::PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn directory(label: &str) -> Temporary {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the fixture clock is after the epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "agens-compactions-{label}-{}-{started}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&path).ok();
    std::fs::create_dir_all(&path).expect("test data directory");
    Temporary(path)
}

/// The compaction table hangs off `sessions`, so a session has to exist before
/// anything can be recorded against it.
fn seed_session(path: &std::path::Path, id: i64) {
    let metadata = SessionMetadata {
        id,
        project: "project".into(),
        title: "title".into(),
        active_agent: "primary".into(),
        provider_id: None,
        model_id: None,
        reasoning_effort: None,
        created_at: 10,
        updated_at: 20,
        completed_turn_count: 0,
        resumable: false,
        parent_session_id: None,
        fork_message_count: None,
    };
    let mut store = SessionStore::open(path).expect("the session store opens");
    store
        .begin_session_attempt(&metadata, "retry".into())
        .expect("the session row is created");
}

#[test]
fn compactions_accumulate_and_the_latest_is_the_one_a_summary_folds_into() {
    let temporary = directory("append");
    seed_session(&temporary.0, 7);
    let mut store = CompactionStore::open(&temporary.0).expect("the compaction store opens");

    store.append(7, "first summary", 4).expect("first append");
    store.append(7, "second summary", 9).expect("second append");

    let all = store.list(7).expect("the compactions are readable");
    assert_eq!(
        all.iter()
            .map(|entry| (entry.summary.as_str(), entry.first_kept_message_index))
            .collect::<Vec<_>>(),
        vec![("first summary", 4), ("second summary", 9)],
        "an earlier compaction is kept, not overwritten",
    );

    let latest = store
        .latest(7)
        .expect("the latest compaction is readable")
        .expect("a compaction was recorded");
    assert_eq!(latest.summary, "second summary");
}

#[test]
fn a_session_that_was_never_compacted_has_no_latest_summary() {
    let temporary = directory("empty");
    seed_session(&temporary.0, 7);
    let store = CompactionStore::open(&temporary.0).expect("the compaction store opens");

    assert_eq!(store.latest(7).expect("the read succeeds"), None);
}

/// A row claiming a stretch of history was summarized into nothing is
/// indistinguishable, on a later read, from that history having been lost.
#[test]
fn an_empty_summary_is_never_recorded() {
    let temporary = directory("empty-summary");
    seed_session(&temporary.0, 7);
    let mut store = CompactionStore::open(&temporary.0).expect("the compaction store opens");

    assert!(store.append(7, "   \n", 4).is_err());
    assert_eq!(store.latest(7).expect("the read succeeds"), None);
}

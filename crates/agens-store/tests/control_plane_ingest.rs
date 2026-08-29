//! The one write ingest performs: the journal entries a harness fact produced,
//! the health snapshot it recomputed, and the first freeze of the run's genesis
//! paths, all in one transaction.

use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_store::{
    AttemptRow, ControlPlaneStore, EventClass, EventRow, IngestWrite, RunHealthRow, RunRow,
    RunState,
};
use rusqlite::Connection;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-store-control-plane-ingest-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

#[test]
fn an_ingest_write_lands_its_journal_and_its_health_snapshot_together() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&running_run()).unwrap();

    let outcome = store
        .apply_ingest(&IngestWrite {
            run_id,
            health: &health_snapshot(run_id, 2),
            freeze_genesis_paths: None,
            charge_attempt_tokens: None,
            events: &[event(run_id, "turn_ended"), event(run_id, "worker_lost")],
        })
        .unwrap();

    assert_eq!(outcome.event_ids.len(), 2);
    assert!(!outcome.genesis_paths_frozen);
    assert_eq!(event_types(&store, run_id), ["turn_ended", "worker_lost"]);
    assert_eq!(
        store.load_run_health(run_id).unwrap().unwrap().noop_turns,
        2
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_ingest_write_whose_journal_is_refused_leaves_the_health_snapshot_untouched() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&running_run()).unwrap();
    store
        .record_run_health(&health_snapshot(run_id, 1))
        .unwrap();

    let mut malformed = event(run_id, "turn_ended");
    malformed.payload = "not json".to_owned();

    let failure = store
        .apply_ingest(&IngestWrite {
            run_id,
            health: &health_snapshot(run_id, 9),
            freeze_genesis_paths: Some(r#"["src/one.rs"]"#),
            charge_attempt_tokens: None,
            events: &[malformed],
        })
        .unwrap_err();

    assert!(!failure.is_conflict());
    assert_eq!(
        store.load_run_health(run_id).unwrap().unwrap().noop_turns,
        1
    );
    assert_eq!(store.load_run(run_id).unwrap().unwrap().genesis_paths, None);
    assert!(store.events_for_run(run_id).unwrap().is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn the_first_freeze_of_genesis_paths_wins_and_a_later_one_never_moves_it() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&running_run()).unwrap();

    let first = store
        .apply_ingest(&IngestWrite {
            run_id,
            health: &health_snapshot(run_id, 0),
            freeze_genesis_paths: Some(r#"["src/one.rs"]"#),
            charge_attempt_tokens: None,
            events: &[event(run_id, "genesis_paths_frozen")],
        })
        .unwrap();
    let second = store
        .apply_ingest(&IngestWrite {
            run_id,
            health: &health_snapshot(run_id, 0),
            freeze_genesis_paths: Some(r#"["src/two.rs"]"#),
            charge_attempt_tokens: None,
            events: &[event(run_id, "genesis_paths_frozen")],
        })
        .unwrap();

    assert!(first.genesis_paths_frozen);
    assert!(!second.genesis_paths_frozen);
    assert_eq!(
        store.load_run(run_id).unwrap().unwrap().genesis_paths,
        Some(r#"["src/one.rs"]"#.to_owned())
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn touched_paths_are_the_mutations_the_ledger_recorded_for_this_run_alone() {
    let directory = data_directory();
    let mut store = ControlPlaneStore::open(&directory).unwrap();
    let run_id = store.insert_run(&running_run()).unwrap();
    let other_run_id = store.insert_run(&running_run()).unwrap();

    let connection = Connection::open(store.database_path()).unwrap();
    seed_session_attempt(&connection, 1);
    seed_session_attempt(&connection, 2);
    store.insert_attempt(&ledger_attempt(run_id, 1)).unwrap();
    store
        .insert_attempt(&ledger_attempt(other_run_id, 2))
        .unwrap();

    seed_fact(&connection, 1, 1, "write", "succeeded", Some("src/b.rs"));
    seed_fact(&connection, 1, 2, "edit", "succeeded", Some("src/a.rs"));
    seed_fact(&connection, 1, 3, "edit", "succeeded", Some("src/a.rs"));
    seed_fact(&connection, 1, 4, "read", "succeeded", Some("src/read.rs"));
    seed_fact(&connection, 1, 5, "write", "failed", Some("src/failed.rs"));
    seed_fact(&connection, 1, 6, "bash", "succeeded", None);
    seed_fact(
        &connection,
        2,
        1,
        "write",
        "succeeded",
        Some("src/other.rs"),
    );

    assert_eq!(
        store.touched_paths_for_run(run_id).unwrap(),
        ["src/a.rs", "src/b.rs"]
    );
    assert_eq!(
        store.touched_paths_for_run(other_run_id).unwrap(),
        ["src/other.rs"]
    );

    fs::remove_dir_all(directory).unwrap();
}

fn running_run() -> RunRow {
    RunRow {
        id: None,
        repo_id: "a1b2c3d4e5f60718".to_owned(),
        repo_root: "/home/dev/agens".to_owned(),
        remote_url: None,
        external_ref: None,
        parent_run_id: None,
        task: "ingest and health signals".to_owned(),
        scope: "crates/agens-server".to_owned(),
        dod: "green gate".to_owned(),
        genesis_paths: None,
        state: RunState::Running,
        priority: 3,
        dep_run_id: None,
        provider: "anthropic".to_owned(),
        budget_tokens: None,
        worktree_path: None,
        worktree_status: None,
        created_at: 1_700_000_000,
        result: None,
    }
}

fn health_snapshot(run_id: i64, noop_turns: i64) -> RunHealthRow {
    RunHealthRow {
        run_id,
        last_progress_turn: Some(4),
        noop_turns,
        failing_test_signature: None,
        tokens_since_progress: 1_200,
        updated_at: 1_700_000_200,
    }
}

fn event(run_id: i64, event_type: &str) -> EventRow {
    EventRow {
        id: None,
        run_id: Some(run_id),
        event_type: event_type.to_owned(),
        class: EventClass::Agent,
        payload: "{}".to_owned(),
        ts: 1_700_000_100,
    }
}

fn event_types(store: &ControlPlaneStore, run_id: i64) -> Vec<String> {
    store
        .events_for_run(run_id)
        .unwrap()
        .into_iter()
        .map(|event| event.event_type)
        .collect()
}

fn ledger_attempt(run_id: i64, session_attempt_id: i64) -> AttemptRow {
    AttemptRow {
        id: None,
        run_id,
        n: 1,
        session_id: Some(session_attempt_id),
        session_attempt_id: Some(session_attempt_id),
        started_at: 1_700_000_000,
        ended_at: None,
        outcome: None,
        retry_trigger: None,
        tokens: None,
        cost_micros: None,
    }
}

/// One session per attempt: a session may only hold one `running` attempt, and
/// the ledger rows this reads back only need the attempt to exist.
fn seed_session_attempt(connection: &Connection, id: i64) {
    connection
        .execute(
            "INSERT INTO sessions (
                 id, project, title, active_agent, created_at, updated_at
             ) VALUES (?1, 'project', 'title', 'build', 0, 0)",
            [id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO session_attempts (
                 id, session_id, sequence, status, retry_prompt, started_at
             ) VALUES (?1, ?1, 1, 'running', 'retry', 0)",
            [id],
        )
        .unwrap();
}

fn seed_fact(
    connection: &Connection,
    attempt_id: i64,
    sequence: i64,
    tool: &str,
    outcome: &str,
    path: Option<&str>,
) {
    let path_status = if path.is_some() {
        "relative"
    } else {
        "not_applicable"
    };

    connection
        .execute(
            "INSERT INTO tool_result_facts (
                 session_id, attempt_id, sequence, tool_call_id, tool, outcome, path,
                 path_status, recorded_at
             ) VALUES (1, ?1, ?2, 'call', ?3, ?4, ?5, ?6, 0)",
            rusqlite::params![attempt_id, sequence, tool, outcome, path, path_status],
        )
        .unwrap();
}

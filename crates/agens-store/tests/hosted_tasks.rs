use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use agens_core::{
    SessionMetadata,
    hosted::{
        HostedControlCommand, HostedControlKind, HostedTaskLimits, HostedTaskReplay,
        HostedTaskState, TaskControlError,
    },
};
use agens_store::{HostedTaskStore, SessionStore};
use rusqlite::Connection;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn data_directory() -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "agens-hosted-tasks-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

fn session(directory: &PathBuf, id: i64) {
    SessionStore::open(directory)
        .unwrap()
        .open_session(&SessionMetadata {
            id,
            project: "project".into(),
            title: "Hosted".into(),
            active_agent: "general".into(),
            provider_id: None,
            model_id: None,
            reasoning_effort: None,
            created_at: id,
            updated_at: id,
            completed_turn_count: 0,
            resumable: false,
            parent_session_id: None,
            fork_message_count: None,
        })
        .unwrap();
}

fn control(
    session_id: i64,
    task_id: &str,
    command_id: &str,
    kind: HostedControlKind,
) -> HostedControlCommand {
    HostedControlCommand::new(session_id, Some(task_id.into()), command_id, kind)
}

#[test]
fn snapshot_tail_replays_the_pruning_snapshot_and_ordered_tail() {
    let directory = data_directory();
    session(&directory, 1);
    let mut store =
        HostedTaskStore::open_with_limits(&directory, HostedTaskLimits::with_limits(2, 10))
            .unwrap();

    store
        .append_event(1, "task", HostedTaskState::Running, "started")
        .unwrap();
    store
        .append_event(1, "task", HostedTaskState::Background, "background")
        .unwrap();
    store
        .append_event(1, "task", HostedTaskState::Completed, "done")
        .unwrap();

    let HostedTaskReplay::SnapshotTail { snapshot, events } = store.snapshot_tail(1).unwrap()
    else {
        panic!("snapshot tail expected");
    };
    assert_eq!(snapshot.cursor(), 1);
    assert_eq!(snapshot.tasks()[0].state(), HostedTaskState::Running);
    assert_eq!(
        events
            .iter()
            .map(|event| event.cursor())
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(events.last().unwrap().state(), HostedTaskState::Completed);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn replay_reports_a_gap_for_a_cursor_older_than_the_snapshot_floor() {
    let directory = data_directory();
    session(&directory, 1);
    let mut store =
        HostedTaskStore::open_with_limits(&directory, HostedTaskLimits::with_limits(2, 10))
            .unwrap();
    for state in [
        HostedTaskState::Running,
        HostedTaskState::Background,
        HostedTaskState::Completed,
    ] {
        store.append_event(1, "task", state, "event").unwrap();
    }

    assert_eq!(
        store.replay_after(1, 0).unwrap(),
        HostedTaskReplay::Gap { oldest_cursor: 1 }
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn completed_child_turns_survive_reopen() {
    let directory = data_directory();
    session(&directory, 1);
    HostedTaskStore::open(&directory)
        .unwrap()
        .persist_completed_child_turn(1, "task", 1, "turn-json")
        .unwrap();

    let reopened = HostedTaskStore::open(&directory).unwrap();
    let turns = reopened.completed_child_turns(1).unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].payload(), "turn-json");
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn deleting_a_session_cascades_all_hosted_task_rows() {
    let directory = data_directory();
    session(&directory, 1);
    let mut tasks = HostedTaskStore::open(&directory).unwrap();
    tasks
        .append_event(1, "task", HostedTaskState::Running, "started")
        .unwrap();
    tasks
        .persist_completed_child_turn(1, "task", 1, "turn")
        .unwrap();
    tasks
        .apply_control(&control(1, "task", "cancel", HostedControlKind::Cancel))
        .unwrap();
    drop(tasks);

    SessionStore::open(&directory)
        .unwrap()
        .delete_session(1)
        .unwrap();
    let connection = Connection::open(directory.join("agens.db")).unwrap();
    for table in [
        "hosted_task_events",
        "hosted_task_snapshots",
        "hosted_child_turns",
        "hosted_task_controls",
        "hosted_tasks",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn control_failure_rolls_back_and_replay_skips_the_live_effect() {
    let directory = data_directory();
    session(&directory, 1);
    let mut store = HostedTaskStore::open(&directory).unwrap();
    store
        .append_event(1, "task", HostedTaskState::Running, "started")
        .unwrap();
    let command = control(1, "task", "cancel", HostedControlKind::Cancel);

    let error = store
        .apply_control_with(&command, || Err(TaskControlError::WrongSession))
        .unwrap_err();
    assert_eq!(error.kind(), TaskControlError::WrongSession);
    let first = store.apply_control_with(&command, || Ok(())).unwrap();
    let second = store
        .apply_control_with(&command, || panic!("replay repeated live effect"))
        .unwrap();

    assert!(!first.replayed());
    assert!(second.replayed());
    assert_eq!(first.state(), second.state());
    let HostedTaskReplay::Events(events) = store.replay_after(1, 0).unwrap() else {
        panic!("events expected")
    };
    assert_eq!(events.len(), 2);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn reused_command_id_with_a_different_request_hash_conflicts() {
    let directory = data_directory();
    session(&directory, 1);
    let mut store = HostedTaskStore::open(&directory).unwrap();
    store
        .append_event(1, "task", HostedTaskState::Running, "started")
        .unwrap();
    store
        .apply_control(&control(1, "task", "same", HostedControlKind::Background))
        .unwrap();

    let error = store
        .apply_control(&control(1, "task", "same", HostedControlKind::Cancel))
        .unwrap_err();
    assert_eq!(error.kind(), TaskControlError::CommandConflict);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn control_capacity_rejects_new_commands_but_keeps_duplicate_replay() {
    let directory = data_directory();
    session(&directory, 1);
    let mut store =
        HostedTaskStore::open_with_limits(&directory, HostedTaskLimits::with_limits(10, 1))
            .unwrap();
    store
        .append_event(1, "task", HostedTaskState::Running, "started")
        .unwrap();
    let first = control(1, "task", "background", HostedControlKind::Background);
    store.apply_control(&first).unwrap();
    assert!(store.apply_control(&first).unwrap().replayed());

    let error = store
        .apply_control(&control(
            1,
            "task",
            "message",
            HostedControlKind::Message("hello".into()),
        ))
        .unwrap_err();
    assert_eq!(error.kind(), TaskControlError::ControlCapacity);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn controls_cannot_cross_session_or_apply_invalid_transitions() {
    let directory = data_directory();
    session(&directory, 1);
    session(&directory, 2);
    let mut store = HostedTaskStore::open(&directory).unwrap();
    store
        .append_event(1, "task", HostedTaskState::Completed, "done")
        .unwrap();

    let wrong = store
        .apply_control(&control(2, "task", "wrong", HostedControlKind::Cancel))
        .unwrap_err();
    assert_eq!(wrong.kind(), TaskControlError::UnknownTask);
    let invalid = store
        .apply_control(&control(
            1,
            "task",
            "invalid",
            HostedControlKind::Background,
        ))
        .unwrap_err();
    assert_eq!(invalid.kind(), TaskControlError::InvalidTransition);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn hosted_task_limits_default_to_ten_thousand_records() {
    assert_eq!(
        HostedTaskLimits::default(),
        HostedTaskLimits::with_limits(10_000, 10_000)
    );
}

use crate::database;
use agens_core::hosted::{
    HostedChildTurn, HostedControlCommand, HostedControlKind, HostedControlResult, HostedTaskEvent,
    HostedTaskJournal, HostedTaskLimits, HostedTaskRecord, HostedTaskReplay, HostedTaskSnapshot,
    HostedTaskState, TaskControlError,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedTaskStoreError {
    kind: TaskControlError,
    message: String,
}
impl HostedTaskStoreError {
    fn new(kind: TaskControlError, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    fn database(operation: &str, path: &Path, error: impl fmt::Display) -> Self {
        Self::new(
            TaskControlError::Storage,
            format!("hosted tasks {operation} at {}: {error}", path.display()),
        )
    }
    fn from_database(error: database::DatabaseError) -> Self {
        Self::database(error.operation(), error.path(), error.detail())
    }
    pub const fn kind(&self) -> TaskControlError {
        self.kind
    }
}
impl fmt::Display for HostedTaskStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for HostedTaskStoreError {}

pub struct HostedTaskStore {
    database_path: PathBuf,
    connection: Connection,
    limits: HostedTaskLimits,
}
impl HostedTaskStore {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, HostedTaskStoreError> {
        Self::open_with_limits(directory, HostedTaskLimits::default())
    }
    pub fn open_with_limits(
        directory: impl AsRef<Path>,
        limits: HostedTaskLimits,
    ) -> Result<Self, HostedTaskStoreError> {
        if limits.event_records() == 0 || limits.control_records() == 0 {
            return Err(error(
                TaskControlError::InvalidRequest,
                "hosted task limits must be positive",
            ));
        }
        let (database_path, connection) = database::open_unified_database(directory.as_ref())
            .map_err(HostedTaskStoreError::from_database)?;
        Ok(Self {
            database_path,
            connection,
            limits,
        })
    }
    pub fn append_event(
        &mut self,
        session: i64,
        task: &str,
        state: HostedTaskState,
        payload: &str,
    ) -> Result<HostedTaskEvent, HostedTaskStoreError> {
        validate_identity(session, task)?;
        let limit = self.limits.event_records();
        let path = self.database_path.clone();
        let tx = self.transaction("append event")?;
        let event = append_event(&tx, session, task, state, payload, limit)?;
        tx.commit()
            .map_err(|e| HostedTaskStoreError::database("commit event", &path, e))?;
        Ok(event)
    }
    pub fn replay_after(
        &self,
        session: i64,
        cursor: u64,
    ) -> Result<HostedTaskReplay, HostedTaskStoreError> {
        ensure_session(&self.connection, session)?;
        let floor = snapshot_cursor(&self.connection, session)?;
        if cursor < floor {
            return Ok(HostedTaskReplay::Gap {
                oldest_cursor: floor,
            });
        }
        Ok(HostedTaskReplay::Events(load_events(
            &self.connection,
            session,
            cursor,
        )?))
    }
    pub fn snapshot_tail(&self, session: i64) -> Result<HostedTaskReplay, HostedTaskStoreError> {
        ensure_session(&self.connection, session)?;
        let cursor = snapshot_cursor(&self.connection, session)?;
        Ok(HostedTaskReplay::SnapshotTail {
            snapshot: HostedTaskSnapshot::new(
                cursor,
                load_snapshot_tasks(&self.connection, session)?,
            )
            .with_child_turns(self.completed_child_turns(session)?),
            events: load_events(&self.connection, session, cursor)?,
        })
    }
    pub fn persist_completed_child_turn(
        &mut self,
        session: i64,
        task: &str,
        sequence: u64,
        payload: &str,
    ) -> Result<(), HostedTaskStoreError> {
        validate_identity(session, task)?;
        self.connection.execute("INSERT INTO hosted_child_turns(session_id,task_id,sequence,payload) VALUES(?1,?2,?3,?4)", params![session,task,i64::try_from(sequence).map_err(storage)?,payload]).map_err(storage)?;
        Ok(())
    }
    pub fn completed_child_turns(
        &self,
        session: i64,
    ) -> Result<Vec<HostedChildTurn>, HostedTaskStoreError> {
        ensure_session(&self.connection, session)?;
        let mut stmt = self.connection.prepare("SELECT task_id,sequence,payload FROM hosted_child_turns WHERE session_id=?1 ORDER BY task_id,sequence").map_err(storage)?;
        stmt.query_map([session], |r| {
            Ok(HostedChildTurn::new(
                r.get::<_, String>(0)?,
                u64::try_from(r.get::<_, i64>(1)?)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, -1))?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(storage)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage)
    }
    pub fn apply_control(
        &mut self,
        command: &HostedControlCommand,
    ) -> Result<HostedControlResult, HostedTaskStoreError> {
        self.apply_control_with(command, || Ok(()))
    }

    pub fn apply_control_with(
        &mut self,
        command: &HostedControlCommand,
        apply: impl FnOnce() -> Result<(), TaskControlError>,
    ) -> Result<HostedControlResult, HostedTaskStoreError> {
        validate_command(command)?;
        let hash = request_hash(command);
        let limit = self.limits.event_records();
        let capacity = self.limits.control_records();
        let path = self.database_path.clone();
        let tx = self.transaction("apply control")?;
        let stored = tx.query_row("SELECT request_hash,result_state FROM hosted_task_controls WHERE session_id=?1 AND command_id=?2", params![command.session_id(),command.command_id()], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional().map_err(storage)?;
        if let Some((stored_hash, state)) = stored {
            if stored_hash != hash {
                return Err(error(
                    TaskControlError::CommandConflict,
                    "command id has a different request hash",
                ));
            }
            return Ok(HostedControlResult::new(decode_state(&state)?, true));
        }
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM hosted_task_controls WHERE session_id=?1",
                [command.session_id()],
                |r| r.get(0),
            )
            .map_err(storage)?;
        if usize::try_from(count).unwrap_or(usize::MAX) >= capacity {
            return Err(error(
                TaskControlError::ControlCapacity,
                "hosted task control capacity reached",
            ));
        }
        let state = apply_transition(&tx, command, limit)?;
        apply().map_err(|kind| error(kind, "live task control failed"))?;
        tx.execute("INSERT INTO hosted_task_controls(session_id,command_id,request_hash,result_state) VALUES(?1,?2,?3,?4)", params![command.session_id(),command.command_id(),hash,encode_state(state)]).map_err(storage)?;
        tx.commit()
            .map_err(|e| HostedTaskStoreError::database("commit control", &path, e))?;
        Ok(HostedControlResult::new(state, false))
    }
    fn transaction(&mut self, operation: &str) -> Result<Transaction<'_>, HostedTaskStoreError> {
        let path = self.database_path.clone();
        self.connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| HostedTaskStoreError::database(operation, &path, e))
    }
}

fn validate_identity(session: i64, task: &str) -> Result<(), HostedTaskStoreError> {
    if session <= 0 || task.trim().is_empty() {
        Err(error(
            TaskControlError::InvalidRequest,
            "session and task ids are required",
        ))
    } else {
        Ok(())
    }
}
fn validate_command(command: &HostedControlCommand) -> Result<(), HostedTaskStoreError> {
    if command.session_id() <= 0 || command.command_id().trim().is_empty() {
        return Err(error(
            TaskControlError::InvalidRequest,
            "session and command ids are required",
        ));
    }
    if !matches!(command.kind(), HostedControlKind::CancelAll) && command.task_id().is_none() {
        return Err(error(
            TaskControlError::InvalidRequest,
            "task id is required",
        ));
    }
    Ok(())
}
fn ensure_session(connection: &Connection, session: i64) -> Result<(), HostedTaskStoreError> {
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            [session],
            |r| r.get(0),
        )
        .map_err(storage)?;
    if exists {
        Ok(())
    } else {
        Err(error(
            TaskControlError::WrongSession,
            "hosted session does not exist",
        ))
    }
}
fn append_event(
    tx: &Transaction<'_>,
    session: i64,
    task: &str,
    state: HostedTaskState,
    payload: &str,
    limit: usize,
) -> Result<HostedTaskEvent, HostedTaskStoreError> {
    ensure_session(tx, session)?;
    let cursor: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(cursor),0)+1 FROM hosted_task_events WHERE session_id=?1",
            [session],
            |r| r.get(0),
        )
        .map_err(storage)?;
    tx.execute("INSERT INTO hosted_task_events(session_id,cursor,task_id,state,payload) VALUES(?1,?2,?3,?4,?5)", params![session,cursor,task,encode_state(state),payload]).map_err(storage)?;
    tx.execute("INSERT INTO hosted_tasks(session_id,task_id,state) VALUES(?1,?2,?3) ON CONFLICT(session_id,task_id) DO UPDATE SET state=excluded.state", params![session,task,encode_state(state)]).map_err(storage)?;
    prune(tx, session, limit)?;
    Ok(HostedTaskEvent::new(
        u64::try_from(cursor).map_err(storage)?,
        task,
        state,
        payload,
    ))
}
fn prune(tx: &Transaction<'_>, session: i64, limit: usize) -> Result<(), HostedTaskStoreError> {
    let count: i64 = tx
        .query_row(
            "SELECT count(*) FROM hosted_task_events WHERE session_id=?1",
            [session],
            |r| r.get(0),
        )
        .map_err(storage)?;
    if usize::try_from(count).unwrap_or(usize::MAX) <= limit {
        return Ok(());
    }
    let floor: i64 = tx.query_row("SELECT cursor FROM hosted_task_events WHERE session_id=?1 ORDER BY cursor DESC LIMIT 1 OFFSET ?2", params![session,i64::try_from(limit).map_err(storage)?], |r| r.get(0)).map_err(storage)?;
    tx.execute("INSERT INTO hosted_task_snapshots(session_id,cursor) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET cursor=excluded.cursor", params![session,floor]).map_err(storage)?;
    tx.execute(
        "UPDATE hosted_task_snapshot_tasks SET snapshot_cursor=?2 WHERE session_id=?1",
        params![session, floor],
    )
    .map_err(storage)?;
    tx.execute("INSERT INTO hosted_task_snapshot_tasks(session_id,snapshot_cursor,task_id,state) SELECT e.session_id,?2,e.task_id,e.state FROM hosted_task_events e WHERE e.session_id=?1 AND e.cursor=(SELECT MAX(x.cursor) FROM hosted_task_events x WHERE x.session_id=e.session_id AND x.task_id=e.task_id AND x.cursor<=?2) ON CONFLICT(session_id,task_id) DO UPDATE SET snapshot_cursor=excluded.snapshot_cursor,state=excluded.state", params![session,floor]).map_err(storage)?;
    tx.execute(
        "DELETE FROM hosted_task_events WHERE session_id=?1 AND cursor<=?2",
        params![session, floor],
    )
    .map_err(storage)?;
    Ok(())
}
fn apply_transition(
    tx: &Transaction<'_>,
    command: &HostedControlCommand,
    limit: usize,
) -> Result<HostedTaskState, HostedTaskStoreError> {
    if matches!(command.kind(), HostedControlKind::CancelAll) {
        let mut stmt = tx.prepare("SELECT task_id FROM hosted_tasks WHERE session_id=?1 AND state IN('running','background') ORDER BY task_id").map_err(storage)?;
        let ids = stmt
            .query_map([command.session_id()], |r| r.get::<_, String>(0))
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        drop(stmt);
        for id in ids {
            append_event(
                tx,
                command.session_id(),
                &id,
                HostedTaskState::Cancelled,
                "cancelled",
                limit,
            )?;
        }
        return Ok(HostedTaskState::Cancelled);
    }
    let task = command.task_id().expect("validated task id");
    let stored = tx
        .query_row(
            "SELECT state FROM hosted_tasks WHERE session_id=?1 AND task_id=?2",
            params![command.session_id(), task],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(storage)?
        .ok_or_else(|| {
            error(
                TaskControlError::UnknownTask,
                "task does not belong to the hosted session",
            )
        })?;
    let current = decode_state(&stored)?;
    let (next, payload) = match command.kind() {
        HostedControlKind::Background if current == HostedTaskState::Running => {
            (HostedTaskState::Background, "background")
        }
        HostedControlKind::Cancel
            if matches!(
                current,
                HostedTaskState::Running | HostedTaskState::Background
            ) =>
        {
            (HostedTaskState::Cancelled, "cancelled")
        }
        HostedControlKind::Message(message)
            if matches!(
                current,
                HostedTaskState::Running | HostedTaskState::Background
            ) =>
        {
            (current, message.as_str())
        }
        _ => {
            return Err(error(
                TaskControlError::InvalidTransition,
                "invalid hosted task transition",
            ));
        }
    };
    append_event(tx, command.session_id(), task, next, payload, limit)?;
    Ok(next)
}
fn snapshot_cursor(connection: &Connection, session: i64) -> Result<u64, HostedTaskStoreError> {
    let cursor: i64 = connection
        .query_row(
            "SELECT COALESCE((SELECT cursor FROM hosted_task_snapshots WHERE session_id=?1),0)",
            [session],
            |r| r.get(0),
        )
        .map_err(storage)?;
    u64::try_from(cursor).map_err(storage)
}
fn load_snapshot_tasks(
    connection: &Connection,
    session: i64,
) -> Result<Vec<HostedTaskRecord>, HostedTaskStoreError> {
    let mut stmt = connection.prepare("SELECT task_id,state FROM hosted_task_snapshot_tasks WHERE session_id=?1 ORDER BY task_id").map_err(storage)?;
    stmt.query_map([session], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })
    .map_err(storage)?
    .map(|row| {
        let (id, state) = row.map_err(storage)?;
        Ok(HostedTaskRecord::new(id, decode_state(&state)?))
    })
    .collect()
}
fn load_events(
    connection: &Connection,
    session: i64,
    cursor: u64,
) -> Result<Vec<HostedTaskEvent>, HostedTaskStoreError> {
    let mut stmt = connection.prepare("SELECT cursor,task_id,state,payload FROM hosted_task_events WHERE session_id=?1 AND cursor>?2 ORDER BY cursor").map_err(storage)?;
    stmt.query_map(
        params![session, i64::try_from(cursor).map_err(storage)?],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        },
    )
    .map_err(storage)?
    .map(|row| {
        let (cursor, id, state, payload) = row.map_err(storage)?;
        Ok(HostedTaskEvent::new(
            u64::try_from(cursor).map_err(storage)?,
            id,
            decode_state(&state)?,
            payload,
        ))
    })
    .collect()
}

fn request_hash(command: &HostedControlCommand) -> String {
    let mut bytes = Vec::new();
    for field in [
        command.session_id().to_string(),
        command.task_id().unwrap_or("").to_owned(),
        control_kind(command.kind()),
    ] {
        bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
        bytes.extend_from_slice(field.as_bytes());
    }
    format!("{:x}", Sha256::digest(bytes))
}
fn control_kind(kind: &HostedControlKind) -> String {
    match kind {
        HostedControlKind::Background => "background".into(),
        HostedControlKind::Cancel => "cancel".into(),
        HostedControlKind::CancelAll => "cancel-all".into(),
        HostedControlKind::Message(message) => format!("message:{message}"),
    }
}
const fn encode_state(state: HostedTaskState) -> &'static str {
    match state {
        HostedTaskState::Running => "running",
        HostedTaskState::Background => "background",
        HostedTaskState::Completed => "completed",
        HostedTaskState::Cancelled => "cancelled",
        HostedTaskState::Failed => "failed",
    }
}
fn decode_state(state: &str) -> Result<HostedTaskState, HostedTaskStoreError> {
    match state {
        "running" => Ok(HostedTaskState::Running),
        "background" => Ok(HostedTaskState::Background),
        "completed" => Ok(HostedTaskState::Completed),
        "cancelled" => Ok(HostedTaskState::Cancelled),
        "failed" => Ok(HostedTaskState::Failed),
        _ => Err(error(
            TaskControlError::Storage,
            "invalid stored task state",
        )),
    }
}
fn storage(error: impl fmt::Display) -> HostedTaskStoreError {
    HostedTaskStoreError::new(TaskControlError::Storage, error.to_string())
}
fn error(kind: TaskControlError, message: impl Into<String>) -> HostedTaskStoreError {
    HostedTaskStoreError::new(kind, message)
}

impl HostedTaskJournal for HostedTaskStore {
    fn append_event(
        &mut self,
        session_id: i64,
        task_id: &str,
        state: HostedTaskState,
        payload: &str,
    ) -> Result<HostedTaskEvent, TaskControlError> {
        HostedTaskStore::append_event(self, session_id, task_id, state, payload)
            .map_err(|error| error.kind())
    }
    fn persist_completed_child_turn(
        &mut self,
        session_id: i64,
        task_id: &str,
        sequence: u64,
        payload: &str,
    ) -> Result<(), TaskControlError> {
        HostedTaskStore::persist_completed_child_turn(self, session_id, task_id, sequence, payload)
            .map_err(|error| error.kind())
    }
    fn completed_child_turns(
        &self,
        session_id: i64,
    ) -> Result<Vec<HostedChildTurn>, TaskControlError> {
        HostedTaskStore::completed_child_turns(self, session_id).map_err(|error| error.kind())
    }
    fn snapshot_tail(&self, session_id: i64) -> Result<HostedTaskReplay, TaskControlError> {
        HostedTaskStore::snapshot_tail(self, session_id).map_err(|error| error.kind())
    }
    fn replay_after(
        &self,
        session_id: i64,
        after_cursor: u64,
    ) -> Result<HostedTaskReplay, TaskControlError> {
        HostedTaskStore::replay_after(self, session_id, after_cursor).map_err(|error| error.kind())
    }
    fn apply_control(
        &mut self,
        command: &HostedControlCommand,
    ) -> Result<HostedControlResult, TaskControlError> {
        HostedTaskStore::apply_control(self, command).map_err(|error| error.kind())
    }
}

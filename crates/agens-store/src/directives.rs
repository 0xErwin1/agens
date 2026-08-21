//! The durable queue a running turn drains at a safe point.
//!
//! Separate from the session store because its readers and writers are
//! different actors: a supervisor or a person writes, and the turn loop reads.
//! Sharing a connection with the session writer would put a queue read behind
//! the borrow the attempt lifecycle holds for a whole turn.

use std::fmt;
use std::path::{Path, PathBuf};

use std::future::{Future, ready};

use agens_core::{
    HeadlessIntraTurnInbox, HeadlessTurnPortError, IntraTurnInputSource, PendingIntraTurnInput,
};
use rusqlite::{Connection, params};

use crate::database;

/// When a queued message may be handed to a turn.
///
/// Two queues, not a priority: a message that changes what the run is doing
/// waits for the turn to close so the worker replans from a settled plan, and
/// one that does not waits only for the current batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectiveGrain {
    ToolCall,
    Turn,
}

impl DirectiveGrain {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolCall => "tool_call",
            Self::Turn => "turn",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectiveStoreError {
    message: String,
}

impl DirectiveStoreError {
    fn operation(operation: &str, path: &Path, error: impl fmt::Display) -> Self {
        Self {
            message: format!("directives {operation} at {}: {error}", path.display()),
        }
    }

    fn from_database(error: database::DatabaseError) -> Self {
        Self::operation(error.operation(), error.path(), error.detail())
    }

    fn detail(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for DirectiveStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DirectiveStoreError {}

pub struct DirectiveStore {
    database_path: PathBuf,
    connection: Connection,
}

impl DirectiveStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, DirectiveStoreError> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(DirectiveStoreError::from_database)?;

        Ok(Self {
            database_path,
            connection,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.database_path.clone()
    }

    pub fn enqueue(
        &mut self,
        session_id: i64,
        source: IntraTurnInputSource,
        grain: DirectiveGrain,
        text: &str,
    ) -> Result<(), DirectiveStoreError> {
        if text.trim().is_empty() {
            return Err(DirectiveStoreError::detail(
                "a directive carries no instruction",
            ));
        }

        self.connection
            .execute(
                "INSERT INTO directives (session_id, source, grain, text, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id,
                    source.as_str(),
                    grain.as_str(),
                    text,
                    timestamp()
                ],
            )
            .map_err(|error| {
                DirectiveStoreError::operation("enqueue", &self.database_path, error)
            })?;

        Ok(())
    }

    /// Hands over every undelivered directive for one session and grain, oldest
    /// first, and marks them delivered in the same transaction.
    ///
    /// One transaction because the two halves cannot disagree: a directive read
    /// but not marked is delivered twice, and a directive marked but not
    /// returned is lost with no trace of what was meant to steer the turn.
    pub fn drain(
        &mut self,
        session_id: i64,
        grain: DirectiveGrain,
    ) -> Result<Vec<PendingIntraTurnInput>, DirectiveStoreError> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| DirectiveStoreError::operation("drain", &self.database_path, error))?;

        let rows = {
            let mut statement = transaction
                .prepare(
                    "SELECT id, source, text FROM directives
                     WHERE session_id = ?1 AND grain = ?2 AND delivered_at IS NULL
                     ORDER BY id",
                )
                .map_err(|error| {
                    DirectiveStoreError::operation("drain", &self.database_path, error)
                })?;
            statement
                .query_map(params![session_id, grain.as_str()], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|error| {
                    DirectiveStoreError::operation("drain", &self.database_path, error)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|error| {
                    DirectiveStoreError::operation("drain", &self.database_path, error)
                })?
        };

        let mut drained = Vec::with_capacity(rows.len());
        for (id, source, text) in rows {
            let source = match source.as_str() {
                "human" => IntraTurnInputSource::Human,
                "supervisor" => IntraTurnInputSource::Supervisor,
                _ => return Err(DirectiveStoreError::detail("unknown directive source")),
            };

            transaction
                .execute(
                    "UPDATE directives SET delivered_at = ?2 WHERE id = ?1",
                    params![id, timestamp()],
                )
                .map_err(|error| {
                    DirectiveStoreError::operation("drain", &self.database_path, error)
                })?;
            drained.push(PendingIntraTurnInput { source, text });
        }

        transaction
            .commit()
            .map_err(|error| DirectiveStoreError::operation("drain", &self.database_path, error))?;

        Ok(drained)
    }
}

/// The queue, seen by a running turn.
///
/// Only the tool-call grain: the turn grain is not a turn's own business. By
/// the time a turn ends it is closed and can accept nothing, so what waits for
/// that grain belongs to whoever assembles the next prompt.
pub struct DirectiveInbox {
    store: DirectiveStore,
    session_id: i64,
}

impl DirectiveInbox {
    pub const fn new(store: DirectiveStore, session_id: i64) -> Self {
        Self { store, session_id }
    }
}

impl HeadlessIntraTurnInbox for DirectiveInbox {
    fn drain(
        &mut self,
    ) -> impl Future<Output = Result<Vec<PendingIntraTurnInput>, HeadlessTurnPortError>> + Send
    {
        // A queue that cannot be read is reported as empty rather than failing
        // the turn: losing a directive is bad, and killing work in progress over
        // an unreadable inbox is worse.
        ready(Ok(self
            .store
            .drain(self.session_id, DirectiveGrain::ToolCall)
            .unwrap_or_default()))
    }
}

fn timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().to_string())
        .unwrap_or_default()
}

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

/// What a queued row is: an answer to a question, a directive that steers, or
/// the "continue" that resumes a parked run.
///
/// All three drain through the same queue at the same two grains. The
/// distinction is for whoever reads the queue afterwards and for the run the
/// row belongs to: a resumed run has to be told apart from a steered one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectiveKind {
    Answer,
    Directive,
    Continue,
}

impl DirectiveKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Directive => "directive",
            Self::Continue => "continue",
        }
    }
}

/// Who a queued message is addressed to.
///
/// A delegated child does not read its parent session's queue. Several children
/// run under one session at once, so whichever drained first would take a
/// message meant for another and the parent turn would lose whatever a child
/// reached before it. Each addressable turn reads only what names it.
///
/// A child is named by the diagnostic reference its own turn publishes when it
/// starts, which is the only identity of a delegation that exists outside the
/// process running it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectiveTarget {
    Session(i64),
    Child(String),
}

impl DirectiveTarget {
    fn columns(&self) -> (Option<i64>, Option<&str>) {
        match self {
            Self::Session(id) => (Some(*id), None),
            Self::Child(reference) => (None, Some(reference.as_str())),
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

    /// Queues a message that steers the turn it reaches.
    pub fn enqueue(
        &mut self,
        target: &DirectiveTarget,
        source: IntraTurnInputSource,
        grain: DirectiveGrain,
        text: &str,
    ) -> Result<(), DirectiveStoreError> {
        self.enqueue_kind(target, DirectiveKind::Directive, source, grain, text)
    }

    /// Queues a message and says what it is.
    pub fn enqueue_kind(
        &mut self,
        target: &DirectiveTarget,
        kind: DirectiveKind,
        source: IntraTurnInputSource,
        grain: DirectiveGrain,
        text: &str,
    ) -> Result<(), DirectiveStoreError> {
        if text.trim().is_empty() {
            return Err(DirectiveStoreError::detail(
                "a directive carries no instruction",
            ));
        }

        let (session_id, child) = target.columns();
        self.connection
            .execute(
                "INSERT INTO directives (session_id, child, kind, source, grain, text, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session_id,
                    child,
                    kind.as_str(),
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

    /// Hands over every undelivered directive for one addressee and grain,
    /// oldest first, and marks them delivered in the same transaction.
    ///
    /// One transaction because the two halves cannot disagree: a directive read
    /// but not marked is delivered twice, and a directive marked but not
    /// returned is lost with no trace of what was meant to steer the turn.
    pub fn drain(
        &mut self,
        target: &DirectiveTarget,
        grain: DirectiveGrain,
    ) -> Result<Vec<PendingIntraTurnInput>, DirectiveStoreError> {
        let (session_id, child) = target.columns();
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| DirectiveStoreError::operation("drain", &self.database_path, error))?;

        let rows = {
            let mut statement = transaction
                .prepare(
                    "SELECT id, source, text FROM directives
                     WHERE session_id IS ?1 AND child IS ?2 AND grain = ?3
                       AND delivered_at IS NULL
                     ORDER BY id",
                )
                .map_err(|error| {
                    DirectiveStoreError::operation("drain", &self.database_path, error)
                })?;
            statement
                .query_map(params![session_id, child, grain.as_str()], |row| {
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
    store: Option<DirectiveStore>,
    target: DirectiveTarget,
}

impl DirectiveInbox {
    pub const fn new(store: DirectiveStore, target: DirectiveTarget) -> Self {
        Self {
            store: Some(store),
            target,
        }
    }

    /// The inbox for one session, opening the queue best-effort.
    ///
    /// Infallible on purpose: a turn that cannot open the queue runs without
    /// one, exactly as every turn did before the queue existed. Refusing to
    /// start over an unreadable inbox would make an optional channel a
    /// precondition for working at all.
    pub fn for_session(data_directory: impl AsRef<Path>, session_id: i64) -> Self {
        Self::for_target(data_directory, DirectiveTarget::Session(session_id))
    }

    /// The inbox for one delegated child turn, named by the reference that
    /// turn published when it started. Best-effort for the same reason: a
    /// delegation nobody can reach still has work to finish.
    pub fn for_child(data_directory: impl AsRef<Path>, reference: impl Into<String>) -> Self {
        Self::for_target(data_directory, DirectiveTarget::Child(reference.into()))
    }

    fn for_target(data_directory: impl AsRef<Path>, target: DirectiveTarget) -> Self {
        Self {
            store: DirectiveStore::open(data_directory).ok(),
            target,
        }
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
        let target = &self.target;
        let drained = self.store.as_mut().map_or_else(Vec::new, |store| {
            store
                .drain(target, DirectiveGrain::ToolCall)
                .unwrap_or_default()
        });

        ready(Ok(drained))
    }
}

fn timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().to_string())
        .unwrap_or_default()
}

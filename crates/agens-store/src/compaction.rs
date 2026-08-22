//! The append-only record of what a session's compactions replaced.
//!
//! A compaction does not rewrite the session's messages. It appends one row
//! naming the summary and the first message that survived the cut, so the
//! transcript underneath stays exactly as it was written and a later read can
//! still reconstruct what the session looked like before any of it happened.
//! Rewriting instead would make a compaction destructive, and a destructive
//! step is the wrong shape for a recovery: the run that triggers it is already
//! failing.
//!
//! Separate from the session writer for the same reason the directive queue is:
//! its writer is the recovery path, running while the attempt lifecycle holds
//! the session connection for the whole turn.

use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::database;

/// One compaction, as it was recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCompaction {
    pub id: i64,
    pub summary: String,
    /// Position of the first message the compaction kept verbatim, counted from
    /// zero over the session's ordered messages. Everything before it is what
    /// the summary stands for.
    pub first_kept_message_index: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionStoreError {
    message: String,
}

impl CompactionStoreError {
    fn operation(operation: &str, path: &Path, error: impl fmt::Display) -> Self {
        Self {
            message: format!("compactions {operation} at {}: {error}", path.display()),
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

impl fmt::Display for CompactionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CompactionStoreError {}

pub struct CompactionStore {
    database_path: PathBuf,
    connection: Connection,
}

impl CompactionStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, CompactionStoreError> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(CompactionStoreError::from_database)?;

        Ok(Self {
            database_path,
            connection,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.database_path.clone()
    }

    /// Records one compaction.
    ///
    /// An empty summary is refused here as well as at the layer that produced
    /// it: a row claiming a stretch of history was summarized into nothing
    /// would be indistinguishable, on a later read, from that history simply
    /// having been lost.
    pub fn append(
        &mut self,
        session_id: i64,
        summary: &str,
        first_kept_message_index: i64,
    ) -> Result<i64, CompactionStoreError> {
        if summary.trim().is_empty() {
            return Err(CompactionStoreError::detail("a compaction carries no summary"));
        }

        self.connection
            .execute(
                "INSERT INTO session_compactions
                     (session_id, summary, first_kept_message_index, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    session_id,
                    summary,
                    first_kept_message_index,
                    timestamp()
                ],
            )
            .map_err(|error| {
                CompactionStoreError::operation("append", &self.database_path, error)
            })?;

        Ok(self.connection.last_insert_rowid())
    }

    /// The most recent compaction of a session, which is the summary the next
    /// one folds into its own.
    pub fn latest(
        &self,
        session_id: i64,
    ) -> Result<Option<StoredCompaction>, CompactionStoreError> {
        self.connection
            .query_row(
                "SELECT id, summary, first_kept_message_index FROM session_compactions
                 WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
                params![session_id],
                |row| {
                    Ok(StoredCompaction {
                        id: row.get(0)?,
                        summary: row.get(1)?,
                        first_kept_message_index: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|error| CompactionStoreError::operation("latest", &self.database_path, error))
    }

    /// Every compaction of a session, oldest first.
    pub fn list(&self, session_id: i64) -> Result<Vec<StoredCompaction>, CompactionStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, summary, first_kept_message_index FROM session_compactions
                 WHERE session_id = ?1 ORDER BY id",
            )
            .map_err(|error| CompactionStoreError::operation("list", &self.database_path, error))?;

        let rows = statement
            .query_map(params![session_id], |row| {
                Ok(StoredCompaction {
                    id: row.get(0)?,
                    summary: row.get(1)?,
                    first_kept_message_index: row.get(2)?,
                })
            })
            .map_err(|error| CompactionStoreError::operation("list", &self.database_path, error))?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| CompactionStoreError::operation("list", &self.database_path, error))
    }
}

fn timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().to_string())
        .unwrap_or_default()
}

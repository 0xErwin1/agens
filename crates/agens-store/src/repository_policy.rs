//! Where the daemon records what it has been told about a repository's
//! provisioning hooks.
//!
//! A hook is repository code executed with the daemon's whole environment, so
//! the register naming the repositories that have earned that is the one thing
//! a run's own worktree must never be able to edit. It lives in the control
//! plane rather than in a file under the data directory for that reason: the
//! daemon reaches it through SQL it owns, and there is no hand-editable
//! document two levels above a worktree for a run to append its own fingerprint
//! to.
//!
//! Timestamps come from the caller, like everywhere else in this crate.

use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use crate::database;

/// What the operator has said about one repository's provisioning hooks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoredHookTrust {
    Granted,
    Refused,
    Unknown,
}

/// A repository whose hooks are waiting on one durable question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPendingTrust {
    pub question_id: i64,
    pub repo_id: String,
    /// The canonical checkout, kept so whoever reads the question knows what it
    /// is about without having to resolve a fingerprint.
    pub repository: PathBuf,
    pub asked_at: i64,
}

#[derive(Debug)]
pub struct RepositoryPolicyStoreError {
    message: String,
}

impl RepositoryPolicyStoreError {
    fn operation(operation: &str, path: &Path, detail: impl fmt::Display) -> Self {
        Self {
            message: format!(
                "failed to {operation} the repository policy at {}: {detail}",
                path.display()
            ),
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

impl fmt::Display for RepositoryPolicyStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RepositoryPolicyStoreError {}

type Result<T> = std::result::Result<T, RepositoryPolicyStoreError>;

/// The hook-trust register, over the shared `agens.db` file.
pub struct RepositoryPolicyStore {
    database_path: PathBuf,
    connection: Connection,
}

impl RepositoryPolicyStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(RepositoryPolicyStoreError::from_database)?;

        Ok(Self {
            database_path,
            connection,
        })
    }

    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.database_path.clone()
    }

    /// What the operator decided about this repository, or `Unknown` when
    /// nobody has decided anything.
    ///
    /// Read through to the database on every call rather than from a cache: the
    /// operator grants trust from a second process, and a daemon answering from
    /// a document it read at start would keep refusing hooks it has been told
    /// to run until somebody restarted it.
    pub fn hook_trust(&self, repo_id: &str) -> Result<StoredHookTrust> {
        let granted: Option<bool> = self
            .connection
            .query_row(
                "SELECT granted FROM repository_hook_trust WHERE repo_id = ?1",
                params![repo_id],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(RepositoryPolicyStoreError::operation(
                    "read hook trust from",
                    &self.database_path,
                    other,
                )),
            })?;

        Ok(match granted {
            Some(true) => StoredHookTrust::Granted,
            Some(false) => StoredHookTrust::Refused,
            None => StoredHookTrust::Unknown,
        })
    }

    /// Records a decision that was made without a question being asked, which
    /// is what the operator's own `trust` verb does.
    pub fn decide(
        &mut self,
        repo_id: &str,
        repository: &Path,
        granted: bool,
        now: i64,
    ) -> Result<()> {
        if repo_id.trim().is_empty() {
            return Err(RepositoryPolicyStoreError::detail(
                "a hook decision names no repository",
            ));
        }

        self.connection
            .execute(
                "INSERT INTO repository_hook_trust (repo_id, repository, granted, decided_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(repo_id) DO UPDATE SET
                     repository = excluded.repository,
                     granted = excluded.granted,
                     decided_at = excluded.decided_at",
                params![repo_id, repository.display().to_string(), granted, now],
            )
            .map_err(|error| {
                RepositoryPolicyStoreError::operation(
                    "record a hook decision in",
                    &self.database_path,
                    error,
                )
            })?;

        Ok(())
    }

    /// Notes that a repository's hooks wait on one durable question, so the
    /// answer can be applied without the question carrying the repository's
    /// identity in its prose.
    pub fn record_pending(&mut self, pending: &StoredPendingTrust) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO repository_hook_questions
                     (question_id, repo_id, repository, asked_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(question_id) DO UPDATE SET
                     repo_id = excluded.repo_id,
                     repository = excluded.repository,
                     asked_at = excluded.asked_at",
                params![
                    pending.question_id,
                    pending.repo_id,
                    pending.repository.display().to_string(),
                    pending.asked_at
                ],
            )
            .map_err(|error| {
                RepositoryPolicyStoreError::operation(
                    "record a pending hook question in",
                    &self.database_path,
                    error,
                )
            })?;

        Ok(())
    }

    /// Whether this question is one whose answer decides a repository's hooks.
    pub fn is_pending(&self, question_id: i64) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT count(*) FROM repository_hook_questions WHERE question_id = ?1",
                params![question_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count > 0)
            .map_err(|error| {
                RepositoryPolicyStoreError::operation(
                    "read a pending hook question from",
                    &self.database_path,
                    error,
                )
            })
    }

    /// Applies an answer to a recorded question, reporting whether that
    /// question was one of them.
    ///
    /// One transaction: a decision written without the question being cleared
    /// would be applied again by the next answer to arrive for it, and a
    /// question cleared without a decision loses the answer entirely.
    pub fn resolve_pending(&mut self, question_id: i64, granted: bool) -> Result<bool> {
        let transaction = self.connection.transaction().map_err(|error| {
            RepositoryPolicyStoreError::operation(
                "resolve a pending hook question in",
                &self.database_path,
                error,
            )
        })?;

        let pending = transaction
            .query_row(
                "SELECT repo_id, repository, asked_at
                 FROM repository_hook_questions WHERE question_id = ?1",
                params![question_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(RepositoryPolicyStoreError::operation(
                    "resolve a pending hook question in",
                    &self.database_path,
                    other,
                )),
            })?;

        let Some((repo_id, repository, asked_at)) = pending else {
            return Ok(false);
        };

        let write = transaction
            .execute(
                "INSERT INTO repository_hook_trust (repo_id, repository, granted, decided_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(repo_id) DO UPDATE SET
                     repository = excluded.repository,
                     granted = excluded.granted,
                     decided_at = excluded.decided_at",
                params![repo_id, repository, granted, asked_at],
            )
            .and_then(|_| {
                transaction.execute(
                    "DELETE FROM repository_hook_questions WHERE question_id = ?1",
                    params![question_id],
                )
            });

        write.map_err(|error| {
            RepositoryPolicyStoreError::operation(
                "resolve a pending hook question in",
                &self.database_path,
                error,
            )
        })?;

        transaction.commit().map_err(|error| {
            RepositoryPolicyStoreError::operation(
                "resolve a pending hook question in",
                &self.database_path,
                error,
            )
        })?;

        Ok(true)
    }
}

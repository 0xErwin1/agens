//! Global prompt history and LIFO stash in the unified `agens.db`.
//!
//! History is chronological by `id` ASC with consecutive-text dedupe on append.
//! Stash is an independent LIFO (highest `id` is the top). Neither store has a
//! product-level row cap. Text only — no attachment columns.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, params};

use crate::database;

/// One durable prompt row from history or stash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPrompt {
    pub id: i64,
    pub text: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptMemoryStoreError {
    message: String,
}

impl PromptMemoryStoreError {
    fn operation(operation: &str, path: &Path, error: impl fmt::Display) -> Self {
        Self {
            message: format!("prompt memory {operation} at {}: {error}", path.display()),
        }
    }

    fn detail(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn from_database(error: database::DatabaseError) -> Self {
        Self::operation(error.operation(), error.path(), error.detail())
    }
}

impl fmt::Display for PromptMemoryStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PromptMemoryStoreError {}

/// Runtime store for global composer history and independent LIFO stash.
///
/// Shares the unified `agens.db` file with sessions, preferences, and grants.
pub struct PromptMemoryStore {
    database_path: PathBuf,
    connection: Connection,
}

impl PromptMemoryStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, PromptMemoryStoreError> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(PromptMemoryStoreError::from_database)?;

        Ok(Self {
            database_path,
            connection,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.database_path.clone()
    }

    /// Chronological history, oldest first (`id` ASC).
    pub fn list_history(&self) -> Result<Vec<StoredPrompt>, PromptMemoryStoreError> {
        self.list_table("prompt_history")
    }

    /// Append `text` unless it equals the newest history row's text.
    ///
    /// Returns `Ok(None)` when skipped as a consecutive duplicate.
    pub fn append_history(
        &mut self,
        text: &str,
    ) -> Result<Option<StoredPrompt>, PromptMemoryStoreError> {
        self.append_history_at(text, None)
    }

    /// Append with an explicit unix-seconds timestamp (used by the TUI persist adapter).
    pub fn append_history_at(
        &mut self,
        text: &str,
        created_at: Option<i64>,
    ) -> Result<Option<StoredPrompt>, PromptMemoryStoreError> {
        validate_prompt_text(text)?;

        let last_text: Option<String> = self
            .connection
            .query_row(
                "SELECT text FROM prompt_history ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| {
                PromptMemoryStoreError::operation(
                    "read last history row",
                    &self.database_path,
                    error,
                )
            })?;

        if last_text.as_deref() == Some(text) {
            return Ok(None);
        }

        match created_at {
            Some(created_at) => {
                self.connection
                    .execute(
                        "INSERT INTO prompt_history (text, created_at) VALUES (?1, ?2)",
                        params![text, created_at],
                    )
                    .map_err(|error| {
                        PromptMemoryStoreError::operation(
                            "append history",
                            &self.database_path,
                            error,
                        )
                    })?;
            }
            None => {
                self.connection
                    .execute(
                        "INSERT INTO prompt_history (text, created_at)
                         VALUES (?1, CAST(strftime('%s','now') AS INTEGER))",
                        params![text],
                    )
                    .map_err(|error| {
                        PromptMemoryStoreError::operation(
                            "append history",
                            &self.database_path,
                            error,
                        )
                    })?;
            }
        }

        let id = self.connection.last_insert_rowid();
        self.load_row("prompt_history", id).map(Some)
    }

    /// Stash ordered oldest-first (`id` ASC); the last element is the LIFO top.
    pub fn list_stash(&self) -> Result<Vec<StoredPrompt>, PromptMemoryStoreError> {
        self.list_table("prompt_stash")
    }

    /// Push onto the LIFO top (append row).
    pub fn push_stash(&mut self, text: &str) -> Result<StoredPrompt, PromptMemoryStoreError> {
        self.push_stash_at(text, None)
    }

    /// Push with an explicit unix-seconds timestamp.
    pub fn push_stash_at(
        &mut self,
        text: &str,
        created_at: Option<i64>,
    ) -> Result<StoredPrompt, PromptMemoryStoreError> {
        validate_prompt_text(text)?;

        match created_at {
            Some(created_at) => {
                self.connection
                    .execute(
                        "INSERT INTO prompt_stash (text, created_at) VALUES (?1, ?2)",
                        params![text, created_at],
                    )
                    .map_err(|error| {
                        PromptMemoryStoreError::operation("push stash", &self.database_path, error)
                    })?;
            }
            None => {
                self.connection
                    .execute(
                        "INSERT INTO prompt_stash (text, created_at)
                         VALUES (?1, CAST(strftime('%s','now') AS INTEGER))",
                        params![text],
                    )
                    .map_err(|error| {
                        PromptMemoryStoreError::operation("push stash", &self.database_path, error)
                    })?;
            }
        }

        let id = self.connection.last_insert_rowid();
        self.load_row("prompt_stash", id)
    }

    /// Pop the LIFO top (highest `id`), or `None` when empty.
    pub fn pop_stash(&mut self) -> Result<Option<StoredPrompt>, PromptMemoryStoreError> {
        let top = self
            .connection
            .query_row(
                "SELECT id, text, created_at FROM prompt_stash ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok(StoredPrompt {
                        id: row.get(0)?,
                        text: row.get(1)?,
                        created_at: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|error| {
                PromptMemoryStoreError::operation("read stash top", &self.database_path, error)
            })?;

        let Some(entry) = top else {
            return Ok(None);
        };

        self.connection
            .execute("DELETE FROM prompt_stash WHERE id = ?1", params![entry.id])
            .map_err(|error| {
                PromptMemoryStoreError::operation("pop stash", &self.database_path, error)
            })?;

        Ok(Some(entry))
    }

    /// Remove by index into the current oldest-first list (`0` = oldest).
    pub fn remove_stash_at(
        &mut self,
        index: usize,
    ) -> Result<Option<StoredPrompt>, PromptMemoryStoreError> {
        let entries = self.list_stash()?;
        let Some(entry) = entries.get(index).cloned() else {
            return Ok(None);
        };

        self.connection
            .execute("DELETE FROM prompt_stash WHERE id = ?1", params![entry.id])
            .map_err(|error| {
                PromptMemoryStoreError::operation("remove stash", &self.database_path, error)
            })?;

        Ok(Some(entry))
    }

    /// Replace the entire stash stack in one transaction (order = oldest first).
    pub fn replace_stash(
        &mut self,
        entries: &[(String, i64)],
    ) -> Result<(), PromptMemoryStoreError> {
        for (text, _) in entries {
            validate_prompt_text(text)?;
        }

        let transaction = self.connection.transaction().map_err(|error| {
            PromptMemoryStoreError::operation("start stash rewrite", &self.database_path, error)
        })?;

        transaction
            .execute("DELETE FROM prompt_stash", [])
            .map_err(|error| {
                PromptMemoryStoreError::operation("clear stash", &self.database_path, error)
            })?;

        for (text, created_at) in entries {
            transaction
                .execute(
                    "INSERT INTO prompt_stash (text, created_at) VALUES (?1, ?2)",
                    params![text, created_at],
                )
                .map_err(|error| {
                    PromptMemoryStoreError::operation("rewrite stash", &self.database_path, error)
                })?;
        }

        transaction.commit().map_err(|error| {
            PromptMemoryStoreError::operation("commit stash rewrite", &self.database_path, error)
        })?;

        Ok(())
    }

    fn list_table(&self, table: &str) -> Result<Vec<StoredPrompt>, PromptMemoryStoreError> {
        // Table names are crate-private literals only.
        let sql = format!("SELECT id, text, created_at FROM {table} ORDER BY id ASC");
        let mut statement = self.connection.prepare(&sql).map_err(|error| {
            PromptMemoryStoreError::operation(
                &format!("prepare {table} list"),
                &self.database_path,
                error,
            )
        })?;

        let rows = statement
            .query_map([], |row| {
                Ok(StoredPrompt {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })
            .map_err(|error| {
                PromptMemoryStoreError::operation(
                    &format!("query {table} list"),
                    &self.database_path,
                    error,
                )
            })?;

        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|error| {
            PromptMemoryStoreError::operation(
                &format!("read {table} list"),
                &self.database_path,
                error,
            )
        })
    }

    fn load_row(&self, table: &str, id: i64) -> Result<StoredPrompt, PromptMemoryStoreError> {
        let sql = format!("SELECT id, text, created_at FROM {table} WHERE id = ?1");
        self.connection
            .query_row(&sql, params![id], |row| {
                Ok(StoredPrompt {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    created_at: row.get(2)?,
                })
            })
            .map_err(|error| {
                PromptMemoryStoreError::operation(
                    &format!("load {table} row"),
                    &self.database_path,
                    error,
                )
            })
    }
}

fn validate_prompt_text(text: &str) -> Result<(), PromptMemoryStoreError> {
    if text.is_empty() {
        return Err(PromptMemoryStoreError::detail("prompt text is required"));
    }
    Ok(())
}

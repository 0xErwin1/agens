//! SQLite-backed [`PromptMemory`] for the unified `agens.db`.
//!
//! Domain browse/dedupe/overlay logic lives in [`PromptMemoryState`]. This
//! adapter loads tables on open and keeps SQLite consistent with in-memory
//! state: durable mutators write SQLite first, then update state; on SQLite
//! failure state is left unchanged. Browse ops are memory-only.
//!
//! Attachments persist as JSON text of `[media_id, mime]` pairs (durable media
//! ids only, never source paths); `NULL` marks a text-only row, so entries
//! recorded before the media migration keep loading with no attachments.

use std::{
    fmt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use agens_core::{
    HistoryBrowseResult, PromptAttachment, PromptMemory, PromptMemoryEntry, PromptMemoryError,
    PromptMemoryState, PromptOverlayItem, PromptRecall,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::database;

/// One durable prompt row from history or stash (includes SQLite id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPrompt {
    pub id: i64,
    pub text: String,
    pub attachments: Vec<PromptAttachment>,
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

impl From<PromptMemoryStoreError> for PromptMemoryError {
    fn from(error: PromptMemoryStoreError) -> Self {
        PromptMemoryError::new(error.to_string())
    }
}

/// Runtime store for global composer history and independent LIFO stash.
///
/// Shares the unified `agens.db` file with sessions, preferences, and grants.
/// Holds an in-memory [`PromptMemoryState`] mirror loaded at open.
pub struct PromptMemoryStore {
    database_path: PathBuf,
    connection: Connection,
    state: PromptMemoryState,
}

impl PromptMemoryStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, PromptMemoryStoreError> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(PromptMemoryStoreError::from_database)?;

        let mut store = Self {
            database_path,
            connection,
            state: PromptMemoryState::new(),
        };
        store.reload_state_from_db()?;
        Ok(store)
    }

    pub fn database_path(&self) -> PathBuf {
        self.database_path.clone()
    }

    pub fn state(&self) -> &PromptMemoryState {
        &self.state
    }

    /// Chronological history, oldest first (`id` ASC).
    pub fn list_history(&self) -> Result<Vec<StoredPrompt>, PromptMemoryStoreError> {
        self.list_table("prompt_history")
    }

    /// Append an entry unless it duplicates the newest history row.
    ///
    /// A duplicate means the same text AND the same attachments; the same text
    /// with different media is a distinct prompt and is recorded.
    /// Returns `Ok(None)` when skipped as a consecutive duplicate.
    pub fn append_history(
        &mut self,
        text: &str,
        attachments: &[PromptAttachment],
    ) -> Result<Option<StoredPrompt>, PromptMemoryStoreError> {
        validate_prompt_entry(text, attachments)?;

        if self
            .state
            .history()
            .last()
            .is_some_and(|entry| entry.text == text && entry.attachments == attachments)
        {
            self.state.clear_browse();
            return Ok(None);
        }

        let created_at = unix_now_secs();
        self.connection
            .execute(
                "INSERT INTO prompt_history (text, created_at, attachments) VALUES (?1, ?2, ?3)",
                params![text, created_at, encode_attachments(attachments)?],
            )
            .map_err(|error| {
                PromptMemoryStoreError::operation("append history", &self.database_path, error)
            })?;

        let id = self.connection.last_insert_rowid();
        let recorded = self
            .state
            .record_submission_at(text, attachments, created_at);
        debug_assert!(recorded);

        self.load_row("prompt_history", id).map(Some)
    }

    /// Stash ordered oldest-first (`id` ASC); the last element is the LIFO top.
    pub fn list_stash(&self) -> Result<Vec<StoredPrompt>, PromptMemoryStoreError> {
        self.list_table("prompt_stash")
    }

    /// Push onto the LIFO top (append row).
    pub fn push_stash(
        &mut self,
        text: &str,
        attachments: &[PromptAttachment],
    ) -> Result<StoredPrompt, PromptMemoryStoreError> {
        validate_prompt_entry(text, attachments)?;

        let created_at = unix_now_secs();
        self.connection
            .execute(
                "INSERT INTO prompt_stash (text, created_at, attachments) VALUES (?1, ?2, ?3)",
                params![text, created_at, encode_attachments(attachments)?],
            )
            .map_err(|error| {
                PromptMemoryStoreError::operation("push stash", &self.database_path, error)
            })?;

        let id = self.connection.last_insert_rowid();
        self.state.stash_push_at(text, attachments, created_at);
        self.load_row("prompt_stash", id)
    }

    /// Pop the LIFO top (highest `id`), or `None` when empty.
    pub fn pop_stash(&mut self) -> Result<Option<StoredPrompt>, PromptMemoryStoreError> {
        let top = self
            .connection
            .query_row(
                "SELECT id, text, created_at, attachments FROM prompt_stash ORDER BY id DESC LIMIT 1",
                [],
                stored_prompt_from_row,
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

        let _ = self.state.stash_pop();
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

        let _ = self.state.stash_remove_at(index);
        Ok(Some(entry))
    }

    /// Replace the entire stash stack in one transaction (order = oldest first).
    pub fn replace_stash(
        &mut self,
        entries: &[PromptMemoryEntry],
    ) -> Result<(), PromptMemoryStoreError> {
        for entry in entries {
            validate_prompt_entry(&entry.text, &entry.attachments)?;
        }

        let transaction = self.connection.transaction().map_err(|error| {
            PromptMemoryStoreError::operation("start stash rewrite", &self.database_path, error)
        })?;

        transaction
            .execute("DELETE FROM prompt_stash", [])
            .map_err(|error| {
                PromptMemoryStoreError::operation("clear stash", &self.database_path, error)
            })?;

        for entry in entries {
            transaction
                .execute(
                    "INSERT INTO prompt_stash (text, created_at, attachments) VALUES (?1, ?2, ?3)",
                    params![
                        entry.text,
                        entry.created_at,
                        encode_attachments(&entry.attachments)?
                    ],
                )
                .map_err(|error| {
                    PromptMemoryStoreError::operation("rewrite stash", &self.database_path, error)
                })?;
        }

        transaction.commit().map_err(|error| {
            PromptMemoryStoreError::operation("commit stash rewrite", &self.database_path, error)
        })?;

        self.state.seed_stash(entries.iter().cloned());
        Ok(())
    }

    fn reload_state_from_db(&mut self) -> Result<(), PromptMemoryStoreError> {
        let history = self.list_history()?;
        let stash = self.list_stash()?;

        self.state
            .seed_history(history.into_iter().map(stored_prompt_into_entry));
        self.state
            .seed_stash(stash.into_iter().map(stored_prompt_into_entry));
        Ok(())
    }

    fn list_table(&self, table: &str) -> Result<Vec<StoredPrompt>, PromptMemoryStoreError> {
        // Table names are crate-private literals only.
        let sql = format!("SELECT id, text, created_at, attachments FROM {table} ORDER BY id ASC");
        let mut statement = self.connection.prepare(&sql).map_err(|error| {
            PromptMemoryStoreError::operation(
                &format!("prepare {table} list"),
                &self.database_path,
                error,
            )
        })?;

        let rows = statement
            .query_map([], stored_prompt_from_row)
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
        let sql = format!("SELECT id, text, created_at, attachments FROM {table} WHERE id = ?1");
        self.connection
            .query_row(&sql, params![id], stored_prompt_from_row)
            .map_err(|error| {
                PromptMemoryStoreError::operation(
                    &format!("load {table} row"),
                    &self.database_path,
                    error,
                )
            })
    }
}

impl PromptMemory for PromptMemoryStore {
    fn record_submission(
        &mut self,
        text: &str,
        attachments: &[PromptAttachment],
    ) -> Result<bool, PromptMemoryError> {
        self.append_history(text, attachments)
            .map(|row| row.is_some())
            .map_err(PromptMemoryError::from)
    }

    fn browse_up(
        &mut self,
        composer_input: &str,
        staged_attachments: &[PromptAttachment],
    ) -> Option<PromptRecall> {
        self.state.browse_up(composer_input, staged_attachments)
    }

    fn browse_down(&mut self) -> HistoryBrowseResult {
        self.state.browse_down()
    }

    fn clear_browse(&mut self) {
        self.state.clear_browse();
    }

    fn is_browsing(&self) -> bool {
        self.state.is_browsing()
    }

    fn stash_push(
        &mut self,
        text: &str,
        attachments: &[PromptAttachment],
    ) -> Result<bool, PromptMemoryError> {
        self.push_stash(text, attachments)
            .map(|_| true)
            .map_err(PromptMemoryError::from)
    }

    fn stash_pop(&mut self) -> Result<Option<PromptRecall>, PromptMemoryError> {
        self.pop_stash()
            .map(|row| {
                row.map(|entry| PromptRecall {
                    text: entry.text,
                    attachments: entry.attachments,
                })
            })
            .map_err(PromptMemoryError::from)
    }

    fn stash_remove_at(&mut self, index: usize) -> Result<bool, PromptMemoryError> {
        self.remove_stash_at(index)
            .map(|row| row.is_some())
            .map_err(PromptMemoryError::from)
    }

    fn history_overlay(&self, query: &str, limit: usize) -> Vec<PromptOverlayItem> {
        self.state.history_overlay(query, limit)
    }

    fn stash_overlay(&self, query: &str, limit: usize) -> Vec<PromptOverlayItem> {
        self.state.stash_overlay(query, limit)
    }
}

fn stored_prompt_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredPrompt> {
    Ok(StoredPrompt {
        id: row.get(0)?,
        text: row.get(1)?,
        created_at: row.get(2)?,
        attachments: decode_attachments(row.get::<_, Option<String>>(3)?.as_deref())?,
    })
}

fn stored_prompt_into_entry(row: StoredPrompt) -> PromptMemoryEntry {
    PromptMemoryEntry::with_created_at(row.text, row.created_at).with_attachments(row.attachments)
}

fn validate_prompt_entry(
    text: &str,
    attachments: &[PromptAttachment],
) -> Result<(), PromptMemoryStoreError> {
    if text.is_empty() && attachments.is_empty() {
        return Err(PromptMemoryStoreError::detail(
            "prompt text or attachments are required",
        ));
    }
    Ok(())
}

fn encode_attachments(
    attachments: &[PromptAttachment],
) -> Result<Option<String>, PromptMemoryStoreError> {
    if attachments.is_empty() {
        return Ok(None);
    }

    let pairs: Vec<(i64, &str)> = attachments
        .iter()
        .map(|attachment| (attachment.media_id, attachment.mime.as_str()))
        .collect();
    serde_json::to_string(&pairs)
        .map(Some)
        .map_err(|error| PromptMemoryStoreError::detail(format!("encode attachments: {error}")))
}

fn decode_attachments(value: Option<&str>) -> rusqlite::Result<Vec<PromptAttachment>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };

    let pairs: Vec<(i64, String)> = serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(pairs
        .into_iter()
        .map(|(media_id, mime)| PromptAttachment::new(media_id, mime))
        .collect())
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

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
    HistoryBrowseResult, MessagePart, PromptAttachment, PromptMemory, PromptMemoryEntry,
    PromptMemoryError, PromptMemoryState, PromptOverlayItem, PromptRecall,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::database;

/// One durable prompt row from history or stash (includes SQLite id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPrompt {
    pub id: i64,
    pub text: String,
    pub attachments: Vec<PromptAttachment>,
    pub parts: Vec<MessagePart>,
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
    undecodable_attachment_rows: usize,
}

impl PromptMemoryStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, PromptMemoryStoreError> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(PromptMemoryStoreError::from_database)?;

        let mut store = Self {
            database_path,
            connection,
            state: PromptMemoryState::new(),
            undecodable_attachment_rows: 0,
        };
        store.reload_state_from_db()?;
        Ok(store)
    }

    /// How many loaded rows carried an `attachments` column that could not be decoded.
    ///
    /// Those rows are usable as text-only prompts; the count is what a surface reports so the
    /// silent loss of their attachments is visible.
    pub fn undecodable_attachment_rows(&self) -> usize {
        self.undecodable_attachment_rows
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
                params![
                    text,
                    created_at,
                    encode_parts(&legacy_parts(text, attachments))?
                ],
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

    /// Appends canonical ordered Text/Media content.
    pub fn append_history_parts(
        &mut self,
        parts: &[MessagePart],
    ) -> Result<Option<StoredPrompt>, PromptMemoryStoreError> {
        validate_parts(parts)?;
        let text = parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let attachments = attachments_from_parts(parts);
        let created_at = unix_now_secs();
        let encoded_parts = encode_parts(parts)?;
        self.connection
            .execute(
                "INSERT INTO prompt_history (text, created_at, attachments) VALUES (?1, ?2, ?3)",
                params![text, created_at, encoded_parts],
            )
            .map_err(|error| {
                PromptMemoryStoreError::operation("append history", &self.database_path, error)
            })?;
        let id = self.connection.last_insert_rowid();
        if !self
            .state
            .record_submission_at(&text, &attachments, created_at)
        {
            // Ordered parts can be durably distinct while sharing the legacy text/media
            // projection used for deduplication. Reload so the mirror retains the row.
            self.reload_state_from_db()?;
        }
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
                params![
                    text,
                    created_at,
                    encode_parts(&legacy_parts(text, attachments))?
                ],
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
                    params![entry.text, entry.created_at, encode_parts(&entry.parts)?],
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
        let (history, undecodable_history) =
            self.list_table_with_decode_status("prompt_history")?;
        let (stash, undecodable_stash) = self.list_table_with_decode_status("prompt_stash")?;

        self.undecodable_attachment_rows = undecodable_history + undecodable_stash;
        self.state
            .seed_history(history.into_iter().map(stored_prompt_into_entry));
        self.state
            .seed_stash(stash.into_iter().map(stored_prompt_into_entry));
        Ok(())
    }

    fn list_table(&self, table: &str) -> Result<Vec<StoredPrompt>, PromptMemoryStoreError> {
        self.list_table_with_decode_status(table)
            .map(|(entries, _)| entries)
    }

    /// Lists a table and counts the rows whose attachments column could not be decoded.
    fn list_table_with_decode_status(
        &self,
        table: &str,
    ) -> Result<(Vec<StoredPrompt>, usize), PromptMemoryStoreError> {
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
            .query_map([], stored_prompt_with_decode_status)
            .map_err(|error| {
                PromptMemoryStoreError::operation(
                    &format!("query {table} list"),
                    &self.database_path,
                    error,
                )
            })?;

        let rows = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| {
                PromptMemoryStoreError::operation(
                    &format!("read {table} list"),
                    &self.database_path,
                    error,
                )
            })?;

        let undecodable = rows.iter().filter(|(_, undecodable)| *undecodable).count();
        Ok((
            rows.into_iter().map(|(entry, _)| entry).collect(),
            undecodable,
        ))
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

    fn record_submission_parts(
        &mut self,
        parts: &[MessagePart],
    ) -> Result<bool, PromptMemoryError> {
        self.append_history_parts(parts)
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

    fn stash_push_parts(&mut self, parts: &[MessagePart]) -> Result<bool, PromptMemoryError> {
        validate_parts(parts).map_err(PromptMemoryError::from)?;
        let text = parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let created_at = unix_now_secs();
        self.connection
            .execute(
                "INSERT INTO prompt_stash (text, created_at, attachments) VALUES (?1, ?2, ?3)",
                params![
                    text,
                    created_at,
                    encode_parts(parts).map_err(PromptMemoryError::from)?
                ],
            )
            .map_err(|error| {
                PromptMemoryError::from(PromptMemoryStoreError::operation(
                    "push stash",
                    &self.database_path,
                    error,
                ))
            })?;
        self.state.stash_push_parts_at(parts.to_vec(), created_at);
        Ok(true)
    }

    fn stash_pop(&mut self) -> Result<Option<PromptRecall>, PromptMemoryError> {
        self.pop_stash()
            .map(|row| {
                row.map(|entry| PromptRecall {
                    text: entry.text,
                    attachments: entry.attachments,
                    parts: entry.parts,
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
    stored_prompt_with_decode_status(row).map(|(entry, _)| entry)
}

/// Reads a row, reporting whether its `attachments` column had to be given up on.
fn stored_prompt_with_decode_status(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(StoredPrompt, bool)> {
    let raw: Option<String> = row.get(3)?;
    let text: String = row.get(1)?;
    let decoded = decode_parts(raw.as_deref(), &text);
    let undecodable = decoded.is_none();
    let parts = decoded.unwrap_or_else(|| legacy_parts(&text, &[]));
    let attachments = attachments_from_parts(&parts);

    Ok((
        StoredPrompt {
            id: row.get(0)?,
            text,
            created_at: row.get(2)?,
            attachments,
            parts,
        },
        undecodable,
    ))
}

fn stored_prompt_into_entry(row: StoredPrompt) -> PromptMemoryEntry {
    PromptMemoryEntry::with_created_at(row.text, row.created_at)
        .with_attachments(row.attachments)
        .with_parts(row.parts)
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

fn validate_parts(parts: &[MessagePart]) -> Result<(), PromptMemoryStoreError> {
    if parts.is_empty()
        || parts.iter().any(|part| match part {
            MessagePart::Text(text) => text.is_empty(),
            MessagePart::Media { media_id, mime } => *media_id <= 0 || mime.is_empty(),
            _ => true,
        })
    {
        return Err(PromptMemoryStoreError::detail(
            "prompt parts must be non-empty Text or Media",
        ));
    }
    Ok(())
}

fn legacy_parts(text: &str, attachments: &[PromptAttachment]) -> Vec<MessagePart> {
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(MessagePart::Text(text.to_owned()));
    }
    parts.extend(attachments.iter().map(|attachment| MessagePart::Media {
        media_id: attachment.media_id,
        mime: attachment.mime.clone(),
    }));
    parts
}

fn attachments_from_parts(parts: &[MessagePart]) -> Vec<PromptAttachment> {
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Media { media_id, mime } => {
                Some(PromptAttachment::new(*media_id, mime.clone()))
            }
            _ => None,
        })
        .collect()
}

fn encode_parts(parts: &[MessagePart]) -> Result<Option<String>, PromptMemoryStoreError> {
    let encoded = parts
        .iter()
        .map(|part| match part {
            MessagePart::Text(text) => serde_json::json!({"text": text}),
            MessagePart::Media { media_id, mime } => {
                serde_json::json!({"media": [media_id, mime]})
            }
            _ => unreachable!("validated prompt parts are Text or Media"),
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&serde_json::json!([1, encoded]))
        .map(Some)
        .map_err(|error| PromptMemoryStoreError::detail(format!("encode prompt parts: {error}")))
}

/// Decodes version 1 ordered parts or legacy `[media_id, mime]` pairs.
fn decode_parts(value: Option<&str>, text: &str) -> Option<Vec<MessagePart>> {
    let Some(value) = value else {
        return Some(legacy_parts(text, &[]));
    };
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    let array = value.as_array()?;
    if array.first().and_then(serde_json::Value::as_i64) == Some(1) {
        let encoded = array.get(1)?.as_array()?;
        let mut parts = Vec::with_capacity(encoded.len());
        for value in encoded {
            if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) {
                parts.push(MessagePart::Text(text.to_owned()));
            } else {
                let media = value.get("media").and_then(serde_json::Value::as_array)?;
                let media_id = media.first()?.as_i64()?;
                let mime = media.get(1)?.as_str()?.to_owned();
                parts.push(MessagePart::Media { media_id, mime });
            }
        }
        validate_parts(&parts).ok()?;
        return Some(parts);
    }

    let pairs: Vec<(i64, String)> = serde_json::from_value(value).ok()?;
    let attachments = pairs
        .into_iter()
        .map(|(media_id, mime)| PromptAttachment::new(media_id, mime))
        .collect::<Vec<_>>();
    Some(legacy_parts(text, &attachments))
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

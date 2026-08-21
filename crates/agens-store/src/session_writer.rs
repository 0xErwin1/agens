use agens_core::{
    AttemptFinishOutcome, AttemptKey, BeginSessionAttemptError, CompletedSessionTurn,
    MAX_RETRY_PROMPT_BYTES, Message, MessagePart, ReasoningEffort, RecoveryOutcome, RequestConfig,
    RetryBoundary, Role, SessionAttemptFailureKind, SessionAttemptStatus, SessionAttemptSummary,
    SessionMetadata,
};
use std::path::PathBuf;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::{SessionStore, SessionStoreError};

type PersistedPart = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<bool>,
    Option<i64>,
    Option<String>,
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSession {
    pub metadata: SessionMetadata,
    pub messages: Vec<Message>,
    pub latest_attempt: Option<SessionAttemptSummary>,
}

/// Why a fork was refused. The two rejections are kept apart from a storage failure because a
/// surface answers them differently: an unknown source and a cut point outside the source's
/// history are both things the caller asked for and can correct, not something that went wrong
/// underneath. Neither is clamped into a fork of whatever happened to be there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForkSessionError {
    UnknownSession(i64),
    PrefixOutOfRange { requested: i64, available: i64 },
    Store(SessionStoreError),
}

impl std::fmt::Display for ForkSessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSession(id) => write!(formatter, "unknown session {id}"),
            Self::PrefixOutOfRange {
                requested,
                available,
            } => write!(
                formatter,
                "message prefix {requested} is outside the {available} messages the session holds"
            ),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ForkSessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::UnknownSession(_) | Self::PrefixOutOfRange { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPage {
    pub sessions: Vec<StoredSession>,
    pub next_cursor: Option<SessionCursor>,
}

const MAX_TRANSCRIPT_PAGE_SIZE: usize = 200;

/// The session columns [`session_metadata`] reads by position, followed by the six attempt columns
/// [`attempt_summary_from_row`] reads at offset [`LATEST_ATTEMPT_COLUMN_OFFSET`]. Shared by every
/// listing that returns a [`StoredSession`] so the two readers can keep indexing one column order.
const SESSION_WITH_LATEST_ATTEMPT_SELECT: &str =
    "SELECT sessions.id, sessions.project, sessions.title, sessions.active_agent,
                    sessions.created_at, sessions.updated_at, sessions.completed_turn_count,
                    sessions.resumable, sessions.provider_id, sessions.model_id,
                    sessions.reasoning_effort, sessions.parent_session_id,
                    sessions.fork_message_count, latest.id, latest.sequence, latest.status,
                    latest.failure_kind, latest.started_at, latest.finished_at
             FROM sessions
             LEFT JOIN session_attempts AS latest
               ON latest.session_id = sessions.id
              AND latest.sequence = (
                  SELECT MAX(candidate.sequence)
                  FROM session_attempts AS candidate
                  WHERE candidate.session_id = sessions.id
              )";

const LATEST_ATTEMPT_COLUMN_OFFSET: usize = 13;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptPage {
    pub messages: Vec<Message>,
    pub next_cursor: Option<TranscriptCursor>,
}

/// The sequence of the last message already returned; the next page starts
/// strictly after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TranscriptCursor {
    after_sequence: i64,
}

impl TranscriptCursor {
    pub const fn new(after_sequence: i64) -> Self {
        Self { after_sequence }
    }

    pub const fn after_sequence(self) -> i64 {
        self.after_sequence
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionCursor {
    updated_at: i64,
    id: i64,
}

impl SessionCursor {
    pub const fn new(updated_at: i64, id: i64) -> Self {
        Self { updated_at, id }
    }

    pub const fn updated_at(self) -> i64 {
        self.updated_at
    }

    pub const fn id(self) -> i64 {
        self.id
    }
}

impl SessionStore {
    pub fn begin_session_attempt(
        &mut self,
        metadata: &SessionMetadata,
        retry_prompt: String,
    ) -> Result<SessionAttemptSummary, BeginSessionAttemptError> {
        self.begin_session_attempt_with_media(metadata, retry_prompt, Vec::new())
    }

    pub fn begin_session_attempt_with_media(
        &mut self,
        metadata: &SessionMetadata,
        retry_prompt: String,
        media_ids: Vec<i64>,
    ) -> Result<SessionAttemptSummary, BeginSessionAttemptError> {
        if retry_prompt.len() > MAX_RETRY_PROMPT_BYTES {
            return Err(BeginSessionAttemptError::Store);
        }
        // Media-only turns may begin with an empty prompt when media_ids is non-empty.
        if retry_prompt.is_empty() && media_ids.is_empty() {
            return Err(BeginSessionAttemptError::Store);
        }
        validate_attempt_metadata(metadata).map_err(|_| BeginSessionAttemptError::Store)?;
        let retry_media_ids = encode_retry_media_ids(&media_ids);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| BeginSessionAttemptError::Store)?;
        let session_id = insert_attempt_session(&transaction, &self.database_path, metadata)
            .map_err(|_| BeginSessionAttemptError::Store)?;
        let running = transaction
            .query_row(
                "SELECT id, sequence, started_at FROM session_attempts WHERE session_id = ?1 AND status = 'running'",
                [session_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
            )
            .optional()
            .map_err(|_| BeginSessionAttemptError::Store)?;
        if let Some((id, sequence, started_at)) = running {
            let summary = SessionAttemptSummary::new(
                AttemptKey::new(session_id, id).map_err(|_| BeginSessionAttemptError::Store)?,
                sequence
                    .try_into()
                    .map_err(|_| BeginSessionAttemptError::Store)?,
                SessionAttemptStatus::Running,
                None,
                started_at,
                None,
            )
            .map_err(|_| BeginSessionAttemptError::Store)?;
            return Err(BeginSessionAttemptError::AlreadyRunning(summary));
        }
        transaction
            .execute(
                "UPDATE session_attempts SET retry_prompt = NULL, retry_media_ids = NULL
                 WHERE session_id = ?1 AND retry_prompt IS NOT NULL",
                [session_id],
            )
            .map_err(|_| BeginSessionAttemptError::Store)?;
        let sequence = next_sequence(
            &transaction,
            &self.database_path,
            "session_attempts",
            session_id,
        )
        .map_err(|_| BeginSessionAttemptError::Store)?;
        transaction
            .execute(
                "INSERT INTO session_attempts(session_id, sequence, status, retry_prompt, retry_media_ids, started_at)
                 VALUES (?1, ?2, 'running', ?3, ?4, ?5)",
                params![
                    session_id,
                    sequence,
                    retry_prompt,
                    retry_media_ids,
                    metadata.updated_at
                ],
            )
            .map_err(|_| BeginSessionAttemptError::Store)?;
        let key = AttemptKey::new(session_id, transaction.last_insert_rowid())
            .map_err(|_| BeginSessionAttemptError::Store)?;
        let summary = SessionAttemptSummary::new(
            key,
            sequence
                .try_into()
                .map_err(|_| BeginSessionAttemptError::Store)?,
            SessionAttemptStatus::Running,
            None,
            metadata.updated_at,
            None,
        )
        .map_err(|_| BeginSessionAttemptError::Store)?;
        transaction
            .commit()
            .map_err(|_| BeginSessionAttemptError::Store)?;
        Ok(summary)
    }

    pub fn finish_session_attempt(
        &mut self,
        key: AttemptKey,
        status: SessionAttemptStatus,
        finished_at: i64,
    ) -> Result<AttemptFinishOutcome, SessionStoreError> {
        let Some(failure_kind) = status.expected_failure_kind() else {
            return Err(SessionStoreError::operation(
                "finish session attempt",
                &self.database_path,
                "completed attempts require completed history",
            ));
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation(
                    "start session attempt finish",
                    &self.database_path,
                    error,
                )
            })?;
        let changed = transaction
            .execute(
                "UPDATE session_attempts SET status = ?1, failure_kind = ?2, finished_at = ?3
             WHERE id = ?4 AND session_id = ?5 AND status = 'running'",
                params![
                    attempt_status(status),
                    attempt_failure_kind(failure_kind),
                    finished_at,
                    key.attempt_id(),
                    key.session_id()
                ],
            )
            .map_err(|error| {
                SessionStoreError::operation("finish session attempt", &self.database_path, error)
            })?;
        if changed == 1 {
            transaction
                .execute(
                    "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                    params![finished_at, key.session_id()],
                )
                .map_err(|error| {
                    SessionStoreError::operation(
                        "update session attempt",
                        &self.database_path,
                        error,
                    )
                })?;
        }
        transaction.commit().map_err(|error| {
            SessionStoreError::operation(
                "commit session attempt finish",
                &self.database_path,
                error,
            )
        })?;
        Ok(if changed == 1 {
            AttemptFinishOutcome::Finished
        } else {
            AttemptFinishOutcome::Stale
        })
    }

    pub fn persist_completed_session_attempt(
        &mut self,
        key: AttemptKey,
        metadata: &SessionMetadata,
        turn: &CompletedSessionTurn,
        finished_at: i64,
    ) -> Result<AttemptFinishOutcome, SessionStoreError> {
        if metadata.id != key.session_id() {
            return Err(SessionStoreError::operation(
                "complete session attempt",
                &self.database_path,
                "attempt session does not match metadata",
            ));
        }
        validate_metadata(metadata, &self.database_path)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation(
                    "start completed session attempt",
                    &self.database_path,
                    error,
                )
            })?;
        let running = transaction
            .query_row(
                "SELECT 1 FROM session_attempts WHERE id = ?1 AND session_id = ?2 AND status = 'running'",
                params![key.attempt_id(), key.session_id()],
                |_| Ok(()),
            )
            .optional()
            .map_err(|error| {
                SessionStoreError::operation("check completed session attempt", &self.database_path, error)
            })?;
        if running.is_none() {
            transaction.commit().map_err(|error| {
                SessionStoreError::operation(
                    "commit stale session attempt",
                    &self.database_path,
                    error,
                )
            })?;
            return Ok(AttemptFinishOutcome::Stale);
        }
        let completed_turn_sequence = persist_completed_turn_in_transaction(
            &transaction,
            &self.database_path,
            metadata,
            turn,
            finished_at,
        )?
        .sequence;
        let changed = transaction
            .execute(
                "UPDATE session_attempts
                 SET status = 'completed', retry_prompt = NULL, retry_media_ids = NULL, finished_at = ?1, completed_turn_sequence = ?2
                 WHERE id = ?3 AND session_id = ?4 AND status = 'running'",
                params![finished_at, completed_turn_sequence, key.attempt_id(), key.session_id()],
            )
            .map_err(|error| {
                SessionStoreError::operation("complete session attempt", &self.database_path, error)
            })?;
        if changed != 1 {
            return Err(SessionStoreError::operation(
                "complete session attempt",
                &self.database_path,
                "running attempt changed during completion",
            ));
        }
        transaction.commit().map_err(|error| {
            SessionStoreError::operation(
                "commit completed session attempt",
                &self.database_path,
                error,
            )
        })?;
        Ok(AttemptFinishOutcome::Finished)
    }

    /// Persists the content an unsuccessful attempt produced and marks the attempt with its
    /// terminal failure status in one immediate transaction, so a sub-agent turn persisted into
    /// the same session cannot interleave with this history. The retained retry prompt is dropped
    /// because the prompt now lives in the persisted history.
    pub fn persist_partial_session_attempt(
        &mut self,
        key: AttemptKey,
        metadata: &SessionMetadata,
        turn: &CompletedSessionTurn,
        status: SessionAttemptStatus,
        finished_at: i64,
    ) -> Result<AttemptFinishOutcome, SessionStoreError> {
        let Some(failure_kind) = status.expected_failure_kind() else {
            return Err(SessionStoreError::operation(
                "persist partial session attempt",
                &self.database_path,
                "partial attempts require a failure status",
            ));
        };
        if metadata.id != key.session_id() {
            return Err(SessionStoreError::operation(
                "persist partial session attempt",
                &self.database_path,
                "attempt session does not match metadata",
            ));
        }
        validate_metadata(metadata, &self.database_path)?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation(
                    "start partial session attempt",
                    &self.database_path,
                    error,
                )
            })?;
        let running = transaction
            .query_row(
                "SELECT 1 FROM session_attempts WHERE id = ?1 AND session_id = ?2 AND status = 'running'",
                params![key.attempt_id(), key.session_id()],
                |_| Ok(()),
            )
            .optional()
            .map_err(|error| {
                SessionStoreError::operation("check partial session attempt", &self.database_path, error)
            })?;
        if running.is_none() {
            transaction.commit().map_err(|error| {
                SessionStoreError::operation(
                    "commit stale partial session attempt",
                    &self.database_path,
                    error,
                )
            })?;
            return Ok(AttemptFinishOutcome::Stale);
        }

        persist_completed_turn_in_transaction(
            &transaction,
            &self.database_path,
            metadata,
            turn,
            finished_at,
        )?;
        let changed = transaction
            .execute(
                "UPDATE session_attempts
                 SET status = ?1, failure_kind = ?2, retry_prompt = NULL, retry_media_ids = NULL, finished_at = ?3
                 WHERE id = ?4 AND session_id = ?5 AND status = 'running'",
                params![
                    attempt_status(status),
                    attempt_failure_kind(failure_kind),
                    finished_at,
                    key.attempt_id(),
                    key.session_id()
                ],
            )
            .map_err(|error| {
                SessionStoreError::operation(
                    "persist partial session attempt",
                    &self.database_path,
                    error,
                )
            })?;
        if changed != 1 {
            return Err(SessionStoreError::operation(
                "persist partial session attempt",
                &self.database_path,
                "running attempt changed during persistence",
            ));
        }

        transaction.commit().map_err(|error| {
            SessionStoreError::operation(
                "commit partial session attempt",
                &self.database_path,
                error,
            )
        })?;
        Ok(AttemptFinishOutcome::Finished)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionMetadata>, SessionStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, project, title, active_agent, created_at, updated_at, completed_turn_count, resumable,
                        provider_id, model_id, reasoning_effort, parent_session_id, fork_message_count
                 FROM sessions WHERE resumable = 1 ORDER BY updated_at DESC, id DESC",
            )
            .map_err(|error| SessionStoreError::operation("prepare session list", &self.database_path, error))?;
        let sessions = statement
            .query_map([], session_metadata)
            .map_err(|error| {
                SessionStoreError::operation("query session list", &self.database_path, error)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| {
                SessionStoreError::operation("read session list", &self.database_path, error)
            })?;

        Ok(sessions)
    }

    pub fn list_session_page(
        &self,
        project: Option<&str>,
        query: &str,
        cursor: Option<SessionCursor>,
        page_size: usize,
    ) -> Result<SessionPage, SessionStoreError> {
        if page_size == 0 {
            return Err(SessionStoreError::operation(
                "validate session page size",
                &self.database_path,
                "page size must be greater than zero",
            ));
        }

        let page_size = page_size.min(64);
        let fetch_limit = i64::try_from(page_size.saturating_add(1)).map_err(|error| {
            SessionStoreError::operation("validate session page size", &self.database_path, error)
        })?;
        let cursor_updated_at = cursor.map(SessionCursor::updated_at);
        let cursor_id = cursor.map(SessionCursor::id);
        let mut statement = self
            .connection
            .prepare(&format!(
                "{SESSION_WITH_LATEST_ATTEMPT_SELECT}
             WHERE (sessions.completed_turn_count > 0 OR EXISTS (
                  SELECT 1 FROM session_attempts
                  WHERE session_attempts.session_id = sessions.id
                    AND session_attempts.retry_prompt IS NOT NULL
             )) AND (?1 IS NULL OR sessions.project = ?1)
                AND (?2 = ''
                     OR instr(lower(CAST(sessions.id AS TEXT)), lower(?2)) > 0
                     OR instr(lower(sessions.title), lower(?2)) > 0
                     OR instr(lower(sessions.project), lower(?2)) > 0
                     OR instr(lower(sessions.active_agent), lower(?2)) > 0)
                AND (?3 IS NULL
                     OR sessions.updated_at < ?3
                     OR (sessions.updated_at = ?3 AND sessions.id < ?4))
             ORDER BY sessions.updated_at DESC, sessions.id DESC
             LIMIT ?5"
            ))
            .map_err(|error| {
                SessionStoreError::operation("prepare session page", &self.database_path, error)
            })?;
        let mut sessions = statement
            .query_map(
                params![project, query, cursor_updated_at, cursor_id, fetch_limit],
                stored_session_from_row,
            )
            .map_err(|error| {
                SessionStoreError::operation("query session page", &self.database_path, error)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| {
                SessionStoreError::operation("read session page", &self.database_path, error)
            })?;
        let has_more = sessions.len() > page_size;
        sessions.truncate(page_size);
        let next_cursor = has_more
            .then(|| sessions.last())
            .flatten()
            .map(|session| SessionCursor::new(session.metadata.updated_at, session.metadata.id));
        Ok(SessionPage {
            sessions,
            next_cursor,
        })
    }

    /// Reads the sessions forked directly from `parent_id`, oldest fork first.
    ///
    /// One level only: assembling a forest is repeated calls down the lineage, which keeps the
    /// depth a caller's concern rather than a recursive query's. Unlike
    /// [`SessionStore::list_session_page`] this applies no content filter — a fork always carries
    /// the history it copied — and it returns an empty list for a session with no forks and for
    /// an unknown id alike, since a parent that is gone has no children left to list.
    pub fn list_session_children(
        &self,
        parent_id: i64,
    ) -> Result<Vec<StoredSession>, SessionStoreError> {
        let mut statement = self
            .connection
            .prepare(&format!(
                "{SESSION_WITH_LATEST_ATTEMPT_SELECT}
             WHERE sessions.parent_session_id = ?1
             ORDER BY sessions.created_at, sessions.id"
            ))
            .map_err(|error| {
                SessionStoreError::operation("prepare session children", &self.database_path, error)
            })?;

        statement
            .query_map([parent_id], stored_session_from_row)
            .map_err(|error| {
                SessionStoreError::operation("query session children", &self.database_path, error)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| {
                SessionStoreError::operation("read session children", &self.database_path, error)
            })
    }

    /// Reads a page of a session's thread, oldest message first.
    ///
    /// Unlike [`SessionStore::load_session_for_resume`], this does not require the
    /// session to be resumable: the thread is evidence, and a run that failed or
    /// was exhausted is exactly the one worth reading. It never writes to the
    /// thread, which the `&self` receiver makes structural.
    pub fn read_transcript_page(
        &self,
        session_id: i64,
        cursor: Option<TranscriptCursor>,
        page_size: usize,
    ) -> Result<TranscriptPage, SessionStoreError> {
        if page_size == 0 {
            return Err(SessionStoreError::operation(
                "validate transcript page size",
                &self.database_path,
                "page size must be greater than zero",
            ));
        }

        let exists = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                [session_id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| {
                SessionStoreError::operation("check session", &self.database_path, error)
            })?;
        if !exists {
            return Err(SessionStoreError::operation(
                "read transcript page",
                &self.database_path,
                format!("unknown session {session_id}"),
            ));
        }

        let page_size = page_size.min(MAX_TRANSCRIPT_PAGE_SIZE);
        let sequences = self.transcript_sequences(session_id, cursor, page_size)?;
        let has_more = sequences.len() > page_size;
        let sequences = &sequences[..sequences.len().min(page_size)];

        let (Some(first), Some(last)) = (sequences.first(), sequences.last()) else {
            return Ok(TranscriptPage {
                messages: Vec::new(),
                next_cursor: None,
            });
        };

        Ok(TranscriptPage {
            messages: self.transcript_messages(session_id, *first, *last)?,
            next_cursor: has_more.then(|| TranscriptCursor::new(*last)),
        })
    }

    /// Selects the message boundaries of the page before any part is read, so a
    /// message's parts can never be split across two pages.
    fn transcript_sequences(
        &self,
        session_id: i64,
        cursor: Option<TranscriptCursor>,
        page_size: usize,
    ) -> Result<Vec<i64>, SessionStoreError> {
        let fetch_limit = i64::try_from(page_size.saturating_add(1)).map_err(|error| {
            SessionStoreError::operation(
                "validate transcript page size",
                &self.database_path,
                error,
            )
        })?;
        let after = cursor.map(TranscriptCursor::after_sequence);
        let mut statement = self
            .connection
            .prepare(
                "SELECT sequence FROM messages
                 WHERE session_id = ?1 AND (?2 IS NULL OR sequence > ?2)
                 ORDER BY sequence LIMIT ?3",
            )
            .map_err(|error| {
                SessionStoreError::operation(
                    "prepare transcript sequences",
                    &self.database_path,
                    error,
                )
            })?;

        statement
            .query_map(params![session_id, after, fetch_limit], |row| row.get(0))
            .map_err(|error| {
                SessionStoreError::operation(
                    "query transcript sequences",
                    &self.database_path,
                    error,
                )
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| {
                SessionStoreError::operation(
                    "read transcript sequences",
                    &self.database_path,
                    error,
                )
            })
    }

    fn transcript_messages(
        &self,
        session_id: i64,
        first: i64,
        last: i64,
    ) -> Result<Vec<Message>, SessionStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT messages.sequence, role, kind, text, call_id, name, input_json, content, is_error, media_id, mime
                 FROM messages JOIN message_parts ON messages.session_id = message_parts.session_id
                     AND messages.sequence = message_parts.message_sequence
                 WHERE messages.session_id = ?1 AND messages.sequence BETWEEN ?2 AND ?3
                 ORDER BY messages.sequence, message_parts.sequence",
            )
            .map_err(|error| {
                SessionStoreError::operation("prepare transcript page", &self.database_path, error)
            })?;
        let rows = statement
            .query_map(params![session_id, first, last], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<bool>>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            })
            .map_err(|error| {
                SessionStoreError::operation("query transcript page", &self.database_path, error)
            })?;

        let mut messages = Vec::new();
        let mut sequence = None;

        for row in rows {
            let (
                message_sequence,
                role,
                kind,
                text,
                call_id,
                name,
                input,
                content,
                is_error,
                media_id,
                mime,
            ) = row.map_err(|error| {
                SessionStoreError::operation("read transcript page", &self.database_path, error)
            })?;

            if sequence != Some(message_sequence) {
                messages.push(Message {
                    role: decode_role(&role, &self.database_path)?,
                    parts: Vec::new(),
                });
                sequence = Some(message_sequence);
            }

            messages
                .last_mut()
                .expect("message inserted for part")
                .parts
                .push(decode_part(
                    &kind,
                    (
                        text, call_id, name, input, content, is_error, media_id, mime,
                    ),
                    &self.database_path,
                )?);
        }

        Ok(messages)
    }

    pub fn recover_running_attempt(
        &mut self,
        key: AttemptKey,
        finished_at: i64,
    ) -> Result<RecoveryOutcome, SessionStoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation("start attempt recovery", &self.database_path, error)
            })?;
        let changed = transaction
            .execute(
                "UPDATE session_attempts SET status = 'interrupted', failure_kind = 'interrupted', finished_at = ?1
                 WHERE id = ?2 AND session_id = ?3 AND status = 'running'",
                params![finished_at, key.attempt_id(), key.session_id()],
            )
            .map_err(|error| {
                SessionStoreError::operation("recover session attempt", &self.database_path, error)
            })?;
        if changed == 0 {
            transaction.commit().map_err(|error| {
                SessionStoreError::operation(
                    "commit stale attempt recovery",
                    &self.database_path,
                    error,
                )
            })?;
            return Ok(RecoveryOutcome::Stale);
        }
        transaction
            .execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                params![finished_at, key.session_id()],
            )
            .map_err(|error| {
                SessionStoreError::operation("update recovered session", &self.database_path, error)
            })?;
        let summary = latest_attempt_summary(&transaction, &self.database_path, key.session_id())?
            .ok_or_else(|| {
                SessionStoreError::operation(
                    "recover session attempt",
                    &self.database_path,
                    "recovered attempt is unavailable",
                )
            })?;
        transaction.commit().map_err(|error| {
            SessionStoreError::operation("commit attempt recovery", &self.database_path, error)
        })?;
        Ok(RecoveryOutcome::Recovered(summary))
    }

    pub fn load_retry_boundary(
        &self,
        key: AttemptKey,
    ) -> Result<Option<RetryBoundary>, SessionStoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT retry_prompt, retry_media_ids FROM session_attempts WHERE id = ?1 AND session_id = ?2",
                params![key.attempt_id(), key.session_id()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                SessionStoreError::operation(
                    "load attempt retry boundary",
                    &self.database_path,
                    error,
                )
            })?;

        let Some((prompt, media_ids_json)) = row else {
            return Ok(None);
        };

        let Some(prompt) = prompt else {
            return Ok(None);
        };

        let media_ids = decode_retry_media_ids(media_ids_json.as_deref(), &self.database_path)?;
        RetryBoundary::new(key, prompt, media_ids)
            .map(Some)
            .map_err(|error| {
                SessionStoreError::operation(
                    "validate attempt retry boundary",
                    &self.database_path,
                    format!("{error:?}"),
                )
            })
    }

    /// The literal filesystem root a session's tools must be confined to.
    ///
    /// Falls back to the session's `project` column when `confinement_root` was never recorded
    /// (every row created before migration `0005`), so a pre-existing session still resumes to
    /// the root it was always confined to rather than failing to resolve one at all.
    pub fn confinement_root(&self, session_id: i64) -> Result<PathBuf, SessionStoreError> {
        let (confinement_root, project): (Option<String>, String) = self
            .connection
            .query_row(
                "SELECT confinement_root, project FROM sessions WHERE id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| {
                SessionStoreError::operation("load confinement root", &self.database_path, error)
            })?;

        Ok(PathBuf::from(confinement_root.unwrap_or(project)))
    }

    /// The session's recorded bypass-permission-prompts value. `None` means it was never
    /// recorded — either the session predates migration `0006` or no turn has completed since it
    /// was created — and the caller falls back to configuration, exactly as `confinement_root`
    /// falls back to `project`.
    pub fn bypass_permission_prompts(
        &self,
        session_id: i64,
    ) -> Result<Option<bool>, SessionStoreError> {
        let stored: Option<i64> = self
            .connection
            .query_row(
                "SELECT bypass_permission_prompts FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                SessionStoreError::operation(
                    "load bypass permission prompts",
                    &self.database_path,
                    error,
                )
            })?;

        Ok(stored.map(|value| value != 0))
    }

    /// Records the session's bypass-permission-prompts value, overwriting whatever was recorded
    /// before.
    pub fn set_bypass_permission_prompts(
        &mut self,
        session_id: i64,
        enabled: bool,
    ) -> Result<(), SessionStoreError> {
        self.connection
            .execute(
                "UPDATE sessions SET bypass_permission_prompts = ?1 WHERE id = ?2",
                params![enabled as i64, session_id],
            )
            .map_err(|error| {
                SessionStoreError::operation(
                    "record bypass permission prompts",
                    &self.database_path,
                    error,
                )
            })?;

        Ok(())
    }

    pub fn load_session_for_resume(&self, id: i64) -> Result<StoredSession, SessionStoreError> {
        let metadata = self
            .connection
            .query_row(
                "SELECT id, project, title, active_agent, created_at, updated_at, completed_turn_count, resumable,
                        provider_id, model_id, reasoning_effort, parent_session_id, fork_message_count
                 FROM sessions WHERE id = ?1 AND (resumable = 1 OR EXISTS (
                     SELECT 1 FROM session_attempts
                     WHERE session_attempts.session_id = sessions.id
                       AND session_attempts.retry_prompt IS NOT NULL
                 ))",
                [id],
                session_metadata,
            )
            .optional()
            .map_err(|error| SessionStoreError::operation("load session", &self.database_path, error))?;
        let Some(metadata) = metadata else {
            let legacy = self
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM legacy_turns WHERE id = ?1)",
                    [id],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(|error| {
                    SessionStoreError::operation("check session", &self.database_path, error)
                })?;
            let reason = if legacy {
                format!("legacy session {id} is non-resumable")
            } else {
                format!("unknown session {id}")
            };
            return Err(SessionStoreError::operation(
                "load session",
                &self.database_path,
                reason,
            ));
        };
        let mut statement = self.connection.prepare(
            "SELECT messages.sequence, role, kind, text, call_id, name, input_json, content, is_error, media_id, mime
             FROM messages JOIN message_parts ON messages.session_id = message_parts.session_id
                 AND messages.sequence = message_parts.message_sequence
             WHERE messages.session_id = ?1 ORDER BY messages.sequence, message_parts.sequence",
        ).map_err(|error| SessionStoreError::operation("prepare session messages", &self.database_path, error))?;
        let rows = statement
            .query_map([id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<bool>>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            })
            .map_err(|error| {
                SessionStoreError::operation("query session messages", &self.database_path, error)
            })?;
        let mut messages = Vec::new();
        let mut sequence = None;
        for row in rows {
            let (
                message_sequence,
                role,
                kind,
                text,
                call_id,
                name,
                input,
                content,
                is_error,
                media_id,
                mime,
            ) = row.map_err(|error| {
                SessionStoreError::operation("read session messages", &self.database_path, error)
            })?;
            if sequence != Some(message_sequence) {
                messages.push(Message {
                    role: decode_role(&role, &self.database_path)?,
                    parts: Vec::new(),
                });
                sequence = Some(message_sequence);
            }
            messages
                .last_mut()
                .expect("message inserted for part")
                .parts
                .push(decode_part(
                    &kind,
                    (
                        text, call_id, name, input, content, is_error, media_id, mime,
                    ),
                    &self.database_path,
                )?);
        }

        let latest_attempt = latest_attempt_summary(&self.connection, &self.database_path, id)?;
        Ok(StoredSession {
            metadata,
            messages,
            latest_attempt,
        })
    }

    pub fn update_session(&mut self, metadata: &SessionMetadata) -> Result<(), SessionStoreError> {
        metadata.validate().map_err(|error| {
            SessionStoreError::operation(
                "validate session metadata",
                &self.database_path,
                format!("{error:?}"),
            )
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation("start session update", &self.database_path, error)
            })?;
        let count = i64::try_from(metadata.completed_turn_count).map_err(|error| {
            SessionStoreError::operation("validate session metadata", &self.database_path, error)
        })?;
        if transaction
            .execute(
                "UPDATE sessions SET title = ?1, active_agent = ?2, updated_at = ?3
             WHERE id = ?4 AND project = ?5 AND created_at = ?6
               AND completed_turn_count = ?7 AND resumable = ?8",
                params![
                    metadata.title,
                    metadata.active_agent,
                    metadata.updated_at,
                    metadata.id,
                    metadata.project,
                    metadata.created_at,
                    count,
                    metadata.resumable
                ],
            )
            .map_err(|error| {
                SessionStoreError::operation("update session", &self.database_path, error)
            })?
            != 1
        {
            return Err(SessionStoreError::operation(
                "update session",
                &self.database_path,
                "session metadata changed",
            ));
        }
        transaction.commit().map_err(|error| {
            SessionStoreError::operation("commit session update", &self.database_path, error)
        })
    }

    pub fn update_session_selection(
        &mut self,
        metadata: &SessionMetadata,
    ) -> Result<(), SessionStoreError> {
        validate_metadata(metadata, &self.database_path)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation(
                    "start session selection update",
                    &self.database_path,
                    error,
                )
            })?;
        let count = i64::try_from(metadata.completed_turn_count).map_err(|error| {
            SessionStoreError::operation("validate session metadata", &self.database_path, error)
        })?;
        let changed = transaction
            .execute(
                "UPDATE sessions SET provider_id = ?1, model_id = ?2, reasoning_effort = ?3
             WHERE id = ?4 AND project = ?5 AND title = ?6 AND active_agent = ?7
               AND created_at = ?8 AND updated_at = ?9 AND completed_turn_count = ?10
               AND resumable = ?11",
                params![
                    metadata.provider_id,
                    metadata.model_id,
                    metadata.reasoning_effort.map(ReasoningEffort::as_str),
                    metadata.id,
                    metadata.project,
                    metadata.title,
                    metadata.active_agent,
                    metadata.created_at,
                    metadata.updated_at,
                    count,
                    metadata.resumable,
                ],
            )
            .map_err(|error| {
                SessionStoreError::operation("update session selection", &self.database_path, error)
            })?;
        if changed != 1 {
            return Err(SessionStoreError::operation(
                "update session selection",
                &self.database_path,
                "session metadata changed",
            ));
        }
        transaction.commit().map_err(|error| {
            SessionStoreError::operation(
                "commit session selection update",
                &self.database_path,
                error,
            )
        })
    }

    pub fn delete_session(&mut self, id: i64) -> Result<(), SessionStoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation("start session delete", &self.database_path, error)
            })?;
        transaction
            .execute("DELETE FROM sessions WHERE id = ?1", [id])
            .and_then(|_| transaction.execute("DELETE FROM legacy_turns WHERE id = ?1", [id]))
            .map_err(|error| {
                SessionStoreError::operation("delete session", &self.database_path, error)
            })?;
        transaction.commit().map_err(|error| {
            SessionStoreError::operation("commit session delete", &self.database_path, error)
        })
    }

    /// Copies a prefix of `source_id`'s conversation into a new session and returns its id,
    /// leaving the source untouched.
    ///
    /// `message_prefix` is a message sequence, not an offset: every message at or below it is
    /// copied, together with the turns those messages belong to and every part they hold.
    /// Sequences are per-session, so they are copied verbatim rather than renumbered and the fork
    /// reads back with the same numbering the source has. A prefix landing inside a turn copies
    /// that turn with only the messages under the cut, the same way a truncation keeps the turn
    /// it lands in, and the fork's `completed_turn_count`/`resumable` are set from the turns that
    /// were actually copied.
    ///
    /// The fork starts with no attempt history: nothing has ever run in it, so it has no attempt
    /// to recover, retry or report. `media` rows are shared between sessions by content hash, so
    /// the copied parts reference the same rows and no blob is copied, rewritten or reference
    /// counted.
    ///
    /// One immediate transaction covers the whole copy: a half-copied fork would be a session
    /// whose history stops mid-turn for no reason a reader could see.
    pub fn fork_session(
        &mut self,
        source_id: i64,
        message_prefix: i64,
    ) -> Result<i64, ForkSessionError> {
        let database_path = self.database_path.clone();
        let store_error = |operation: &'static str| {
            let database_path = database_path.clone();
            move |error: rusqlite::Error| {
                ForkSessionError::Store(SessionStoreError::operation(
                    operation,
                    &database_path,
                    error,
                ))
            }
        };

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error("start session fork"))?;

        let source_exists = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                [source_id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(store_error("check fork source"))?;
        if !source_exists {
            return Err(ForkSessionError::UnknownSession(source_id));
        }

        let available = transaction
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) FROM messages WHERE session_id = ?1",
                [source_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(store_error("read fork source history"))?;
        let copied_messages = transaction
            .query_row(
                "SELECT count(*) FROM messages WHERE session_id = ?1 AND sequence <= ?2",
                params![source_id, message_prefix],
                |row| row.get::<_, i64>(0),
            )
            .map_err(store_error("read fork source history"))?;
        if message_prefix < 1 || message_prefix > available || copied_messages == 0 {
            return Err(ForkSessionError::PrefixOutOfRange {
                requested: message_prefix,
                available,
            });
        }

        transaction
            .execute(
                "INSERT INTO sessions (project, title, active_agent, provider_id, model_id,
                                       reasoning_effort, created_at, updated_at, confinement_root,
                                       bypass_permission_prompts, parent_session_id,
                                       fork_message_count, completed_turn_count, resumable)
                 SELECT project, title, active_agent, provider_id, model_id,
                        reasoning_effort, CAST(strftime('%s','now') AS INTEGER),
                        CAST(strftime('%s','now') AS INTEGER), confinement_root,
                        bypass_permission_prompts, id,
                        ?2, 0, 0
                 FROM sessions WHERE id = ?1",
                params![source_id, message_prefix],
            )
            .map_err(store_error("create forked session"))?;
        let fork_id = transaction.last_insert_rowid();

        copy_session_prefix(&transaction, source_id, fork_id, message_prefix)
            .map_err(store_error("copy forked session history"))?;

        transaction
            .commit()
            .map_err(store_error("commit session fork"))?;
        Ok(fork_id)
    }

    /// Reads one session by id, without its messages, or `None` when it is gone.
    ///
    /// Unlike [`SessionStore::load_session_for_resume`] this applies no resumability filter and
    /// treats a missing session as an absence rather than a failure. It exists for the lineage
    /// views, which show a session because something was forked from it — a reason that holds
    /// whether or not the session itself could be entered.
    pub fn read_session(&self, id: i64) -> Result<Option<StoredSession>, SessionStoreError> {
        self.connection
            .query_row(
                &format!("{SESSION_WITH_LATEST_ATTEMPT_SELECT} WHERE sessions.id = ?1"),
                [id],
                stored_session_from_row,
            )
            .optional()
            .map_err(|error| {
                SessionStoreError::operation("read session", &self.database_path, error)
            })
    }

    /// The session `session_id` was forked from, if it was forked at all.
    ///
    /// The counterpart of [`SessionStore::list_session_children`], and the step a caller repeats
    /// to climb to the session a lineage is rooted at. A parent id that names a session which no
    /// longer exists still reads as `Some`: the row records where the fork came from, not whether
    /// that session is still around, and a caller climbing past it simply finds nothing there.
    ///
    /// An unknown session reads as `None`, the same as a session that was started rather than
    /// forked — neither has a parent to climb to.
    pub fn session_parent(&self, session_id: i64) -> Result<Option<i64>, SessionStoreError> {
        self.connection
            .query_row(
                "SELECT parent_session_id FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(|error| {
                SessionStoreError::operation("read session parent", &self.database_path, error)
            })
    }

    /// The sequence of the `count`th message of a session, counting from its oldest.
    ///
    /// [`SessionStore::fork_session`] cuts at a sequence, but a caller measuring an in-memory
    /// history only ever holds a count: sequences are not dense, because a truncation leaves the
    /// surviving messages with the numbering they already had. This is the one translation
    /// between the two, so a caller never has to assume the nth message is numbered n.
    ///
    /// Returns `None` when the session holds fewer than `count` messages, which is a caller
    /// asking about a cut that does not exist rather than a storage failure.
    pub fn message_sequence_at(
        &self,
        session_id: i64,
        count: usize,
    ) -> Result<Option<i64>, SessionStoreError> {
        let Some(offset) = count.checked_sub(1) else {
            return Ok(None);
        };
        let offset = i64::try_from(offset).map_err(|error| {
            SessionStoreError::operation(
                "read session message sequence",
                &self.database_path,
                error,
            )
        })?;

        self.connection
            .query_row(
                "SELECT sequence FROM messages WHERE session_id = ?1
                 ORDER BY sequence LIMIT 1 OFFSET ?2",
                params![session_id, offset],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| {
                SessionStoreError::operation(
                    "read session message sequence",
                    &self.database_path,
                    error,
                )
            })
    }

    /// Drops what a session persisted between its first `surviving_messages` messages and the
    /// `measured_messages` the caller's own history covers.
    ///
    /// Both counts are prefixes of the session's messages in `sequence` order, which is the order
    /// [`SessionStore::load_session_for_resume`] reads them back in and the order the in-memory
    /// history holds them in. Message sequences are only ever appended, so the nth message names
    /// the same row before and after another turn extends the session past it.
    ///
    /// The upper bound is what keeps a caller from deleting rows it never saw. A sub-agent turn
    /// persisted out of band after the caller measured its history sits past `measured_messages`
    /// and belongs to no undone range, so it is kept even though it lies after the surviving
    /// prefix; deleting it would drop a turn nobody took back. A session that holds fewer than
    /// `measured_messages` messages is truncated to its end.
    ///
    /// A turn keeps its row while any of its messages survive, so a prefix landing inside a turn
    /// keeps that turn with fewer messages. A turn left with none is deleted together with the
    /// attempt that completed it, and `completed_turn_count`/`resumable` are recomputed from the
    /// turns that remain rather than adjusted by a delta. Attempts are deleted before their turn
    /// so the `ON DELETE SET NULL` back-reference cannot clear a completed attempt's turn and
    /// break its own `CHECK`.
    ///
    /// `media` rows are shared between sessions by content hash and are left alone; only the
    /// message parts that referenced them go.
    ///
    /// Everything happens in one immediate transaction: a partial delete would leave a session
    /// that cannot be resumed.
    pub fn truncate_session_history(
        &mut self,
        session_id: i64,
        surviving_messages: usize,
        measured_messages: usize,
    ) -> Result<(), SessionStoreError> {
        let surviving = i64::try_from(surviving_messages).map_err(|error| {
            SessionStoreError::operation("truncate session history", &self.database_path, error)
        })?;
        let measured =
            i64::try_from(measured_messages.max(surviving_messages)).map_err(|error| {
                SessionStoreError::operation("truncate session history", &self.database_path, error)
            })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation(
                    "start session history truncation",
                    &self.database_path,
                    error,
                )
            })?;

        let Some(last_surviving_sequence) =
            surviving_message_sequence(&transaction, &self.database_path, session_id, surviving)?
        else {
            return Ok(());
        };
        let last_measured_sequence =
            surviving_message_sequence(&transaction, &self.database_path, session_id, measured)?
                .unwrap_or(i64::MAX);

        transaction
            .execute(
                "DELETE FROM session_attempts
                 WHERE session_id = ?1 AND completed_turn_sequence IS NOT NULL
                   AND completed_turn_sequence NOT IN (
                       SELECT turn_sequence FROM messages
                       WHERE session_id = ?1 AND (sequence <= ?2 OR sequence > ?3)
                   )",
                params![session_id, last_surviving_sequence, last_measured_sequence],
            )
            .and_then(|_| {
                transaction.execute(
                    "DELETE FROM messages
                     WHERE session_id = ?1 AND sequence > ?2 AND sequence <= ?3",
                    params![session_id, last_surviving_sequence, last_measured_sequence],
                )
            })
            .and_then(|_| {
                transaction.execute(
                    "DELETE FROM turns
                     WHERE session_id = ?1 AND sequence NOT IN (
                         SELECT turn_sequence FROM messages WHERE session_id = ?1
                     )",
                    params![session_id],
                )
            })
            .and_then(|_| {
                transaction.execute(
                    "UPDATE sessions
                     SET completed_turn_count = (SELECT count(*) FROM turns WHERE session_id = ?1),
                         resumable = (SELECT count(*) FROM turns WHERE session_id = ?1) > 0
                     WHERE id = ?1",
                    params![session_id],
                )
            })
            .map_err(|error| {
                SessionStoreError::operation("truncate session history", &self.database_path, error)
            })?;

        transaction.commit().map_err(|error| {
            SessionStoreError::operation(
                "commit session history truncation",
                &self.database_path,
                error,
            )
        })
    }

    pub fn persist_completed_session_turn(
        &mut self,
        metadata: &SessionMetadata,
        turn: &CompletedSessionTurn,
    ) -> Result<SessionMetadata, SessionStoreError> {
        validate_attempt_metadata(metadata).map_err(|error| {
            SessionStoreError::operation(
                "validate session metadata",
                &self.database_path,
                format!("{error:?}"),
            )
        })?;
        let mut persisted_metadata = metadata.clone();
        persisted_metadata.resumable = true;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation("start session turn", &self.database_path, error)
            })?;
        persisted_metadata.id =
            insert_attempt_session(&transaction, &self.database_path, metadata)?;
        let persisted = persist_completed_turn_in_transaction(
            &transaction,
            &self.database_path,
            &persisted_metadata,
            turn,
            persisted_metadata.updated_at,
        )?;
        persisted_metadata.completed_turn_count = u64::try_from(persisted.completed_turn_count)
            .map_err(|error| {
                SessionStoreError::operation("update session", &self.database_path, error)
            })?;
        transaction.commit().map_err(|error| {
            SessionStoreError::operation("commit session turn", &self.database_path, error)
        })?;
        Ok(persisted_metadata)
    }
}

/// Copies the forked prefix in foreign-key order — turns, then the messages that reference them,
/// then those messages' parts — and sets the fork's turn counters from what the copy produced.
///
/// A turn is copied when any message under the cut belongs to it, so a cut inside a turn carries
/// that turn with fewer messages. Media ids are copied as references; the `media` rows they point
/// at are shared by content hash and belong to no single session.
fn copy_session_prefix(
    transaction: &Transaction<'_>,
    source_id: i64,
    fork_id: i64,
    message_prefix: i64,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO turns (session_id, sequence, completed_at)
         SELECT ?2, sequence, completed_at FROM turns
         WHERE session_id = ?1 AND sequence IN (
             SELECT turn_sequence FROM messages WHERE session_id = ?1 AND sequence <= ?3
         )",
        params![source_id, fork_id, message_prefix],
    )?;
    transaction.execute(
        "INSERT INTO messages (session_id, sequence, turn_sequence, role)
         SELECT ?2, sequence, turn_sequence, role FROM messages
         WHERE session_id = ?1 AND sequence <= ?3",
        params![source_id, fork_id, message_prefix],
    )?;
    transaction.execute(
        "INSERT INTO message_parts (session_id, message_sequence, sequence, kind, text, call_id,
                                    name, input_json, content, is_error, media_id, mime)
         SELECT ?2, message_sequence, sequence, kind, text, call_id,
                name, input_json, content, is_error, media_id, mime
         FROM message_parts
         WHERE session_id = ?1 AND message_sequence <= ?3",
        params![source_id, fork_id, message_prefix],
    )?;
    transaction.execute(
        "UPDATE sessions
         SET completed_turn_count = (SELECT count(*) FROM turns WHERE session_id = ?1),
             resumable = (SELECT count(*) FROM turns WHERE session_id = ?1) > 0
         WHERE id = ?1",
        params![fork_id],
    )?;

    Ok(())
}

fn validate_attempt_metadata(
    metadata: &SessionMetadata,
) -> Result<(), agens_core::SessionMetadataError> {
    if metadata.id != 0 {
        return metadata.validate();
    }

    SessionMetadata {
        id: i64::MAX,
        ..metadata.clone()
    }
    .validate()
}

fn latest_attempt_summary(
    connection: &rusqlite::Connection,
    database_path: &std::path::Path,
    session_id: i64,
) -> Result<Option<SessionAttemptSummary>, SessionStoreError> {
    connection
        .query_row(
            "SELECT id, sequence, status, failure_kind, started_at, finished_at
             FROM session_attempts WHERE session_id = ?1 ORDER BY sequence DESC LIMIT 1",
            [session_id],
            |row| attempt_summary_from_row(row, session_id, 0),
        )
        .optional()
        .map_err(|error| SessionStoreError::operation("load session attempt", database_path, error))
}

fn stored_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSession> {
    let metadata = session_metadata(row)?;
    let latest_attempt = row
        .get::<_, Option<i64>>(LATEST_ATTEMPT_COLUMN_OFFSET)?
        .map(|_| attempt_summary_from_row(row, metadata.id, LATEST_ATTEMPT_COLUMN_OFFSET))
        .transpose()?;

    Ok(StoredSession {
        metadata,
        messages: Vec::new(),
        latest_attempt,
    })
}

fn attempt_summary_from_row(
    row: &rusqlite::Row<'_>,
    session_id: i64,
    offset: usize,
) -> rusqlite::Result<SessionAttemptSummary> {
    let status = decode_attempt_status(&row.get::<_, String>(offset + 2)?)?;
    let failure_kind = row
        .get::<_, Option<String>>(offset + 3)?
        .map(|value| decode_attempt_failure_kind(&value))
        .transpose()?;

    SessionAttemptSummary::new(
        AttemptKey::new(session_id, row.get(offset)?).map_err(|_| rusqlite::Error::InvalidQuery)?,
        row.get::<_, i64>(offset + 1)?
            .try_into()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        status,
        failure_kind,
        row.get(offset + 4)?,
        row.get(offset + 5)?,
    )
    .map_err(|_| rusqlite::Error::InvalidQuery)
}

fn decode_attempt_status(value: &str) -> rusqlite::Result<SessionAttemptStatus> {
    match value {
        "running" => Ok(SessionAttemptStatus::Running),
        "completed" => Ok(SessionAttemptStatus::Completed),
        "cancelled" => Ok(SessionAttemptStatus::Cancelled),
        "failed" => Ok(SessionAttemptStatus::Failed),
        "provider_error" => Ok(SessionAttemptStatus::ProviderError),
        "interrupted" => Ok(SessionAttemptStatus::Interrupted),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn decode_attempt_failure_kind(value: &str) -> rusqlite::Result<SessionAttemptFailureKind> {
    match value {
        "cancelled" => Ok(SessionAttemptFailureKind::Cancelled),
        "failed" => Ok(SessionAttemptFailureKind::Failed),
        "provider_error" => Ok(SessionAttemptFailureKind::ProviderError),
        "interrupted" => Ok(SessionAttemptFailureKind::Interrupted),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

/// Seeds `confinement_root` from `project` at session creation: today the two are always the
/// same discovered root, so a brand-new row can carry both without a second confinement-root
/// input threaded through this call. A resumed session reads this column back explicitly rather
/// than re-deriving it from the process's own current working directory, which is the whole
/// reason the column exists as its own field — the two are expected to diverge once a session's
/// literal confinement root can differ from its project grouping/display identity.
fn insert_attempt_session(
    transaction: &Transaction<'_>,
    database_path: &std::path::Path,
    metadata: &SessionMetadata,
) -> Result<i64, SessionStoreError> {
    if metadata.id == 0 {
        transaction
            .execute(
                "INSERT INTO sessions (project, title, active_agent, provider_id, model_id,
                                        reasoning_effort, created_at, updated_at, confinement_root)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?1)",
                params![
                    metadata.project,
                    metadata.title,
                    metadata.active_agent,
                    metadata.provider_id,
                    metadata.model_id,
                    metadata.reasoning_effort.map(ReasoningEffort::as_str),
                    metadata.created_at,
                    metadata.updated_at,
                ],
            )
            .map_err(|error| {
                SessionStoreError::operation("create session", database_path, error)
            })?;
        return Ok(transaction.last_insert_rowid());
    }

    transaction
        .execute(
            "INSERT INTO sessions (id, project, title, active_agent, provider_id, model_id, reasoning_effort, created_at, updated_at, confinement_root)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?2) ON CONFLICT(id) DO NOTHING",
            params![metadata.id, metadata.project, metadata.title, metadata.active_agent, metadata.provider_id, metadata.model_id, metadata.reasoning_effort.map(ReasoningEffort::as_str), metadata.created_at, metadata.updated_at],
        )
        .map_err(|error| SessionStoreError::operation("create session", database_path, error))?;
    Ok(metadata.id)
}

struct PersistedTurn {
    sequence: i64,
    completed_turn_count: i64,
}

/// Appends a completed turn relative to the count stored in the database rather than to a
/// caller-supplied snapshot: the enclosing immediate transaction already holds the write lock,
/// so another turn persisted by the same session (a sub-agent turn running inside the parent
/// turn) must extend the history instead of rejecting it.
fn persist_completed_turn_in_transaction(
    transaction: &Transaction<'_>,
    database_path: &std::path::Path,
    metadata: &SessionMetadata,
    turn: &CompletedSessionTurn,
    completed_at: i64,
) -> Result<PersistedTurn, SessionStoreError> {
    let stored_turn_count = transaction
        .query_row(
            "SELECT completed_turn_count FROM sessions WHERE id = ?1",
            params![metadata.id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| SessionStoreError::operation("update session", database_path, error))?
        .ok_or_else(|| {
            SessionStoreError::operation("update session", database_path, "session does not exist")
        })?;
    let completed_turn_count = stored_turn_count.checked_add(1).ok_or_else(|| {
        SessionStoreError::operation(
            "update session",
            database_path,
            "completed turn count overflow",
        )
    })?;
    transaction
        .execute(
            "UPDATE sessions SET active_agent = ?1, provider_id = ?2, model_id = ?3,
                reasoning_effort = ?4, updated_at = ?5,
                completed_turn_count = ?6, resumable = 1
             WHERE id = ?7",
            params![
                metadata.active_agent,
                metadata.provider_id,
                metadata.model_id,
                metadata.reasoning_effort.map(ReasoningEffort::as_str),
                completed_at,
                completed_turn_count,
                metadata.id
            ],
        )
        .map_err(|error| SessionStoreError::operation("update session", database_path, error))?;

    let turn_sequence = next_sequence(transaction, database_path, "turns", metadata.id)?;
    transaction
        .execute(
            "INSERT INTO turns (session_id, sequence, completed_at) VALUES (?1, ?2, ?3)",
            params![metadata.id, turn_sequence, completed_at],
        )
        .map_err(|error| SessionStoreError::operation("create turn", database_path, error))?;
    let first_message_sequence =
        next_sequence(transaction, database_path, "messages", metadata.id)?;
    for (message_offset, message) in turn.messages().iter().enumerate() {
        let message_sequence = first_message_sequence + message_offset as i64;
        transaction
            .execute(
                "INSERT INTO messages (session_id, sequence, turn_sequence, role) VALUES (?1, ?2, ?3, ?4)",
                params![metadata.id, message_sequence, turn_sequence, encode_role(message.role)],
            )
            .map_err(|error| SessionStoreError::operation("create message", database_path, error))?;
        for (part_sequence, part) in message.parts.iter().enumerate() {
            insert_message_part(
                transaction,
                database_path,
                metadata.id,
                message_sequence,
                part_sequence as i64,
                part,
            )?;
        }
    }

    Ok(PersistedTurn {
        sequence: turn_sequence,
        completed_turn_count,
    })
}

fn attempt_status(status: SessionAttemptStatus) -> &'static str {
    match status {
        SessionAttemptStatus::Running => "running",
        SessionAttemptStatus::Completed => "completed",
        SessionAttemptStatus::Cancelled => "cancelled",
        SessionAttemptStatus::Failed => "failed",
        SessionAttemptStatus::ProviderError => "provider_error",
        SessionAttemptStatus::Interrupted => "interrupted",
    }
}

fn attempt_failure_kind(kind: SessionAttemptFailureKind) -> &'static str {
    match kind {
        SessionAttemptFailureKind::Cancelled => "cancelled",
        SessionAttemptFailureKind::Failed => "failed",
        SessionAttemptFailureKind::ProviderError => "provider_error",
        SessionAttemptFailureKind::Interrupted => "interrupted",
    }
}

fn insert_message_part(
    transaction: &Transaction<'_>,
    database_path: &std::path::Path,
    session_id: i64,
    message_sequence: i64,
    sequence: i64,
    part: &MessagePart,
) -> Result<(), SessionStoreError> {
    let result = match part {
        MessagePart::Text(text) => transaction.execute(
            "INSERT INTO message_parts (session_id, message_sequence, sequence, kind, text) VALUES (?1, ?2, ?3, 'text', ?4)",
            params![session_id, message_sequence, sequence, text],
        ),
        MessagePart::Reasoning(text) => transaction.execute(
            "INSERT INTO message_parts (session_id, message_sequence, sequence, kind, text) VALUES (?1, ?2, ?3, 'reasoning', ?4)",
            params![session_id, message_sequence, sequence, text],
        ),
        MessagePart::ToolCall { id, name, input } => transaction.execute(
            "INSERT INTO message_parts (session_id, message_sequence, sequence, kind, call_id, name, input_json) VALUES (?1, ?2, ?3, 'tool_call', ?4, ?5, ?6)",
            params![session_id, message_sequence, sequence, id, name, canonical_json(input, database_path)?],
        ),
        MessagePart::ToolResult { tool_call_id, content, is_error } => transaction.execute(
            "INSERT INTO message_parts (session_id, message_sequence, sequence, kind, call_id, content, is_error) VALUES (?1, ?2, ?3, 'tool_result', ?4, ?5, ?6)",
            params![session_id, message_sequence, sequence, tool_call_id, content, is_error],
        ),
        MessagePart::Media { media_id, mime } => transaction.execute(
            "INSERT INTO message_parts (session_id, message_sequence, sequence, kind, media_id, mime) VALUES (?1, ?2, ?3, 'media', ?4, ?5)",
            params![session_id, message_sequence, sequence, media_id, mime],
        ),
    };
    result.map_err(|error| {
        SessionStoreError::operation("create message part", database_path, error)
    })?;
    Ok(())
}

fn canonical_json(
    input: &str,
    database_path: &std::path::Path,
) -> Result<String, SessionStoreError> {
    let mut value: serde_json::Value = serde_json::from_str(input).map_err(|error| {
        SessionStoreError::operation("canonicalize tool input", database_path, error)
    })?;
    canonicalize_value(&mut value);
    serde_json::to_string(&value).map_err(|error| {
        SessionStoreError::operation("canonicalize tool input", database_path, error)
    })
}

fn canonicalize_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => values.iter_mut().for_each(canonicalize_value),
        serde_json::Value::Object(values) => {
            values.values_mut().for_each(canonicalize_value);
            values.sort_keys();
        }
        _ => {}
    }
}

/// The `sequence` of the last message a `surviving`-message prefix keeps, or `None` when the
/// session holds fewer messages than that and there is nothing to truncate.
///
/// Zero surviving messages answers with a sequence below every stored one rather than with an
/// absent bound, since message sequences start at one.
fn surviving_message_sequence(
    transaction: &Transaction<'_>,
    database_path: &std::path::Path,
    session_id: i64,
    surviving: i64,
) -> Result<Option<i64>, SessionStoreError> {
    if surviving == 0 {
        return Ok(Some(0));
    }

    transaction
        .query_row(
            "SELECT sequence FROM messages WHERE session_id = ?1
             ORDER BY sequence LIMIT 1 OFFSET ?2",
            params![session_id, surviving - 1],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| {
            SessionStoreError::operation("read session history boundary", database_path, error)
        })
}

fn next_sequence(
    transaction: &Transaction<'_>,
    database_path: &std::path::Path,
    table: &str,
    session_id: i64,
) -> Result<i64, SessionStoreError> {
    transaction
        .query_row(
            &format!("SELECT COALESCE(MAX(sequence), 0) + 1 FROM {table} WHERE session_id = ?1"),
            [session_id],
            |row| row.get(0),
        )
        .map_err(|error| SessionStoreError::operation("allocate sequence", database_path, error))
}

fn encode_role(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::Supervisor => "supervisor",
    }
}

fn session_metadata(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMetadata> {
    let completed_turn_count = row.get::<_, i64>(6)?;

    let reasoning_effort = row
        .get::<_, Option<String>>(10)?
        .map(|value| {
            RequestConfig::with_reasoning_effort(&value)
                .ok()
                .and_then(|config| config.reasoning_effort())
                .ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        10,
                        rusqlite::types::Type::Text,
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid reasoning effort",
                        )
                        .into(),
                    )
                })
        })
        .transpose()?;
    let metadata = SessionMetadata {
        id: row.get(0)?,
        project: row.get(1)?,
        title: row.get(2)?,
        active_agent: row.get(3)?,
        provider_id: row.get(8)?,
        model_id: row.get(9)?,
        reasoning_effort,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        completed_turn_count: u64::try_from(completed_turn_count)
            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(6, completed_turn_count))?,
        resumable: row.get(7)?,
        parent_session_id: row.get(11)?,
        fork_message_count: row.get(12)?,
    };
    metadata.validate().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid session metadata").into(),
        )
    })?;
    Ok(metadata)
}

fn validate_metadata(
    metadata: &SessionMetadata,
    database_path: &std::path::Path,
) -> Result<(), SessionStoreError> {
    metadata.validate().map_err(|error| {
        SessionStoreError::operation(
            "validate session metadata",
            database_path,
            format!("{error:?}"),
        )
    })
}

fn decode_role(role: &str, database_path: &std::path::Path) -> Result<Role, SessionStoreError> {
    match role {
        "system" => Ok(Role::System),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        "supervisor" => Ok(Role::Supervisor),
        _ => Err(SessionStoreError::operation(
            "decode session message",
            database_path,
            "invalid role",
        )),
    }
}

fn decode_part(
    kind: &str,
    (text, call_id, name, input, content, is_error, media_id, mime): PersistedPart,
    database_path: &std::path::Path,
) -> Result<MessagePart, SessionStoreError> {
    let part = match kind {
        "text" => text.map(MessagePart::Text),
        "reasoning" => text.map(MessagePart::Reasoning),
        "tool_call" => match (call_id, name, input) {
            (Some(id), Some(name), Some(input)) => Some(MessagePart::ToolCall { id, name, input }),
            _ => None,
        },
        "tool_result" => match (call_id, content, is_error) {
            (Some(tool_call_id), Some(content), Some(is_error)) => Some(MessagePart::ToolResult {
                tool_call_id,
                content,
                is_error,
            }),
            _ => None,
        },
        "media" => match (media_id, mime) {
            (Some(media_id), Some(mime)) => Some(MessagePart::Media { media_id, mime }),
            _ => None,
        },
        _ => None,
    };
    part.ok_or_else(|| {
        SessionStoreError::operation("decode session message part", database_path, "invalid part")
    })
}

fn encode_retry_media_ids(media_ids: &[i64]) -> Option<String> {
    if media_ids.is_empty() {
        None
    } else {
        Some(
            serde_json::to_string(media_ids)
                .expect("media id list serializes to compact JSON array"),
        )
    }
}

fn decode_retry_media_ids(
    value: Option<&str>,
    database_path: &std::path::Path,
) -> Result<Vec<i64>, SessionStoreError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };

    serde_json::from_str(value).map_err(|error| {
        SessionStoreError::operation("decode retry media ids", database_path, error)
    })
}

#[cfg(test)]
mod session_page_statement_tests {
    use std::cell::Cell;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use rusqlite::trace::{TraceEvent, TraceEventCodes};

    use super::*;

    static DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    thread_local! {
        static STATEMENT_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    fn count_statement(event: TraceEvent<'_>) {
        if matches!(event, TraceEvent::Stmt(_, _)) {
            STATEMENT_COUNT.with(|count| count.set(count.get() + 1));
        }
    }

    #[test]
    fn five_hundred_one_session_page_executes_exactly_one_sql_statement() {
        let directory = std::env::temp_dir().join(format!(
            "agens-session-page-statement-count-{}-{}",
            std::process::id(),
            DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let mut store = SessionStore::open(&directory).unwrap();
        for id in 1..=501 {
            let metadata = SessionMetadata {
                id,
                project: if id % 2 == 0 { "current" } else { "other" }.into(),
                title: format!("session-{id}"),
                active_agent: "primary".into(),
                provider_id: None,
                model_id: None,
                reasoning_effort: None,
                created_at: id,
                updated_at: id / 2,
                completed_turn_count: 0,
                resumable: false,
                parent_session_id: None,
                fork_message_count: None,
            };
            store
                .begin_session_attempt(&metadata, format!("private-{id}"))
                .unwrap();
        }

        store
            .connection
            .trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_statement));
        STATEMENT_COUNT.with(|count| count.set(0));

        let page = store.list_session_page(None, "", None, 64).unwrap();
        let statement_count = STATEMENT_COUNT.with(Cell::get);
        store.connection.trace_v2(TraceEventCodes::empty(), None);

        assert_eq!(page.sessions.len(), 64);
        assert!(page.next_cursor.is_some());
        assert_eq!(statement_count, 1);

        fs::remove_dir_all(directory).unwrap();
    }
}

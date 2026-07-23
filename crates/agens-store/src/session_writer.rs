use agens_core::{
    AttemptFinishOutcome, AttemptKey, BeginSessionAttemptError, CompletedSessionTurn,
    MAX_RETRY_PROMPT_BYTES, Message, MessagePart, ReasoningEffort, RecoveryOutcome, RequestConfig,
    RetryBoundary, Role, SessionAttemptFailureKind, SessionAttemptStatus, SessionAttemptSummary,
    SessionMetadata,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::{SessionStore, SessionStoreError};

type PersistedPart = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<bool>,
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSession {
    pub metadata: SessionMetadata,
    pub messages: Vec<Message>,
    pub latest_attempt: Option<SessionAttemptSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPage {
    pub total_count: u64,
    pub sessions: Vec<StoredSession>,
}

impl SessionStore {
    pub fn begin_session_attempt(
        &mut self,
        metadata: &SessionMetadata,
        retry_prompt: String,
    ) -> Result<SessionAttemptSummary, BeginSessionAttemptError> {
        if retry_prompt.is_empty() || retry_prompt.len() > MAX_RETRY_PROMPT_BYTES {
            return Err(BeginSessionAttemptError::Store);
        }
        validate_attempt_metadata(metadata).map_err(|_| BeginSessionAttemptError::Store)?;
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
        let sequence = next_sequence(
            &transaction,
            &self.database_path,
            "session_attempts",
            session_id,
        )
        .map_err(|_| BeginSessionAttemptError::Store)?;
        transaction
            .execute(
                "INSERT INTO session_attempts(session_id, sequence, status, retry_prompt, started_at)
                 VALUES (?1, ?2, 'running', ?3, ?4)",
                params![session_id, sequence, retry_prompt, metadata.updated_at],
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
        let expected_turn_count =
            i64::try_from(metadata.completed_turn_count).map_err(|error| {
                SessionStoreError::operation(
                    "validate session metadata",
                    &self.database_path,
                    error,
                )
            })?;
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
            expected_turn_count,
            turn,
            finished_at,
        )?;
        let changed = transaction
            .execute(
                "UPDATE session_attempts
                 SET status = 'completed', retry_prompt = NULL, finished_at = ?1, completed_turn_sequence = ?2
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

    pub fn list_sessions(&self) -> Result<Vec<SessionMetadata>, SessionStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, project, title, active_agent, created_at, updated_at, completed_turn_count, resumable,
                        provider_id, model_id, reasoning_effort
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
        offset: u64,
    ) -> Result<SessionPage, SessionStoreError> {
        let transaction = self.connection.unchecked_transaction().map_err(|error| {
            SessionStoreError::operation("start session page", &self.database_path, error)
        })?;
        let scope = project.map(str::to_owned);
        let offset = i64::try_from(offset).map_err(|error| {
            SessionStoreError::operation("validate session page offset", &self.database_path, error)
        })?;
        let count = transaction
            .query_row(
                "SELECT COUNT(*) FROM sessions
                 WHERE (completed_turn_count > 0 OR EXISTS (
                     SELECT 1 FROM session_attempts
                     WHERE session_attempts.session_id = sessions.id
                       AND session_attempts.retry_prompt IS NOT NULL
                 )) AND (?1 IS NULL OR project = ?1)",
                [scope.as_deref()],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| {
                SessionStoreError::operation("count session page", &self.database_path, error)
            })?;
        let mut statement = transaction.prepare(
            "SELECT id, project, title, active_agent, created_at, updated_at, completed_turn_count, resumable,
                    provider_id, model_id, reasoning_effort
             FROM sessions
             WHERE (completed_turn_count > 0 OR EXISTS (
                 SELECT 1 FROM session_attempts
                 WHERE session_attempts.session_id = sessions.id
                   AND session_attempts.retry_prompt IS NOT NULL
             )) AND (?1 IS NULL OR project = ?1)
             ORDER BY updated_at DESC, id DESC LIMIT 64 OFFSET ?2",
        ).map_err(|error| SessionStoreError::operation("prepare session page", &self.database_path, error))?;
        let sessions = statement
            .query_map(params![scope, offset], session_metadata)
            .map_err(|error| {
                SessionStoreError::operation("query session page", &self.database_path, error)
            })?
            .map(|row| {
                let metadata = row.map_err(|error| {
                    SessionStoreError::operation("read session page", &self.database_path, error)
                })?;
                let latest_attempt =
                    latest_attempt_summary(&transaction, &self.database_path, metadata.id)?;
                Ok(StoredSession {
                    metadata,
                    messages: Vec::new(),
                    latest_attempt,
                })
            })
            .collect::<Result<Vec<_>, SessionStoreError>>()?;
        drop(statement);
        transaction.commit().map_err(|error| {
            SessionStoreError::operation("commit session page", &self.database_path, error)
        })?;
        Ok(SessionPage {
            total_count: count.try_into().map_err(|error| {
                SessionStoreError::operation("count session page", &self.database_path, error)
            })?,
            sessions,
        })
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
        let prompt = self
            .connection
            .query_row(
                "SELECT retry_prompt FROM session_attempts WHERE id = ?1 AND session_id = ?2",
                params![key.attempt_id(), key.session_id()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|error| {
                SessionStoreError::operation(
                    "load attempt retry boundary",
                    &self.database_path,
                    error,
                )
            })?
            .flatten();

        prompt
            .map(|prompt| {
                RetryBoundary::new(key, prompt).map_err(|error| {
                    SessionStoreError::operation(
                        "validate attempt retry boundary",
                        &self.database_path,
                        format!("{error:?}"),
                    )
                })
            })
            .transpose()
    }

    pub fn load_session_for_resume(&self, id: i64) -> Result<StoredSession, SessionStoreError> {
        let metadata = self
            .connection
            .query_row(
                "SELECT id, project, title, active_agent, created_at, updated_at, completed_turn_count, resumable,
                        provider_id, model_id, reasoning_effort
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
            "SELECT messages.sequence, role, kind, text, call_id, name, input_json, content, is_error
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
                ))
            })
            .map_err(|error| {
                SessionStoreError::operation("query session messages", &self.database_path, error)
            })?;
        let mut messages = Vec::new();
        let mut sequence = None;
        for row in rows {
            let (message_sequence, role, kind, text, call_id, name, input, content, is_error) = row
                .map_err(|error| {
                    SessionStoreError::operation(
                        "read session messages",
                        &self.database_path,
                        error,
                    )
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
                    (text, call_id, name, input, content, is_error),
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
        let expected_turn_count =
            i64::try_from(metadata.completed_turn_count).map_err(|error| {
                SessionStoreError::operation(
                    "validate session metadata",
                    &self.database_path,
                    error,
                )
            })?;
        let mut persisted_metadata = metadata.clone();
        persisted_metadata.completed_turn_count = persisted_metadata
            .completed_turn_count
            .checked_add(1)
            .ok_or_else(|| {
                SessionStoreError::operation(
                    "validate session metadata",
                    &self.database_path,
                    "completed turn count overflow",
                )
            })?;
        persisted_metadata.resumable = true;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation("start session turn", &self.database_path, error)
            })?;
        persisted_metadata.id =
            insert_attempt_session(&transaction, &self.database_path, metadata)?;
        let metadata = &persisted_metadata;
        persist_completed_turn_in_transaction(
            &transaction,
            &self.database_path,
            metadata,
            expected_turn_count,
            turn,
            metadata.updated_at,
        )?;
        transaction.commit().map_err(|error| {
            SessionStoreError::operation("commit session turn", &self.database_path, error)
        })?;
        Ok(persisted_metadata)
    }
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
            |row| {
                let status = decode_attempt_status(&row.get::<_, String>(2)?)?;
                let failure_kind = row
                    .get::<_, Option<String>>(3)?
                    .map(|value| decode_attempt_failure_kind(&value))
                    .transpose()?;
                SessionAttemptSummary::new(
                    AttemptKey::new(session_id, row.get(0)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    row.get::<_, i64>(1)?
                        .try_into()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    status,
                    failure_kind,
                    row.get(4)?,
                    row.get(5)?,
                )
                .map_err(|_| rusqlite::Error::InvalidQuery)
            },
        )
        .optional()
        .map_err(|error| SessionStoreError::operation("load session attempt", database_path, error))
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

fn insert_attempt_session(
    transaction: &Transaction<'_>,
    database_path: &std::path::Path,
    metadata: &SessionMetadata,
) -> Result<i64, SessionStoreError> {
    if metadata.id == 0 {
        transaction
            .execute(
                "INSERT INTO sessions (project, title, active_agent, provider_id, model_id,
                                        reasoning_effort, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
            "INSERT INTO sessions (id, project, title, active_agent, provider_id, model_id, reasoning_effort, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT(id) DO NOTHING",
            params![metadata.id, metadata.project, metadata.title, metadata.active_agent, metadata.provider_id, metadata.model_id, metadata.reasoning_effort.map(ReasoningEffort::as_str), metadata.created_at, metadata.updated_at],
        )
        .map_err(|error| SessionStoreError::operation("create session", database_path, error))?;
    Ok(metadata.id)
}

fn persist_completed_turn_in_transaction(
    transaction: &Transaction<'_>,
    database_path: &std::path::Path,
    metadata: &SessionMetadata,
    expected_turn_count: i64,
    turn: &CompletedSessionTurn,
    completed_at: i64,
) -> Result<i64, SessionStoreError> {
    if transaction
        .execute(
            "UPDATE sessions SET active_agent = ?1, provider_id = ?2, model_id = ?3,
                reasoning_effort = ?4, updated_at = ?5,
                completed_turn_count = completed_turn_count + 1, resumable = 1
             WHERE id = ?6 AND completed_turn_count = ?7",
            params![
                metadata.active_agent,
                metadata.provider_id,
                metadata.model_id,
                metadata.reasoning_effort.map(ReasoningEffort::as_str),
                completed_at,
                metadata.id,
                expected_turn_count
            ],
        )
        .map_err(|error| SessionStoreError::operation("update session", database_path, error))?
        != 1
    {
        return Err(SessionStoreError::operation(
            "update session",
            database_path,
            "completed turn count changed",
        ));
    }

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

    Ok(turn_sequence)
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
        _ => Err(SessionStoreError::operation(
            "decode session message",
            database_path,
            "invalid role",
        )),
    }
}

fn decode_part(
    kind: &str,
    (text, call_id, name, input, content, is_error): PersistedPart,
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
        _ => None,
    };
    part.ok_or_else(|| {
        SessionStoreError::operation("decode session message part", database_path, "invalid part")
    })
}

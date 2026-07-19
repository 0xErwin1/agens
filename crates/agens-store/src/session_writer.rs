use agens_core::{CompletedSessionTurn, MessagePart, Role, SessionMetadata};
use rusqlite::{Transaction, TransactionBehavior, params};

use super::{SessionStore, SessionStoreError};

impl SessionStore {
    pub fn persist_completed_session_turn(
        &mut self,
        metadata: &SessionMetadata,
        turn: &CompletedSessionTurn,
    ) -> Result<(), SessionStoreError> {
        metadata.validate().map_err(|error| {
            SessionStoreError::operation(
                "validate session metadata",
                &self.database_path,
                format!("{error:?}"),
            )
        })?;
        validate_text_turn(turn, &self.database_path)?;
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
                SessionStoreError::operation("start session turn", &self.database_path, error)
            })?;
        transaction
            .execute(
                "INSERT INTO sessions (id, project, title, active_agent, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id) DO NOTHING",
                params![
                    metadata.id,
                    metadata.project,
                    metadata.title,
                    metadata.active_agent,
                    metadata.created_at,
                    metadata.updated_at
                ],
            )
            .map_err(|error| {
                SessionStoreError::operation("create session", &self.database_path, error)
            })?;
        if transaction.execute(
            "UPDATE sessions SET active_agent = ?1, updated_at = ?2, completed_turn_count = completed_turn_count + 1, resumable = 1
             WHERE id = ?3 AND completed_turn_count = ?4",
            params![metadata.active_agent, metadata.updated_at, metadata.id, expected_turn_count],
        ).map_err(|error| SessionStoreError::operation("update session", &self.database_path, error))? != 1 {
            return Err(SessionStoreError::operation("update session", &self.database_path, "completed turn count changed"));
        }

        let turn_sequence = next_sequence(&transaction, &self.database_path, "turns", metadata.id)?;
        transaction
            .execute(
                "INSERT INTO turns (session_id, sequence, completed_at) VALUES (?1, ?2, ?3)",
                params![metadata.id, turn_sequence, metadata.updated_at],
            )
            .map_err(|error| {
                SessionStoreError::operation("create turn", &self.database_path, error)
            })?;
        let first_message_sequence =
            next_sequence(&transaction, &self.database_path, "messages", metadata.id)?;
        for (message_offset, message) in turn.messages().iter().enumerate() {
            let message_sequence = first_message_sequence + message_offset as i64;
            transaction.execute(
                "INSERT INTO messages (session_id, sequence, turn_sequence, role) VALUES (?1, ?2, ?3, ?4)",
                params![metadata.id, message_sequence, turn_sequence, role_name(message.role)],
            ).map_err(|error| SessionStoreError::operation("create message", &self.database_path, error))?;
            for (part_sequence, part) in message.parts.iter().enumerate() {
                let MessagePart::Text(text) = part else {
                    unreachable!("validated text turn")
                };
                transaction.execute(
                    "INSERT INTO message_parts (session_id, message_sequence, sequence, kind, text) VALUES (?1, ?2, ?3, 'text', ?4)",
                    params![metadata.id, message_sequence, part_sequence as i64, text],
                ).map_err(|error| SessionStoreError::operation("create message part", &self.database_path, error))?;
            }
        }
        transaction.commit().map_err(|error| {
            SessionStoreError::operation("commit session turn", &self.database_path, error)
        })
    }
}

fn validate_text_turn(
    turn: &CompletedSessionTurn,
    path: &std::path::Path,
) -> Result<(), SessionStoreError> {
    if turn.messages().iter().all(|message| {
        message
            .parts
            .iter()
            .all(|part| matches!(part, MessagePart::Text(_)))
    }) {
        Ok(())
    } else {
        Err(SessionStoreError::operation(
            "validate session turn",
            path,
            "text-only writer does not support this message part",
        ))
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

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => unreachable!("validated text turn"),
    }
}

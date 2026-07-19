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
                params![metadata.id, message_sequence, turn_sequence, encode_role(message.role)],
            ).map_err(|error| SessionStoreError::operation("create message", &self.database_path, error))?;
            for (part_sequence, part) in message.parts.iter().enumerate() {
                insert_message_part(
                    &transaction,
                    &self.database_path,
                    metadata.id,
                    message_sequence,
                    part_sequence as i64,
                    part,
                )?;
            }
        }
        transaction.commit().map_err(|error| {
            SessionStoreError::operation("commit session turn", &self.database_path, error)
        })
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

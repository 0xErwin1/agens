use std::{
    fmt,
    path::{Path, PathBuf},
};

use agens_core::{
    CompletedTurnRepository, CompletedTurnSnapshot, CompletedTurnStoreError, MessagePart,
    PermissionDecision, PermissionPattern, ProjectPermissionGrant, ReasoningEffort, RequestConfig,
    TurnEvent, TurnState,
};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

mod database;
mod session_writer;
pub use session_writer::{SessionCursor, SessionPage, StoredSession};

const PERMISSION_GRANTS_COLUMNS: [ExpectedColumnSignature; 7] = [
    ExpectedColumnSignature::new(0, "id", "INTEGER", false, None, 1),
    ExpectedColumnSignature::new(1, "project", "TEXT", true, None, 0),
    ExpectedColumnSignature::new(2, "decision", "TEXT", true, None, 0),
    ExpectedColumnSignature::new(3, "tool_kind", "TEXT", true, None, 0),
    ExpectedColumnSignature::new(4, "tool_value", "TEXT", false, None, 0),
    ExpectedColumnSignature::new(5, "target_kind", "TEXT", true, None, 0),
    ExpectedColumnSignature::new(6, "target_value", "TEXT", false, None, 0),
];
const PERMISSION_GRANTS_INDEX: ExpectedIndexSignature =
    ExpectedIndexSignature::new(0, "permission_grants_project", false, "c", false);
const PERMISSION_GRANTS_INDEX_COLUMNS: [ExpectedIndexColumnSignature; 2] = [
    ExpectedIndexColumnSignature::new(0, 1, "project"),
    ExpectedIndexColumnSignature::new(1, 0, "id"),
];

const MAX_PREFERENCE_MODEL_BYTES: usize = 64;

#[derive(Debug, PartialEq, Eq)]
struct ExpectedColumnSignature {
    column_id: i64,
    name: &'static str,
    declared_type: &'static str,
    not_null: bool,
    default_value: Option<&'static str>,
    primary_key_position: i64,
}

impl ExpectedColumnSignature {
    const fn new(
        column_id: i64,
        name: &'static str,
        declared_type: &'static str,
        not_null: bool,
        default_value: Option<&'static str>,
        primary_key_position: i64,
    ) -> Self {
        Self {
            column_id,
            name,
            declared_type,
            not_null,
            default_value,
            primary_key_position,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ExpectedIndexSignature {
    sequence: i64,
    name: &'static str,
    unique: bool,
    origin: &'static str,
    partial: bool,
}

impl ExpectedIndexSignature {
    const fn new(
        sequence: i64,
        name: &'static str,
        unique: bool,
        origin: &'static str,
        partial: bool,
    ) -> Self {
        Self {
            sequence,
            name,
            unique,
            origin,
            partial,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ExpectedIndexColumnSignature {
    sequence: i64,
    column_id: i64,
    name: &'static str,
}

impl ExpectedIndexColumnSignature {
    const fn new(sequence: i64, column_id: i64, name: &'static str) -> Self {
        Self {
            sequence,
            column_id,
            name,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionGrantStoreError {
    message: String,
}

impl PermissionGrantStoreError {
    fn operation(operation: &str, path: &Path, error: impl fmt::Display) -> Self {
        Self {
            message: format!(
                "permission grants {operation} at {}: {error}",
                path.display()
            ),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn from_database(error: database::DatabaseError) -> Self {
        Self::operation(error.operation(), error.path(), error.detail())
    }
}

impl fmt::Display for PermissionGrantStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PermissionGrantStoreError {}

pub struct PermissionGrantStore {
    database_path: PathBuf,
    connection: Connection,
}

impl PermissionGrantStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, PermissionGrantStoreError> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(PermissionGrantStoreError::from_database)?;
        verify_schema(&connection, &database_path)?;

        Ok(Self {
            database_path,
            connection,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.database_path.clone()
    }
}

impl PermissionGrantStore {
    pub fn append_grants(
        &mut self,
        grants: &[ProjectPermissionGrant],
    ) -> Result<(), PermissionGrantStoreError> {
        for grant in grants {
            validate_grant(grant)?;
        }

        let transaction = self.connection.transaction().map_err(|error| {
            PermissionGrantStoreError::operation("start transaction", &self.database_path, error)
        })?;
        for grant in grants {
            insert_grant(&transaction, grant)?;
        }
        transaction.commit().map_err(|error| {
            PermissionGrantStoreError::operation("commit transaction", &self.database_path, error)
        })
    }

    pub fn grants_for_project(
        &self,
        project: &str,
    ) -> Result<Vec<ProjectPermissionGrant>, PermissionGrantStoreError> {
        if project.trim().is_empty() {
            return Err(PermissionGrantStoreError::invalid("project is required"));
        }

        let mut statement = self
            .connection
            .prepare(
                "SELECT decision, tool_kind, tool_value, target_kind, target_value
                 FROM permission_grants WHERE project = ?1 ORDER BY id",
            )
            .map_err(|error| {
                PermissionGrantStoreError::operation(
                    "prepare project lookup",
                    &self.database_path,
                    error,
                )
            })?;
        let rows = statement
            .query_map([project], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(|error| {
                PermissionGrantStoreError::operation(
                    "query project grants",
                    &self.database_path,
                    error,
                )
            })?;

        rows.map(|row| {
            let (decision, tool_kind, tool_value, target_kind, target_value) =
                row.map_err(|error| {
                    PermissionGrantStoreError::operation(
                        "read project grants",
                        &self.database_path,
                        error,
                    )
                })?;
            let decision = decode_decision(&decision).map_err(|error| {
                PermissionGrantStoreError::operation(
                    "decode project grant",
                    &self.database_path,
                    error,
                )
            })?;
            let tool = decode_pattern(&tool_kind, tool_value).map_err(|error| {
                PermissionGrantStoreError::operation(
                    "decode project grant",
                    &self.database_path,
                    error,
                )
            })?;
            let target = decode_pattern(&target_kind, target_value).map_err(|error| {
                PermissionGrantStoreError::operation(
                    "decode project grant",
                    &self.database_path,
                    error,
                )
            })?;

            Ok(ProjectPermissionGrant::new(project, decision, tool, target))
        })
        .collect()
    }
}

fn verify_schema(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), PermissionGrantStoreError> {
    let table_matches = permission_grants_table_matches(connection).map_err(|error| {
        PermissionGrantStoreError::operation("verify schema", database_path, error)
    })?;
    let index_matches = permission_grants_index_matches(connection).map_err(|error| {
        PermissionGrantStoreError::operation("verify schema", database_path, error)
    })?;

    if table_matches && index_matches {
        return Ok(());
    }

    Err(PermissionGrantStoreError::operation(
        "verify schema",
        database_path,
        "incompatible permission grants schema",
    ))
}

fn permission_grants_table_matches(connection: &Connection) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare("PRAGMA table_info('permission_grants')")?;
    let columns = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(columns.len() == PERMISSION_GRANTS_COLUMNS.len()
        && columns.iter().zip(PERMISSION_GRANTS_COLUMNS).all(
            |(
                (column_id, name, declared_type, not_null, default_value, primary_key_position),
                expected,
            )| {
                *column_id == expected.column_id
                    && name == expected.name
                    && declared_type == expected.declared_type
                    && *not_null == expected.not_null
                    && default_value.as_deref() == expected.default_value
                    && *primary_key_position == expected.primary_key_position
            },
        ))
}

fn permission_grants_index_matches(connection: &Connection) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare("PRAGMA index_list('permission_grants')")?;
    let indexes = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let indexes_match = indexes.len() == 1
        && indexes
            .first()
            .is_some_and(|(sequence, name, unique, origin, partial)| {
                *sequence == PERMISSION_GRANTS_INDEX.sequence
                    && name == PERMISSION_GRANTS_INDEX.name
                    && *unique == PERMISSION_GRANTS_INDEX.unique
                    && origin == PERMISSION_GRANTS_INDEX.origin
                    && *partial == PERMISSION_GRANTS_INDEX.partial
            });

    if !indexes_match {
        return Ok(false);
    }

    let mut statement = connection.prepare("PRAGMA index_info('permission_grants_project')")?;
    let columns = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(columns.len() == PERMISSION_GRANTS_INDEX_COLUMNS.len()
        && columns.iter().zip(PERMISSION_GRANTS_INDEX_COLUMNS).all(
            |((sequence, column_id, name), expected)| {
                *sequence == expected.sequence
                    && *column_id == expected.column_id
                    && name == expected.name
            },
        ))
}

fn insert_grant(
    transaction: &Transaction<'_>,
    grant: &ProjectPermissionGrant,
) -> Result<(), PermissionGrantStoreError> {
    let (tool_kind, tool_value) = encode_pattern(&grant.tool);
    let (target_kind, target_value) = encode_pattern(&grant.target);
    transaction
        .execute(
            "INSERT INTO permission_grants
             (project, decision, tool_kind, tool_value, target_kind, target_value)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                grant.project,
                encode_decision(grant.decision),
                tool_kind,
                tool_value,
                target_kind,
                target_value,
            ],
        )
        .map_err(|error| PermissionGrantStoreError::invalid(error.to_string()))?;
    Ok(())
}

fn validate_grant(grant: &ProjectPermissionGrant) -> Result<(), PermissionGrantStoreError> {
    if grant.project.trim().is_empty() {
        return Err(PermissionGrantStoreError::invalid("project is required"));
    }

    if matches!(&grant.tool, PermissionPattern::Exact(tool) if tool.is_empty()) {
        return Err(PermissionGrantStoreError::invalid("grant tool is required"));
    }

    Ok(())
}

fn encode_decision(decision: PermissionDecision) -> &'static str {
    match decision {
        PermissionDecision::Allow => "allow",
        PermissionDecision::Ask => "ask",
        PermissionDecision::Deny => "deny",
    }
}

fn decode_decision(value: &str) -> Result<PermissionDecision, PermissionGrantStoreError> {
    match value {
        "allow" => Ok(PermissionDecision::Allow),
        "ask" => Ok(PermissionDecision::Ask),
        "deny" => Ok(PermissionDecision::Deny),
        _ => Err(PermissionGrantStoreError::invalid(
            "invalid stored grant decision",
        )),
    }
}

fn encode_pattern(pattern: &PermissionPattern) -> (&'static str, Option<&str>) {
    match pattern {
        PermissionPattern::Any => ("any", None),
        PermissionPattern::Exact(value) => ("exact", Some(value)),
        PermissionPattern::Glob(_) => ("glob", pattern.glob_source()),
    }
}

fn decode_pattern(
    kind: &str,
    value: Option<String>,
) -> Result<PermissionPattern, PermissionGrantStoreError> {
    match (kind, value) {
        ("any", None) => Ok(PermissionPattern::Any),
        ("exact", Some(value)) if !value.is_empty() => Ok(PermissionPattern::Exact(value)),
        ("glob", Some(value)) => PermissionPattern::glob(value)
            .map_err(|_| PermissionGrantStoreError::invalid("invalid stored grant pattern")),
        _ => Err(PermissionGrantStoreError::invalid(
            "invalid stored grant pattern",
        )),
    }
}

/// The model and reasoning effort a user last chose explicitly.
///
/// Both travel together: an effort that outlived its model would be applied to a model that never
/// supported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPreference {
    model: String,
    reasoning_effort: Option<ReasoningEffort>,
}

impl ModelPreference {
    pub fn new(model: impl Into<String>, reasoning_effort: Option<ReasoningEffort>) -> Self {
        Self {
            model: model.into(),
            reasoning_effort,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub const fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreferenceStoreError {
    message: String,
}

impl PreferenceStoreError {
    fn operation(operation: &str, path: &Path, error: impl fmt::Display) -> Self {
        Self {
            message: format!("preferences {operation} at {}: {error}", path.display()),
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

impl fmt::Display for PreferenceStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PreferenceStoreError {}

/// Runtime state for choices the user expects to outlive a session.
///
/// It shares the unified `agens.db` file with sessions and permission grants, each behind its
/// own table with no cross-schema relationship; hand-authored configuration stays untouched.
pub struct PreferenceStore {
    database_path: PathBuf,
    connection: Connection,
}

impl PreferenceStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, PreferenceStoreError> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(PreferenceStoreError::from_database)?;

        Ok(Self {
            database_path,
            connection,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.database_path.clone()
    }

    pub fn remember_model(
        &mut self,
        preference: &ModelPreference,
    ) -> Result<(), PreferenceStoreError> {
        if !valid_preference_model(&preference.model) {
            return Err(PreferenceStoreError::detail(
                "remembered model identifier is invalid",
            ));
        }

        self.connection
            .execute(
                "INSERT INTO model_preference (id, model, reasoning_effort)
                 VALUES (1, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET model = ?1, reasoning_effort = ?2",
                params![
                    preference.model,
                    preference.reasoning_effort.map(ReasoningEffort::as_str),
                ],
            )
            .map_err(|error| {
                PreferenceStoreError::operation("save model preference", &self.database_path, error)
            })?;
        Ok(())
    }

    pub fn remembered_model(&self) -> Result<Option<ModelPreference>, PreferenceStoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT model, reasoning_effort FROM model_preference WHERE id = 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                error => Err(PreferenceStoreError::operation(
                    "read model preference",
                    &self.database_path,
                    error,
                )),
            })?;
        let Some((model, reasoning_effort)) = row else {
            return Ok(None);
        };
        if !valid_preference_model(&model) {
            return Err(PreferenceStoreError::detail(
                "stored model preference is invalid",
            ));
        }
        let reasoning_effort = reasoning_effort
            .map(|effort| {
                RequestConfig::with_reasoning_effort(&effort)
                    .map_err(|_| {
                        PreferenceStoreError::detail("stored reasoning effort is unsupported")
                    })?
                    .reasoning_effort()
                    .ok_or_else(|| {
                        PreferenceStoreError::detail("stored reasoning effort is unsupported")
                    })
            })
            .transpose()?;

        Ok(Some(ModelPreference {
            model,
            reasoning_effort,
        }))
    }
}

fn valid_preference_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= MAX_PREFERENCE_MODEL_BYTES
        && model.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

const LEGACY_TURN_EVENTS_INDEX_COLUMNS: [ExpectedIndexColumnSignature; 2] = [
    ExpectedIndexColumnSignature::new(0, 0, "turn_id"),
    ExpectedIndexColumnSignature::new(1, 1, "sequence"),
];
const LEGACY_TURNS_COLUMNS: [ExpectedColumnSignature; 4] = [
    ExpectedColumnSignature::new(0, "id", "INTEGER", false, None, 1),
    ExpectedColumnSignature::new(1, "status", "TEXT", true, None, 0),
    ExpectedColumnSignature::new(2, "reason", "TEXT", true, None, 0),
    ExpectedColumnSignature::new(3, "source_event_count", "INTEGER", true, None, 0),
];
const LEGACY_TURN_EVENTS_COLUMNS: [ExpectedColumnSignature; 10] = [
    ExpectedColumnSignature::new(0, "turn_id", "INTEGER", true, None, 1),
    ExpectedColumnSignature::new(1, "sequence", "INTEGER", true, None, 2),
    ExpectedColumnSignature::new(2, "kind", "TEXT", true, None, 0),
    ExpectedColumnSignature::new(3, "state", "TEXT", false, None, 0),
    ExpectedColumnSignature::new(4, "part_kind", "TEXT", false, None, 0),
    ExpectedColumnSignature::new(5, "call_id", "TEXT", false, None, 0),
    ExpectedColumnSignature::new(6, "name", "TEXT", false, None, 0),
    ExpectedColumnSignature::new(7, "input", "TEXT", false, None, 0),
    ExpectedColumnSignature::new(8, "content", "TEXT", false, None, 0),
    ExpectedColumnSignature::new(9, "is_error", "INTEGER", false, None, 0),
];
const LEGACY_TURN_EVENTS_INDEXES: [ExpectedIndexSignature; 2] = [
    ExpectedIndexSignature::new(0, "legacy_turn_events_turn_sequence", true, "c", false),
    ExpectedIndexSignature::new(
        1,
        "sqlite_autoindex_legacy_turn_events_1",
        true,
        "pk",
        false,
    ),
];
const NORMALIZED_SESSION_SCHEMA_V2: &str = "
    CREATE TABLE sessions (
        id INTEGER PRIMARY KEY,
        project TEXT NOT NULL CHECK(project <> ''),
        title TEXT NOT NULL,
        active_agent TEXT NOT NULL CHECK(active_agent <> ''),
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        completed_turn_count INTEGER NOT NULL DEFAULT 0 CHECK(completed_turn_count >= 0),
        resumable INTEGER NOT NULL DEFAULT 0 CHECK(resumable IN(0, 1)),
        CHECK(resumable = (completed_turn_count > 0))
    );
    CREATE TABLE turns (
        session_id INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence > 0),
        completed_at INTEGER NOT NULL,
        PRIMARY KEY(session_id, sequence),
        FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
    );
    CREATE TABLE messages (
        session_id INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence > 0),
        turn_sequence INTEGER NOT NULL CHECK(turn_sequence > 0),
        role TEXT NOT NULL CHECK(role IN('system', 'user', 'assistant', 'tool')),
        PRIMARY KEY(session_id, sequence),
        FOREIGN KEY(session_id, turn_sequence) REFERENCES turns(session_id, sequence) ON DELETE CASCADE
    );
    CREATE TABLE message_parts (
        session_id INTEGER NOT NULL,
        message_sequence INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence >= 0),
        kind TEXT NOT NULL CHECK(kind IN('text', 'reasoning', 'tool_call', 'tool_result')),
        text TEXT,
        call_id TEXT,
        name TEXT,
        input_json TEXT,
        content TEXT,
        is_error INTEGER CHECK(is_error IN(0, 1)),
        PRIMARY KEY(session_id, message_sequence, sequence),
        FOREIGN KEY(session_id, message_sequence) REFERENCES messages(session_id, sequence) ON DELETE CASCADE,
        CHECK((kind IN('text', 'reasoning') AND text IS NOT NULL AND call_id IS NULL AND name IS NULL AND input_json IS NULL AND content IS NULL AND is_error IS NULL) OR (kind = 'tool_call' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NOT NULL AND name <> '' AND input_json IS NOT NULL AND content IS NULL AND is_error IS NULL) OR (kind = 'tool_result' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NULL AND input_json IS NULL AND content IS NOT NULL AND is_error IS NOT NULL))
    );
    CREATE INDEX sessions_list ON sessions(resumable, updated_at DESC, id DESC);
    CREATE INDEX messages_turn_order ON messages(session_id, turn_sequence, sequence);
    CREATE INDEX parts_message_order ON message_parts(session_id, message_sequence, sequence);
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStoreError {
    message: String,
}

impl SessionStoreError {
    fn operation(operation: &str, path: &Path, error: impl fmt::Display) -> Self {
        Self {
            message: format!("sessions {operation} at {}: {error}", path.display()),
        }
    }

    fn from_database(error: database::DatabaseError) -> Self {
        Self::operation(error.operation(), error.path(), error.detail())
    }
}

impl fmt::Display for SessionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SessionStoreError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCompletedTurn {
    pub id: i64,
    pub snapshot: CompletedTurnSnapshot,
}

pub struct SessionStore {
    database_path: PathBuf,
    connection: Connection,
}

impl SessionStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, SessionStoreError> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(SessionStoreError::from_database)?;

        validate_v5_schema(&connection, &database_path)?;

        Ok(Self {
            database_path,
            connection,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.database_path.clone()
    }

    pub fn list_completed_turns(&self) -> Result<Vec<StoredCompletedTurn>, SessionStoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM legacy_turns ORDER BY id")
            .map_err(|error| {
                SessionStoreError::operation(
                    "prepare completed turn list",
                    &self.database_path,
                    error,
                )
            })?;
        let ids = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(|error| {
                SessionStoreError::operation(
                    "query completed turn list",
                    &self.database_path,
                    error,
                )
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| {
                SessionStoreError::operation("read completed turn list", &self.database_path, error)
            })?;

        ids.into_iter()
            .map(|id| {
                self.load_legacy_completed_turn(id)
                    .map(|snapshot| StoredCompletedTurn { id, snapshot })
            })
            .collect()
    }

    pub fn load_completed_turn_for_resume(
        &self,
        id: i64,
    ) -> Result<CompletedTurnSnapshot, SessionStoreError> {
        let exists = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM legacy_turns WHERE id = ?1)",
                [id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| {
                SessionStoreError::operation("check completed turn", &self.database_path, error)
            })?;
        if exists {
            return Err(SessionStoreError::operation(
                "load completed turn",
                &self.database_path,
                format!("legacy completed turn {id} is non-resumable"),
            ));
        }

        Err(SessionStoreError::operation(
            "load completed turn",
            &self.database_path,
            format!("unknown completed turn {id}"),
        ))
    }

    fn load_legacy_completed_turn(
        &self,
        id: i64,
    ) -> Result<CompletedTurnSnapshot, SessionStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT kind, state, part_kind, call_id, name, input, content, is_error
             FROM legacy_turn_events WHERE turn_id = ?1 ORDER BY sequence",
            )
            .map_err(|error| {
                SessionStoreError::operation(
                    "prepare legacy completed turn events",
                    &self.database_path,
                    error,
                )
            })?;
        let rows = statement
            .query_map([id], |row| {
                Ok(PersistedTurnEvent {
                    kind: row.get(0)?,
                    state: row.get(1)?,
                    part_kind: row.get(2)?,
                    call_id: row.get(3)?,
                    name: row.get(4)?,
                    input: row.get(5)?,
                    content: row.get(6)?,
                    is_error: row.get(7)?,
                })
            })
            .map_err(|error| {
                SessionStoreError::operation(
                    "query legacy completed turn events",
                    &self.database_path,
                    error,
                )
            })?;
        let events = rows
            .map(|row| {
                let fields = row.map_err(|error| {
                    SessionStoreError::operation(
                        "read legacy completed turn events",
                        &self.database_path,
                        error,
                    )
                })?;
                decode_turn_event(fields).map_err(|error| {
                    SessionStoreError::operation(
                        "decode legacy completed turn events",
                        &self.database_path,
                        error,
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        CompletedTurnSnapshot::from_persisted_events(events).map_err(|error| {
            SessionStoreError::operation("restore completed turn", &self.database_path, error)
        })
    }

    fn store_completed_turn(
        &mut self,
        snapshot: CompletedTurnSnapshot,
    ) -> Result<(), SessionStoreError> {
        CompletedTurnSnapshot::from_persisted_events(snapshot.events().to_vec()).map_err(
            |error| {
                SessionStoreError::operation("validate completed turn", &self.database_path, error)
            },
        )?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation("start transaction", &self.database_path, error)
            })?;
        transaction
            .execute(
                "INSERT INTO legacy_turns(id, status, reason, source_event_count)
                 VALUES (NULL, 'non_resumable', 'v1 lacks session/user/project/title/agent/timestamps', 0)",
                [],
            )
            .map_err(|error| {
                SessionStoreError::operation("create completed turn", &self.database_path, error)
            })?;
        let turn_id = transaction.last_insert_rowid();

        let persisted_events = snapshot
            .events()
            .iter()
            .filter(|event| persistable_turn_event(event))
            .collect::<Vec<_>>();

        for (sequence, event) in persisted_events.iter().enumerate() {
            insert_legacy_turn_event(&transaction, turn_id, sequence as i64, event).map_err(
                |error| {
                    SessionStoreError::operation(
                        "write completed turn event",
                        &self.database_path,
                        error,
                    )
                },
            )?;
        }

        transaction
            .execute(
                "UPDATE legacy_turns SET source_event_count = ?1 WHERE id = ?2",
                params![persisted_events.len() as i64, turn_id],
            )
            .map_err(|error| {
                SessionStoreError::operation("finalize completed turn", &self.database_path, error)
            })?;

        transaction.commit().map_err(|error| {
            SessionStoreError::operation("commit transaction", &self.database_path, error)
        })
    }
}

impl CompletedTurnRepository for SessionStore {
    fn persist_completed_turn(
        &mut self,
        snapshot: CompletedTurnSnapshot,
    ) -> impl std::future::Future<Output = Result<(), CompletedTurnStoreError>> + Send {
        std::future::ready(
            self.store_completed_turn(snapshot)
                .map_err(|error| CompletedTurnStoreError::new(error.to_string())),
        )
    }
}

fn normalized_session_schema_v3() -> String {
    NORMALIZED_SESSION_SCHEMA_V2.replacen(
        "CHECK(resumable = (completed_turn_count > 0))",
        "provider_id TEXT CHECK(provider_id <> '' AND length(provider_id) <= 64),
         model_id TEXT CHECK(model_id <> '' AND length(model_id) <= 64),
         reasoning_effort TEXT CHECK(reasoning_effort IN('none', 'minimal', 'low', 'medium', 'high', 'xhigh')),
         CHECK(resumable = (completed_turn_count > 0))",
        1,
    )
}

fn normalized_session_schema_v4() -> String {
    normalized_session_schema_v3().replace("'xhigh'))", "'xhigh', 'max'))")
}

fn normalized_session_schema_v5() -> String {
    normalized_session_schema_v5_with_required_terminal_retry_prompts()
        .replace(
            "(status = 'cancelled' AND failure_kind = 'cancelled' AND retry_prompt IS NOT NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR\n                   (status = 'failed' AND failure_kind = 'failed' AND retry_prompt IS NOT NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR\n                   (status = 'provider_error' AND failure_kind = 'provider_error' AND retry_prompt IS NOT NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR\n                   (status = 'interrupted' AND failure_kind = 'interrupted' AND retry_prompt IS NOT NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL)",
            "(status = 'cancelled' AND failure_kind = 'cancelled' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR\n                   (status = 'failed' AND failure_kind = 'failed' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR\n                   (status = 'provider_error' AND failure_kind = 'provider_error' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR\n                   (status = 'interrupted' AND failure_kind = 'interrupted' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL)",
        )
}

fn normalized_session_schema_v5_with_required_terminal_retry_prompts() -> String {
    format!(
        "{}\
         CREATE TABLE session_attempts (
             id INTEGER PRIMARY KEY,
             session_id INTEGER NOT NULL,
             sequence INTEGER NOT NULL CHECK(sequence > 0),
             status TEXT NOT NULL CHECK(status IN('running', 'completed', 'cancelled', 'failed', 'provider_error', 'interrupted')),
             failure_kind TEXT CHECK(failure_kind IN('cancelled', 'failed', 'provider_error', 'interrupted')),
             retry_prompt TEXT CHECK(retry_prompt IS NULL OR (length(CAST(retry_prompt AS BLOB)) BETWEEN 1 AND 65536)),
             started_at INTEGER NOT NULL,
             finished_at INTEGER,
             completed_turn_sequence INTEGER,
             FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
             FOREIGN KEY(session_id, completed_turn_sequence) REFERENCES turns(session_id, sequence) ON DELETE SET NULL,
             CHECK((status = 'running' AND failure_kind IS NULL AND retry_prompt IS NOT NULL AND finished_at IS NULL AND completed_turn_sequence IS NULL) OR
                   (status = 'completed' AND failure_kind IS NULL AND retry_prompt IS NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NOT NULL) OR
                   (status = 'cancelled' AND failure_kind = 'cancelled' AND retry_prompt IS NOT NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
                   (status = 'failed' AND failure_kind = 'failed' AND retry_prompt IS NOT NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
                   (status = 'provider_error' AND failure_kind = 'provider_error' AND retry_prompt IS NOT NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
                   (status = 'interrupted' AND failure_kind = 'interrupted' AND retry_prompt IS NOT NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL))
         );
         CREATE UNIQUE INDEX session_attempts_session_sequence ON session_attempts(session_id, sequence);
         CREATE UNIQUE INDEX session_attempts_one_running ON session_attempts(session_id) WHERE status = 'running';
         CREATE INDEX session_attempts_latest ON session_attempts(session_id, sequence DESC, id DESC);",
        normalized_session_schema_v4()
    )
}

fn validate_legacy_archive(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    let source_table_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE type = 'table' AND name IN ('completed_turns', 'completed_turn_events')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| {
            SessionStoreError::operation("validate legacy archive", database_path, error)
        })?;
    if source_table_count != 0 && source_table_count != 2 {
        return Err(SessionStoreError::operation(
            "validate legacy archive",
            database_path,
            "incomplete legacy source schema",
        ));
    }

    let schema_matches = table_matches(connection, "legacy_turns", &LEGACY_TURNS_COLUMNS)
        .and_then(|turns_match| {
            table_matches(
                connection,
                "legacy_turn_events",
                &LEGACY_TURN_EVENTS_COLUMNS,
            )
            .map(|events_match| turns_match && events_match)
        })
        .and_then(|tables_match| {
            legacy_turn_events_foreign_key_matches(connection)
                .map(|foreign_key_matches| tables_match && foreign_key_matches)
        })
        .and_then(|foreign_keys_match| {
            legacy_turn_events_indexes_match(connection)
                .map(|indexes_match| foreign_keys_match && indexes_match)
        })
        .map_err(|error| {
            SessionStoreError::operation("validate legacy archive", database_path, error)
        })?;
    if !schema_matches {
        return Err(SessionStoreError::operation(
            "validate legacy archive",
            database_path,
            "incompatible legacy archive schema",
        ));
    }

    let validation_query = if source_table_count == 2 {
        "SELECT NOT EXISTS(
             SELECT id FROM completed_turns EXCEPT SELECT id FROM legacy_turns
         ) AND NOT EXISTS(
             SELECT id FROM legacy_turns EXCEPT SELECT id FROM completed_turns
         ) AND NOT EXISTS(
             SELECT turn_id, sequence, kind, state, part_kind, call_id, name, input, content, is_error
             FROM completed_turn_events
             EXCEPT
             SELECT turn_id, sequence, kind, state, part_kind, call_id, name, input, content, is_error
             FROM legacy_turn_events
         ) AND NOT EXISTS(
             SELECT turn_id, sequence, kind, state, part_kind, call_id, name, input, content, is_error
             FROM legacy_turn_events
             EXCEPT
             SELECT turn_id, sequence, kind, state, part_kind, call_id, name, input, content, is_error
             FROM completed_turn_events
         ) AND NOT EXISTS(
             SELECT turns.id, count(events.turn_id)
             FROM completed_turns turns
             LEFT JOIN completed_turn_events events ON events.turn_id = turns.id
             GROUP BY turns.id
             EXCEPT
             SELECT id, source_event_count FROM legacy_turns
         ) AND NOT EXISTS(
             SELECT 1 FROM legacy_turns
             WHERE status != 'non_resumable'
                OR reason != 'v1 lacks session/user/project/title/agent/timestamps'
         )"
    } else {
        "SELECT NOT EXISTS(
             SELECT 1 FROM legacy_turns turns
             WHERE turns.status != 'non_resumable'
                OR turns.reason != 'v1 lacks session/user/project/title/agent/timestamps'
                OR turns.source_event_count !=
                    (SELECT count(*) FROM legacy_turn_events WHERE turn_id = turns.id)
         )"
    };
    let archive_matches: bool = connection
        .query_row(validation_query, [], |row| row.get(0))
        .map_err(|error| {
            SessionStoreError::operation("validate legacy archive", database_path, error)
        })?;

    if archive_matches {
        Ok(())
    } else {
        Err(SessionStoreError::operation(
            "validate legacy archive",
            database_path,
            "legacy archive does not match the v1 source",
        ))
    }
}

fn validate_v5_schema(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    validate_legacy_archive(connection, database_path)?;
    validate_normalized_session_schema(connection, database_path, &normalized_session_schema_v5())
}

fn validate_normalized_session_schema(
    connection: &Connection,
    database_path: &Path,
    expected_schema: &str,
) -> Result<(), SessionStoreError> {
    let names = if expected_schema.contains("CREATE TABLE session_attempts") {
        "'sessions', 'turns', 'messages', 'message_parts',
         'sessions_list', 'messages_turn_order', 'parts_message_order',
         'session_attempts', 'session_attempts_session_sequence',
         'session_attempts_one_running', 'session_attempts_latest'"
    } else {
        "'sessions', 'turns', 'messages', 'message_parts',
         'sessions_list', 'messages_turn_order', 'parts_message_order'"
    };
    let mut statement = connection
        .prepare(&format!(
            "SELECT sql FROM sqlite_schema
             WHERE type IN ('table', 'index') AND name IN ({names})"
        ))
        .map_err(|error| {
            SessionStoreError::operation("validate normalized schema", database_path, error)
        })?;
    let mut actual = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| {
            SessionStoreError::operation("validate normalized schema", database_path, error)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| {
            SessionStoreError::operation("validate normalized schema", database_path, error)
        })?;
    let mut expected = expected_schema
        .split(';')
        .filter(|statement| statement.trim_start().starts_with("CREATE"))
        .map(normalize_schema_statement)
        .collect::<Vec<_>>();
    actual
        .iter_mut()
        .for_each(|statement| *statement = normalize_schema_statement(statement));
    actual.sort();
    expected.sort();

    if actual == expected {
        Ok(())
    } else {
        Err(SessionStoreError::operation(
            "validate normalized schema",
            database_path,
            "incompatible normalized session schema",
        ))
    }
}

fn normalize_schema_statement(statement: &str) -> String {
    statement.split_whitespace().collect()
}

fn table_matches(
    connection: &Connection,
    table: &str,
    expected_columns: &[ExpectedColumnSignature],
) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info('{table}')"))?;
    let columns = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(columns.len() == expected_columns.len()
        && columns.iter().zip(expected_columns).all(
            |(
                (column_id, name, declared_type, not_null, default_value, primary_key_position),
                expected,
            )| {
                *column_id == expected.column_id
                    && name == expected.name
                    && declared_type == expected.declared_type
                    && *not_null == expected.not_null
                    && default_value.as_deref() == expected.default_value
                    && *primary_key_position == expected.primary_key_position
            },
        ))
}

fn legacy_turn_events_foreign_key_matches(connection: &Connection) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare("PRAGMA foreign_key_list('legacy_turn_events')")?;
    let keys = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(matches!(
        keys.as_slice(),
        [(0, 0, table, from, to, on_update, on_delete, matching)]
            if table == "legacy_turns"
                && from == "turn_id"
                && to == "id"
                && on_update == "NO ACTION"
                && on_delete == "CASCADE"
                && matching == "NONE"
    ))
}

fn indexes_match(
    connection: &Connection,
    table: &str,
    expected_indexes: &[ExpectedIndexSignature],
) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA index_list('{table}')"))?;
    let indexes = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if indexes.len() != expected_indexes.len()
        || !indexes.iter().zip(expected_indexes).all(
            |((sequence, name, unique, origin, partial), expected)| {
                *sequence == expected.sequence
                    && name == expected.name
                    && *unique == expected.unique
                    && origin == expected.origin
                    && *partial == expected.partial
            },
        )
    {
        return Ok(false);
    }

    for index in expected_indexes {
        let mut statement = connection.prepare(&format!("PRAGMA index_info('{}')", index.name))?;
        let columns = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        if columns.len() != LEGACY_TURN_EVENTS_INDEX_COLUMNS.len()
            || !columns.iter().zip(LEGACY_TURN_EVENTS_INDEX_COLUMNS).all(
                |((sequence, column_id, name), expected)| {
                    *sequence == expected.sequence
                        && *column_id == expected.column_id
                        && name == expected.name
                },
            )
        {
            return Ok(false);
        }
    }

    Ok(true)
}

fn legacy_turn_events_indexes_match(connection: &Connection) -> rusqlite::Result<bool> {
    indexes_match(
        connection,
        "legacy_turn_events",
        &LEGACY_TURN_EVENTS_INDEXES,
    )
}

fn insert_legacy_turn_event(
    transaction: &Transaction<'_>,
    turn_id: i64,
    sequence: i64,
    event: &TurnEvent,
) -> rusqlite::Result<()> {
    let fields = encode_turn_event(event);
    transaction.execute(
        "INSERT INTO legacy_turn_events
         (turn_id, sequence, kind, state, part_kind, call_id, name, input, content, is_error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            turn_id,
            sequence,
            fields.kind,
            fields.state,
            fields.part_kind,
            fields.call_id,
            fields.name,
            fields.input,
            fields.content,
            fields.is_error,
        ],
    )?;
    Ok(())
}

struct PersistedTurnEvent {
    kind: String,
    state: Option<String>,
    part_kind: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    input: Option<String>,
    content: Option<String>,
    is_error: Option<i64>,
}

struct EncodedTurnEvent<'a> {
    kind: &'static str,
    state: Option<&'static str>,
    part_kind: Option<&'static str>,
    call_id: Option<&'a str>,
    name: Option<&'a str>,
    input: Option<&'a str>,
    content: Option<&'a str>,
    is_error: Option<i64>,
}

#[derive(Clone, Copy)]
struct PersistedEventFieldMatrix {
    state: bool,
    part_kind: bool,
    call_id: bool,
    name: bool,
    input: bool,
    content: bool,
    is_error: bool,
}

const STATE_CHANGED_FIELDS: PersistedEventFieldMatrix = PersistedEventFieldMatrix {
    state: true,
    part_kind: false,
    call_id: false,
    name: false,
    input: false,
    content: false,
    is_error: false,
};
const PROVIDER_TEXT_FIELDS: PersistedEventFieldMatrix = PersistedEventFieldMatrix {
    state: false,
    part_kind: true,
    call_id: false,
    name: false,
    input: false,
    content: true,
    is_error: false,
};
const PROVIDER_TOOL_CALL_FIELDS: PersistedEventFieldMatrix = PersistedEventFieldMatrix {
    state: false,
    part_kind: true,
    call_id: true,
    name: true,
    input: true,
    content: false,
    is_error: false,
};
const TOOL_CALL_REQUESTED_FIELDS: PersistedEventFieldMatrix = PersistedEventFieldMatrix {
    state: false,
    part_kind: false,
    call_id: true,
    name: true,
    input: true,
    content: false,
    is_error: false,
};
const TOOL_RESULT_FIELDS: PersistedEventFieldMatrix = PersistedEventFieldMatrix {
    state: false,
    part_kind: false,
    call_id: true,
    name: false,
    input: false,
    content: true,
    is_error: true,
};

fn encode_turn_event(event: &TurnEvent) -> EncodedTurnEvent<'_> {
    match event {
        TurnEvent::StateChanged(state) => EncodedTurnEvent {
            kind: "state_changed",
            state: Some(encode_turn_state(*state)),
            part_kind: None,
            call_id: None,
            name: None,
            input: None,
            content: None,
            is_error: None,
        },
        TurnEvent::ProviderPart(MessagePart::Text(text)) => EncodedTurnEvent {
            kind: "provider_part",
            state: None,
            part_kind: Some("text"),
            call_id: None,
            name: None,
            input: None,
            content: Some(text),
            is_error: None,
        },
        TurnEvent::ProviderPart(MessagePart::Reasoning(text)) => EncodedTurnEvent {
            kind: "provider_part",
            state: None,
            part_kind: Some("reasoning"),
            call_id: None,
            name: None,
            input: None,
            content: Some(text),
            is_error: None,
        },
        TurnEvent::ProviderPart(MessagePart::ToolCall { id, name, input }) => EncodedTurnEvent {
            kind: "provider_part",
            state: None,
            part_kind: Some("tool_call"),
            call_id: Some(id),
            name: Some(name),
            input: Some(input),
            content: None,
            is_error: None,
        },
        TurnEvent::ProviderPart(MessagePart::ToolResult { .. }) => {
            unreachable!("completed snapshots reject provider tool results")
        }
        TurnEvent::ToolCallRequested { id, name, input } => EncodedTurnEvent {
            kind: "tool_call_requested",
            state: None,
            part_kind: None,
            call_id: Some(id),
            name: Some(name),
            input: Some(input),
            content: None,
            is_error: None,
        },
        TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id,
            content,
            is_error,
        }) => EncodedTurnEvent {
            kind: "tool_result",
            state: None,
            part_kind: None,
            call_id: Some(tool_call_id),
            name: None,
            input: None,
            content: Some(content),
            is_error: Some(i64::from(*is_error)),
        },
        TurnEvent::ToolResult(_) => {
            unreachable!("completed snapshots reject non-result tool events")
        }
        TurnEvent::Usage(_) => {
            unreachable!("presentation usage events are excluded from completed history")
        }
        TurnEvent::ToolResultFacts { .. } => {
            unreachable!("live-only tool result facts are excluded from completed history")
        }
    }
}

fn persistable_turn_event(event: &TurnEvent) -> bool {
    !matches!(
        event,
        TurnEvent::Usage(_) | TurnEvent::ToolResultFacts { .. }
    )
}

fn decode_turn_event(fields: PersistedTurnEvent) -> Result<TurnEvent, &'static str> {
    match fields.kind.as_str() {
        "state_changed" => {
            validate_field_matrix(&fields, STATE_CHANGED_FIELDS)?;
            Ok(TurnEvent::StateChanged(decode_turn_state(
                fields.state.as_deref(),
            )?))
        }
        "provider_part" => match fields.part_kind.as_deref() {
            Some("text") => {
                let fields = required_fields(fields, PROVIDER_TEXT_FIELDS)?;
                Ok(TurnEvent::ProviderPart(MessagePart::Text(
                    fields.content.unwrap(),
                )))
            }
            Some("reasoning") => {
                let fields = required_fields(fields, PROVIDER_TEXT_FIELDS)?;
                Ok(TurnEvent::ProviderPart(MessagePart::Reasoning(
                    fields.content.unwrap(),
                )))
            }
            Some("tool_call") => {
                let fields = required_fields(fields, PROVIDER_TOOL_CALL_FIELDS)?;
                Ok(TurnEvent::ProviderPart(MessagePart::ToolCall {
                    id: fields.call_id.unwrap(),
                    name: fields.name.unwrap(),
                    input: fields.input.unwrap(),
                }))
            }
            _ => Err("invalid provider part"),
        },
        "tool_call_requested" => {
            let fields = required_fields(fields, TOOL_CALL_REQUESTED_FIELDS)?;
            Ok(TurnEvent::ToolCallRequested {
                id: fields.call_id.unwrap(),
                name: fields.name.unwrap(),
                input: fields.input.unwrap(),
            })
        }
        "tool_result" => {
            let fields = required_fields(fields, TOOL_RESULT_FIELDS)?;
            Ok(TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: fields.call_id.unwrap(),
                content: fields.content.unwrap(),
                is_error: match fields.is_error {
                    Some(0) => false,
                    Some(1) => true,
                    _ => return Err("invalid tool result error flag"),
                },
            }))
        }
        _ => Err("invalid persisted event kind"),
    }
}

fn required_fields(
    fields: PersistedTurnEvent,
    matrix: PersistedEventFieldMatrix,
) -> Result<PersistedTurnEvent, &'static str> {
    validate_field_matrix(&fields, matrix)?;
    Ok(fields)
}

fn validate_field_matrix(
    fields: &PersistedTurnEvent,
    matrix: PersistedEventFieldMatrix,
) -> Result<(), &'static str> {
    (fields.state.is_some() == matrix.state
        && fields.part_kind.is_some() == matrix.part_kind
        && fields.call_id.is_some() == matrix.call_id
        && fields.name.is_some() == matrix.name
        && fields.input.is_some() == matrix.input
        && fields.content.is_some() == matrix.content
        && fields.is_error.is_some() == matrix.is_error)
        .then_some(())
        .ok_or("invalid persisted event fields")
}

fn encode_turn_state(state: TurnState) -> &'static str {
    match state {
        TurnState::Idle => "idle",
        TurnState::Requesting => "requesting",
        TurnState::Streaming => "streaming",
        TurnState::Dispatching => "dispatching",
        TurnState::Completed => "completed",
        TurnState::Cancelled => "cancelled",
        TurnState::Failed => "failed",
    }
}

fn decode_turn_state(value: Option<&str>) -> Result<TurnState, &'static str> {
    match value {
        Some("idle") => Ok(TurnState::Idle),
        Some("requesting") => Ok(TurnState::Requesting),
        Some("streaming") => Ok(TurnState::Streaming),
        Some("dispatching") => Ok(TurnState::Dispatching),
        Some("completed") => Ok(TurnState::Completed),
        Some("cancelled") => Ok(TurnState::Cancelled),
        Some("failed") => Ok(TurnState::Failed),
        _ => Err("invalid turn state"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agens_core::Usage;

    #[test]
    fn ignores_usage_events_when_converting_completed_history() {
        let events = [
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("before usage".into())),
            TurnEvent::Usage(Usage {
                input_tokens: Some(5),
                output_tokens: Some(3),
                total_tokens: Some(8),
                context_window: Some(16),
            }),
            TurnEvent::ProviderPart(MessagePart::Text("after usage".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ];

        let persisted_events = events
            .iter()
            .filter(|event| persistable_turn_event(event))
            .cloned()
            .collect::<Vec<_>>();

        assert_eq!(
            persisted_events,
            vec![
                TurnEvent::StateChanged(TurnState::Requesting),
                TurnEvent::StateChanged(TurnState::Streaming),
                TurnEvent::ProviderPart(MessagePart::Text("before usage".into())),
                TurnEvent::ProviderPart(MessagePart::Text("after usage".into())),
                TurnEvent::StateChanged(TurnState::Completed),
            ]
        );
    }

    #[test]
    fn ignores_tool_result_facts_events_when_converting_completed_history() {
        let mut facts_source = agens_core::TurnCoordinator::new();
        facts_source.begin().unwrap();
        facts_source
            .accept_provider_part(MessagePart::ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                input: "{\"command\":\"exit 1\"}".into(),
            })
            .unwrap();
        facts_source.finish_provider_iteration().unwrap();
        facts_source
            .accept_tool_result(
                "call-1",
                "exit 1".into(),
                true,
                Some(agens_core::ToolResultFacts::Bash { exit_code: Some(1) }),
            )
            .unwrap();
        let facts_event = facts_source
            .events()
            .iter()
            .find(|event| matches!(event, TurnEvent::ToolResultFacts { .. }))
            .cloned()
            .expect("facts event must be present in the source coordinator");

        let events = [
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("before facts".into())),
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call-1".into(),
                content: "exit 1".into(),
                is_error: true,
            }),
            facts_event,
            TurnEvent::ProviderPart(MessagePart::Text("after facts".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ];

        let persisted_events = events
            .iter()
            .filter(|event| persistable_turn_event(event))
            .cloned()
            .collect::<Vec<_>>();

        assert_eq!(
            persisted_events,
            vec![
                TurnEvent::StateChanged(TurnState::Requesting),
                TurnEvent::StateChanged(TurnState::Streaming),
                TurnEvent::ProviderPart(MessagePart::Text("before facts".into())),
                TurnEvent::ToolResult(MessagePart::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: "exit 1".into(),
                    is_error: true,
                }),
                TurnEvent::ProviderPart(MessagePart::Text("after facts".into())),
                TurnEvent::StateChanged(TurnState::Completed),
            ]
        );
    }

    #[test]
    fn preserves_completed_event_order_after_ignoring_multiple_usage_events() {
        let events = [
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::Usage(Usage::default()),
            TurnEvent::StateChanged(TurnState::Streaming),
            TurnEvent::ProviderPart(MessagePart::Text("first".into())),
            TurnEvent::Usage(Usage {
                input_tokens: None,
                output_tokens: Some(0),
                total_tokens: None,
                context_window: None,
            }),
            TurnEvent::ProviderPart(MessagePart::Text("second".into())),
            TurnEvent::StateChanged(TurnState::Completed),
        ];

        let persisted_events = events
            .iter()
            .filter(|event| persistable_turn_event(event))
            .cloned()
            .collect::<Vec<_>>();

        assert_eq!(
            persisted_events,
            vec![
                TurnEvent::StateChanged(TurnState::Requesting),
                TurnEvent::StateChanged(TurnState::Streaming),
                TurnEvent::ProviderPart(MessagePart::Text("first".into())),
                TurnEvent::ProviderPart(MessagePart::Text("second".into())),
                TurnEvent::StateChanged(TurnState::Completed),
            ]
        );
    }
}

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use agens_core::{
    CompletedTurnRepository, CompletedTurnSnapshot, CompletedTurnStoreError, MessagePart,
    PermissionDecision, PermissionPattern, ProjectPermissionGrant, TurnEvent, TurnState,
};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

const PERMISSIONS_DATABASE: &str = "rust-permissions.db";
const PERMISSIONS_SCHEMA_VERSION: i64 = 1;
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
        let data_directory = data_directory.as_ref();
        fs::create_dir_all(data_directory).map_err(|error| {
            PermissionGrantStoreError::operation("create data directory", data_directory, error)
        })?;
        restrict_permissions(data_directory, 0o700)?;

        let database_path = data_directory.join(PERMISSIONS_DATABASE);
        let connection = Connection::open(&database_path).map_err(|error| {
            PermissionGrantStoreError::operation("open database", &database_path, error)
        })?;
        restrict_permissions(&database_path, 0o600)?;
        initialize_schema(&connection, &database_path)?;

        Ok(Self {
            database_path,
            connection,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.database_path.clone()
    }

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
            Ok(ProjectPermissionGrant::new(
                project,
                decode_decision(&decision)?,
                decode_pattern(&tool_kind, tool_value)?,
                decode_pattern(&target_kind, target_value)?,
            ))
        })
        .collect()
    }
}

fn initialize_schema(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), PermissionGrantStoreError> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|error| {
            PermissionGrantStoreError::operation("read schema version", database_path, error)
        })?;

    match version {
        0 => {
            connection
                .execute_batch(&format!(
                    "
                BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS permission_grants (
                    id INTEGER PRIMARY KEY,
                    project TEXT NOT NULL,
                    decision TEXT NOT NULL,
                    tool_kind TEXT NOT NULL,
                    tool_value TEXT,
                    target_kind TEXT NOT NULL,
                    target_value TEXT
                );
                CREATE INDEX IF NOT EXISTS permission_grants_project
                    ON permission_grants(project, id);
                PRAGMA user_version = {PERMISSIONS_SCHEMA_VERSION};
                COMMIT;
                "
                ))
                .map_err(|error| {
                    PermissionGrantStoreError::operation("initialize schema", database_path, error)
                })?;

            verify_schema(connection, database_path)
        }
        PERMISSIONS_SCHEMA_VERSION => verify_schema(connection, database_path),
        unsupported => Err(PermissionGrantStoreError::operation(
            "check schema version",
            database_path,
            format!("unsupported schema version {unsupported}"),
        )),
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

#[cfg(unix)]
fn restrict_permissions(path: &Path, maximum_mode: u32) -> Result<(), PermissionGrantStoreError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::metadata(path).map_err(|error| {
        PermissionGrantStoreError::operation("inspect permissions", path, error)
    })?;
    let current_mode = metadata.mode() & 0o777;
    let restricted_mode = current_mode & maximum_mode;

    if restricted_mode != current_mode {
        fs::set_permissions(path, fs::Permissions::from_mode(restricted_mode)).map_err(
            |error| PermissionGrantStoreError::operation("restrict permissions", path, error),
        )?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_: &Path, _: u32) -> Result<(), PermissionGrantStoreError> {
    Ok(())
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
    }
}

fn decode_pattern(
    kind: &str,
    value: Option<String>,
) -> Result<PermissionPattern, PermissionGrantStoreError> {
    match (kind, value) {
        ("any", None) => Ok(PermissionPattern::Any),
        ("exact", Some(value)) if !value.is_empty() => Ok(PermissionPattern::Exact(value)),
        _ => Err(PermissionGrantStoreError::invalid(
            "invalid stored grant pattern",
        )),
    }
}

const SESSIONS_DATABASE: &str = "rust-sessions.db";
const SESSIONS_SCHEMA_VERSION: i64 = 1;
const COMPLETED_TURNS_COLUMNS: [ExpectedColumnSignature; 1] = [ExpectedColumnSignature::new(
    0, "id", "INTEGER", false, None, 1,
)];
const COMPLETED_TURN_EVENTS_COLUMNS: [ExpectedColumnSignature; 10] = [
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
        let data_directory = data_directory.as_ref();
        fs::create_dir_all(data_directory).map_err(|error| {
            SessionStoreError::operation("create data directory", data_directory, error)
        })?;
        restrict_session_permissions(data_directory, 0o700)?;

        let database_path = data_directory.join(SESSIONS_DATABASE);
        let connection = Connection::open(&database_path).map_err(|error| {
            SessionStoreError::operation("open database", &database_path, error)
        })?;
        restrict_session_permissions(&database_path, 0o600)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| {
                SessionStoreError::operation("configure busy timeout", &database_path, error)
            })?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| {
                SessionStoreError::operation("enable foreign keys", &database_path, error)
            })?;
        initialize_sessions_schema(&connection, &database_path)?;

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
            .prepare("SELECT id FROM completed_turns ORDER BY id")
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
                self.load_completed_turn_for_resume(id)
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
                "SELECT EXISTS(SELECT 1 FROM completed_turns WHERE id = ?1)",
                [id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| {
                SessionStoreError::operation("check completed turn", &self.database_path, error)
            })?;
        if !exists {
            return Err(SessionStoreError::operation(
                "load completed turn",
                &self.database_path,
                format!("unknown completed turn {id}"),
            ));
        }

        let mut statement = self
            .connection
            .prepare(
                "SELECT kind, state, part_kind, call_id, name, input, content, is_error
             FROM completed_turn_events WHERE turn_id = ?1 ORDER BY sequence",
            )
            .map_err(|error| {
                SessionStoreError::operation(
                    "prepare completed turn events",
                    &self.database_path,
                    error,
                )
            })?;
        let rows = statement
            .query_map([id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                ))
            })
            .map_err(|error| {
                SessionStoreError::operation(
                    "query completed turn events",
                    &self.database_path,
                    error,
                )
            })?;
        let events = rows
            .map(|row| {
                let fields = row.map_err(|error| {
                    SessionStoreError::operation(
                        "read completed turn events",
                        &self.database_path,
                        error,
                    )
                })?;
                decode_turn_event(fields).map_err(|error| {
                    SessionStoreError::operation(
                        "decode completed turn events",
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
            .execute("INSERT INTO completed_turns DEFAULT VALUES", [])
            .map_err(|error| {
                SessionStoreError::operation("create completed turn", &self.database_path, error)
            })?;
        let turn_id = transaction.last_insert_rowid();

        for (sequence, event) in snapshot.events().iter().enumerate() {
            insert_turn_event(&transaction, turn_id, sequence as i64, event).map_err(|error| {
                SessionStoreError::operation(
                    "write completed turn event",
                    &self.database_path,
                    error,
                )
            })?;
        }

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

fn initialize_sessions_schema(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|error| {
            SessionStoreError::operation("read schema version", database_path, error)
        })?;

    match version {
        0 => connection
            .execute_batch(&format!(
                "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS completed_turns (id INTEGER PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS completed_turn_events (
                 turn_id INTEGER NOT NULL,
                 sequence INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 state TEXT,
                 part_kind TEXT,
                 call_id TEXT,
                 name TEXT,
                 input TEXT,
                 content TEXT,
                 is_error INTEGER,
                 PRIMARY KEY (turn_id, sequence),
                 FOREIGN KEY (turn_id) REFERENCES completed_turns(id)
             );
             PRAGMA user_version = {SESSIONS_SCHEMA_VERSION};
             COMMIT;"
            ))
            .map_err(|error| {
                SessionStoreError::operation("initialize schema", database_path, error)
            })?,
        SESSIONS_SCHEMA_VERSION => {}
        unsupported => {
            return Err(SessionStoreError::operation(
                "check schema version",
                database_path,
                format!("unsupported schema version {unsupported}"),
            ));
        }
    }

    let completed_turns_matches =
        table_matches(connection, "completed_turns", &COMPLETED_TURNS_COLUMNS)
            .map_err(|error| SessionStoreError::operation("verify schema", database_path, error))?;
    let completed_turn_events_matches = table_matches(
        connection,
        "completed_turn_events",
        &COMPLETED_TURN_EVENTS_COLUMNS,
    )
    .map_err(|error| SessionStoreError::operation("verify schema", database_path, error))?;
    let foreign_key_matches = completed_turn_events_foreign_key_matches(connection)
        .map_err(|error| SessionStoreError::operation("verify schema", database_path, error))?;

    if !(completed_turns_matches && completed_turn_events_matches && foreign_key_matches) {
        return Err(SessionStoreError::operation(
            "verify schema",
            database_path,
            "incompatible sessions schema",
        ));
    }

    Ok(())
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

fn completed_turn_events_foreign_key_matches(connection: &Connection) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare("PRAGMA foreign_key_list('completed_turn_events')")?;
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
            if table == "completed_turns"
                && from == "turn_id"
                && to == "id"
                && on_update == "NO ACTION"
                && on_delete == "NO ACTION"
                && matching == "NONE"
    ))
}

fn insert_turn_event(
    transaction: &Transaction<'_>,
    turn_id: i64,
    sequence: i64,
    event: &TurnEvent,
) -> rusqlite::Result<()> {
    let (kind, state, part_kind, call_id, name, input, content, is_error) =
        encode_turn_event(event);
    transaction.execute(
        "INSERT INTO completed_turn_events
         (turn_id, sequence, kind, state, part_kind, call_id, name, input, content, is_error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            turn_id, sequence, kind, state, part_kind, call_id, name, input, content, is_error
        ],
    )?;
    Ok(())
}

type PersistedTurnEvent = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

fn encode_turn_event(
    event: &TurnEvent,
) -> (
    &'static str,
    Option<&str>,
    Option<&'static str>,
    Option<&str>,
    Option<&str>,
    Option<&str>,
    Option<&str>,
    Option<i64>,
) {
    match event {
        TurnEvent::StateChanged(state) => (
            "state_changed",
            Some(encode_turn_state(*state)),
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        TurnEvent::ProviderPart(MessagePart::Text(text)) => (
            "provider_part",
            None,
            Some("text"),
            None,
            None,
            None,
            Some(text),
            None,
        ),
        TurnEvent::ProviderPart(MessagePart::Reasoning(text)) => (
            "provider_part",
            None,
            Some("reasoning"),
            None,
            None,
            None,
            Some(text),
            None,
        ),
        TurnEvent::ProviderPart(MessagePart::ToolCall { id, name, input }) => (
            "provider_part",
            None,
            Some("tool_call"),
            Some(id),
            Some(name),
            Some(input),
            None,
            None,
        ),
        TurnEvent::ProviderPart(MessagePart::ToolResult { .. }) => {
            unreachable!("completed snapshots reject provider tool results")
        }
        TurnEvent::ToolCallRequested { id, name, input } => (
            "tool_call_requested",
            None,
            None,
            Some(id),
            Some(name),
            Some(input),
            None,
            None,
        ),
        TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id,
            content,
            is_error,
        }) => (
            "tool_result",
            None,
            None,
            Some(tool_call_id),
            None,
            None,
            Some(content),
            Some(i64::from(*is_error)),
        ),
        TurnEvent::ToolResult(_) => {
            unreachable!("completed snapshots reject non-result tool events")
        }
    }
}

fn decode_turn_event(fields: PersistedTurnEvent) -> Result<TurnEvent, &'static str> {
    let (kind, state, part_kind, call_id, name, input, content, is_error) = fields;
    match kind.as_str() {
        "state_changed" => Ok(TurnEvent::StateChanged(decode_turn_state(
            state.as_deref(),
        )?)),
        "provider_part" => match part_kind.as_deref() {
            Some("text") => Ok(TurnEvent::ProviderPart(MessagePart::Text(
                content.ok_or("missing text content")?,
            ))),
            Some("reasoning") => Ok(TurnEvent::ProviderPart(MessagePart::Reasoning(
                content.ok_or("missing reasoning content")?,
            ))),
            Some("tool_call") => Ok(TurnEvent::ProviderPart(MessagePart::ToolCall {
                id: call_id.ok_or("missing tool call id")?,
                name: name.ok_or("missing tool call name")?,
                input: input.ok_or("missing tool call input")?,
            })),
            _ => Err("invalid provider part"),
        },
        "tool_call_requested" => Ok(TurnEvent::ToolCallRequested {
            id: call_id.ok_or("missing requested tool id")?,
            name: name.ok_or("missing requested tool name")?,
            input: input.ok_or("missing requested tool input")?,
        }),
        "tool_result" => Ok(TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: call_id.ok_or("missing tool result id")?,
            content: content.ok_or("missing tool result content")?,
            is_error: match is_error {
                Some(0) => false,
                Some(1) => true,
                _ => return Err("invalid tool result error flag"),
            },
        })),
        _ => Err("invalid persisted event kind"),
    }
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

#[cfg(unix)]
fn restrict_session_permissions(path: &Path, maximum_mode: u32) -> Result<(), SessionStoreError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::metadata(path)
        .map_err(|error| SessionStoreError::operation("inspect permissions", path, error))?;
    let current_mode = metadata.mode() & 0o777;
    let restricted_mode = current_mode & maximum_mode;

    if restricted_mode != current_mode {
        fs::set_permissions(path, fs::Permissions::from_mode(restricted_mode))
            .map_err(|error| SessionStoreError::operation("restrict permissions", path, error))?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn restrict_session_permissions(_: &Path, _: u32) -> Result<(), SessionStoreError> {
    Ok(())
}

use std::{
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
};

use agens_core::{
    CompletedTurnRepository, CompletedTurnSnapshot, CompletedTurnStoreError, MessagePart,
    PermissionDecision, PermissionPattern, ProjectPermissionGrant, TurnEvent, TurnState,
};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};

mod session_writer;
pub use session_writer::StoredSession;

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
}

impl SessionStore {
    pub fn create_verified_v1_backup(&self) -> Result<PathBuf, SessionStoreError> {
        let source =
            Connection::open_with_flags(&self.database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|error| {
                    SessionStoreError::operation("open backup source", &self.database_path, error)
                })?;
        source
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| {
                SessionStoreError::operation("configure backup source", &self.database_path, error)
            })?;
        source.execute_batch("BEGIN").map_err(|error| {
            SessionStoreError::operation("snapshot backup source", &self.database_path, error)
        })?;
        validate_v1_schema(&source, &self.database_path)?;
        let source_manifest = v1_manifest(&source).map_err(|error| {
            SessionStoreError::operation("read backup source", &self.database_path, error)
        })?;

        for suffix in 0_u64.. {
            let extension = if suffix == 0 {
                "v1.bak".to_owned()
            } else {
                format!("v1.bak.{suffix}")
            };
            let backup_path =
                PathBuf::from(format!("{}.{}", self.database_path.display(), extension));
            let temporary_path = PathBuf::from(format!("{}.tmp", backup_path.display()));
            let manifest_path = PathBuf::from(format!("{}.manifest", backup_path.display()));
            let manifest_temporary_path = PathBuf::from(format!("{}.tmp", manifest_path.display()));

            if backup_path.exists() || manifest_path.exists() {
                continue;
            }
            let temporary = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(SessionStoreError::operation(
                        "create backup temporary",
                        &temporary_path,
                        error,
                    ));
                }
            };
            drop(temporary);

            let mut destination = Connection::open(&temporary_path).map_err(|error| {
                SessionStoreError::operation("open backup destination", &temporary_path, error)
            })?;
            {
                let backup = Backup::new(&source, &mut destination).map_err(|error| {
                    SessionStoreError::operation("start backup", &temporary_path, error)
                })?;
                backup
                    .run_to_completion(100, std::time::Duration::from_millis(10), None)
                    .map_err(|error| {
                        SessionStoreError::operation("copy backup", &temporary_path, error)
                    })?;
            }
            migration_fault("after-backup-step", &self.database_path)?;
            drop(destination);
            fs::File::open(&temporary_path)
                .and_then(|file| file.sync_all())
                .map_err(|error| {
                    SessionStoreError::operation("fsync backup", &temporary_path, error)
                })?;

            if let Err(error) = fs::hard_link(&temporary_path, &backup_path) {
                let _ = fs::remove_file(&temporary_path);
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(SessionStoreError::operation(
                    "install backup",
                    &backup_path,
                    error,
                ));
            }
            migration_fault("after-backup-install", &self.database_path)?;
            let verification =
                Connection::open_with_flags(&backup_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|error| {
                        SessionStoreError::operation("reopen backup", &backup_path, error)
                    })?;
            let backup_manifest = verify_v1_backup(&source_manifest, &verification, &backup_path)?;
            drop(verification);

            let mut manifest = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&manifest_temporary_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&backup_path);
                    let _ = fs::remove_file(&temporary_path);
                    continue;
                }
                Err(error) => {
                    return Err(SessionStoreError::operation(
                        "create manifest temporary",
                        &manifest_temporary_path,
                        error,
                    ));
                }
            };
            writeln!(manifest, "version=1\nquick_check=ok\n{backup_manifest}")
                .and_then(|_| manifest.sync_all())
                .map_err(|error| {
                    SessionStoreError::operation(
                        "write backup manifest",
                        &manifest_temporary_path,
                        error,
                    )
                })?;
            drop(manifest);

            if let Err(error) = fs::hard_link(&manifest_temporary_path, &manifest_path) {
                let _ = fs::remove_file(&backup_path);
                let _ = fs::remove_file(&temporary_path);
                let _ = fs::remove_file(&manifest_temporary_path);
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(SessionStoreError::operation(
                    "install backup manifest",
                    &manifest_path,
                    error,
                ));
            }
            fs::remove_file(&temporary_path)
                .and_then(|_| fs::remove_file(&manifest_temporary_path))
                .map_err(|error| {
                    SessionStoreError::operation("finalize backup", &backup_path, error)
                })?;
            migration_fault("after-backup-finalize", &self.database_path)?;
            fs::File::open(
                self.database_path
                    .parent()
                    .unwrap_or_else(|| Path::new(".")),
            )
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                SessionStoreError::operation("fsync backup parent", &backup_path, error)
            })?;
            migration_fault("after-backup-parent-fsync", &self.database_path)?;

            return Ok(backup_path);
        }

        unreachable!("backup suffixes are unbounded")
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

const SESSIONS_DATABASE: &str = "rust-sessions.db";
const SESSIONS_SCHEMA_VERSION: i64 = 5;
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
const COMPLETED_TURN_EVENTS_INDEXES: [ExpectedIndexSignature; 2] = [
    ExpectedIndexSignature::new(0, "completed_turn_events_turn_sequence", true, "c", false),
    ExpectedIndexSignature::new(
        1,
        "sqlite_autoindex_completed_turn_events_1",
        true,
        "pk",
        false,
    ),
];
const COMPLETED_TURN_EVENTS_INDEX_COLUMNS: [ExpectedIndexColumnSignature; 2] = [
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
        let mut connection = Connection::open(&database_path).map_err(|error| {
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

        match session_schema_version(&connection, &database_path)? {
            0 => {
                initialize_v5_schema(&mut connection, &database_path)?;
                connection
                    .pragma_update(None, "journal_mode", "WAL")
                    .map_err(|error| {
                        SessionStoreError::operation("enable WAL", &database_path, error)
                    })?;
            }
            1 => {
                validate_v1_schema(&connection, &database_path)?;
                let mut store = Self {
                    database_path: database_path.clone(),
                    connection,
                };
                store.create_verified_v1_backup()?;
                migrate_v1_on_open(&mut store.connection, &store.database_path)?;
                migrate_v4_on_open(&mut store.connection, &store.database_path)?;
                return Ok(store);
            }
            2 => {
                migrate_v2_on_open(&mut connection, &database_path)?;
                migrate_v4_on_open(&mut connection, &database_path)?;
            }
            3 => {
                migrate_v3_on_open(&mut connection, &database_path)?;
                migrate_v4_on_open(&mut connection, &database_path)?;
            }
            4 => migrate_v4_on_open(&mut connection, &database_path)?,
            5 => validate_v5_schema(&connection, &database_path)?,
            unsupported => {
                return Err(SessionStoreError::operation(
                    "check schema version",
                    &database_path,
                    format!("unsupported schema version {unsupported}"),
                ));
            }
        }

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

fn initialize_v5_schema(
    connection: &mut Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| {
            SessionStoreError::operation("start v2 initialization", database_path, error)
        })?;
    create_legacy_archive_schema(&transaction, database_path)?;
    transaction
        .execute_batch(&format!(
            "{} PRAGMA user_version = {SESSIONS_SCHEMA_VERSION};",
            normalized_session_schema_v5()
        ))
        .map_err(|error| {
            SessionStoreError::operation("initialize v2 schema", database_path, error)
        })?;
    validate_v5_schema(&transaction, database_path)?;
    transaction.commit().map_err(|error| {
        SessionStoreError::operation("commit v2 initialization", database_path, error)
    })
}

fn migrate_v4_on_open(
    connection: &mut Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| {
            SessionStoreError::operation("start v4 migration", database_path, error)
        })?;
    validate_v4_schema(&transaction, database_path)?;
    transaction
        .execute_batch(
            "CREATE TABLE session_attempts (
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
             CREATE INDEX session_attempts_latest ON session_attempts(session_id, sequence DESC, id DESC);
             PRAGMA user_version = 5;",
        )
        .map_err(|error| SessionStoreError::operation("migrate v4 schema", database_path, error))?;
    migration_fault("before-v5-commit", database_path)?;
    validate_v5_schema(&transaction, database_path)?;
    transaction
        .commit()
        .map_err(|error| SessionStoreError::operation("commit v4 migration", database_path, error))
}

fn migrate_v2_on_open(
    connection: &mut Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| {
            SessionStoreError::operation("start v2 migration", database_path, error)
        })?;
    validate_v2_schema(&transaction, database_path)?;
    transaction
        .execute_batch(
            "ALTER TABLE sessions ADD COLUMN provider_id TEXT
             CHECK(provider_id <> '' AND length(provider_id) <= 64);
         ALTER TABLE sessions ADD COLUMN model_id TEXT
             CHECK(model_id <> '' AND length(model_id) <= 64);
         ALTER TABLE sessions ADD COLUMN reasoning_effort TEXT
             CHECK(reasoning_effort IN('none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'));
         PRAGMA user_version = 4;",
        )
        .map_err(|error| SessionStoreError::operation("migrate v2 schema", database_path, error))?;
    migration_fault("before-v3-commit", database_path)?;
    validate_v4_schema(&transaction, database_path)?;
    transaction
        .commit()
        .map_err(|error| SessionStoreError::operation("commit v2 migration", database_path, error))
}

fn migrate_v3_on_open(
    connection: &mut Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    validate_v3_schema(connection, database_path)?;
    connection
        .pragma_update(None, "foreign_keys", "OFF")
        .map_err(|error| {
            SessionStoreError::operation(
                "disable foreign keys for v3 migration",
                database_path,
                error,
            )
        })?;

    let migration = (|| {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                SessionStoreError::operation("start v3 migration", database_path, error)
            })?;
        transaction
            .execute_batch(
                "CREATE TABLE sessions_v3_backup AS SELECT * FROM sessions;
                 DROP TABLE sessions;
                 CREATE TABLE sessions (
                     id INTEGER PRIMARY KEY,
                     project TEXT NOT NULL CHECK(project <> ''),
                     title TEXT NOT NULL,
                     active_agent TEXT NOT NULL CHECK(active_agent <> ''),
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL,
                     completed_turn_count INTEGER NOT NULL DEFAULT 0 CHECK(completed_turn_count >= 0),
                     resumable INTEGER NOT NULL DEFAULT 0 CHECK(resumable IN(0, 1)),
                     provider_id TEXT CHECK(provider_id <> '' AND length(provider_id) <= 64),
                     model_id TEXT CHECK(model_id <> '' AND length(model_id) <= 64),
                     reasoning_effort TEXT CHECK(reasoning_effort IN('none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max')),
                     CHECK(resumable = (completed_turn_count > 0))
                 );
                 INSERT INTO sessions SELECT * FROM sessions_v3_backup;
                 DROP TABLE sessions_v3_backup;
                 CREATE INDEX sessions_list ON sessions(resumable, updated_at DESC, id DESC);
                 PRAGMA user_version = 4;",
            )
            .map_err(|error| {
                SessionStoreError::operation("migrate v3 schema", database_path, error)
            })?;
        migration_fault("before-v4-commit", database_path)?;
        validate_v4_schema(&transaction, database_path)?;
        transaction.commit().map_err(|error| {
            SessionStoreError::operation("commit v3 migration", database_path, error)
        })
    })();

    let foreign_keys = connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|error| {
            SessionStoreError::operation(
                "restore foreign keys after v3 migration",
                database_path,
                error,
            )
        });
    migration?;
    foreign_keys
}

fn validate_v1_schema(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|error| {
            SessionStoreError::operation("read schema version", database_path, error)
        })?;

    if version != 1 {
        return Err(SessionStoreError::operation(
            "check schema version",
            database_path,
            format!("unsupported schema version {version}"),
        ));
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
    let indexes_match = completed_turn_events_indexes_match(connection)
        .map_err(|error| SessionStoreError::operation("verify schema", database_path, error))?;
    let completed_turns_indexes_match = completed_turns_indexes_match(connection)
        .map_err(|error| SessionStoreError::operation("verify schema", database_path, error))?;

    if !(completed_turns_matches
        && completed_turn_events_matches
        && foreign_key_matches
        && indexes_match
        && completed_turns_indexes_match)
    {
        return Err(SessionStoreError::operation(
            "verify schema",
            database_path,
            "incompatible sessions schema",
        ));
    }

    Ok(())
}

fn session_schema_version(
    connection: &Connection,
    database_path: &Path,
) -> Result<i64, SessionStoreError> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| SessionStoreError::operation("read schema version", database_path, error))
}

fn migrate_v1_on_open(
    connection: &mut Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| {
            SessionStoreError::operation("start v1 migration", database_path, error)
        })?;

    create_legacy_archive_schema(&transaction, database_path)?;
    copy_legacy_turns(&transaction, database_path)?;
    copy_legacy_turn_events(&transaction, database_path)?;
    migration_fault("during-mutation", database_path)?;
    validate_legacy_archive(&transaction, database_path)?;
    finalize_v2_migration(&transaction, database_path)?;
    migration_fault("before-commit", database_path)?;
    transaction.commit().map_err(|error| {
        SessionStoreError::operation("commit v1 migration", database_path, error)
    })?;
    migration_fault("after-commit-before-reopen", database_path)?;

    let reopened = Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| {
            SessionStoreError::operation("reopen v2 migration", database_path, error)
        })?;
    validate_v4_schema(&reopened, database_path)
}

fn migration_fault(point: &str, database_path: &Path) -> Result<(), SessionStoreError> {
    let fault_path = PathBuf::from(format!("{}.migration-fault", database_path.display()));
    let injected = fs::read_to_string(&fault_path)
        .map(|value| value.trim() == point)
        .unwrap_or(false);

    if injected {
        return Err(SessionStoreError::operation(
            "inject migration fault",
            database_path,
            point,
        ));
    }

    Ok(())
}

fn create_legacy_archive_schema(
    transaction: &Transaction<'_>,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    transaction
        .execute_batch(
            "CREATE TABLE legacy_turns (
                 id INTEGER PRIMARY KEY,
                 status TEXT NOT NULL CHECK(status = 'non_resumable'),
                 reason TEXT NOT NULL,
                 source_event_count INTEGER NOT NULL CHECK(source_event_count >= 0)
             );
             CREATE TABLE legacy_turn_events (
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
                 PRIMARY KEY(turn_id, sequence),
                 FOREIGN KEY(turn_id) REFERENCES legacy_turns(id) ON DELETE CASCADE
             );
             CREATE UNIQUE INDEX legacy_turn_events_turn_sequence
                 ON legacy_turn_events(turn_id, sequence);",
        )
        .map_err(|error| {
            SessionStoreError::operation("create legacy archive", database_path, error)
        })
}

fn copy_legacy_turns(
    transaction: &Transaction<'_>,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    transaction
        .execute(
            "INSERT INTO legacy_turns(id, status, reason, source_event_count)
             SELECT turns.id, 'non_resumable',
                    'v1 lacks session/user/project/title/agent/timestamps',
                    count(events.turn_id)
             FROM completed_turns turns
             LEFT JOIN completed_turn_events events ON events.turn_id = turns.id
             GROUP BY turns.id",
            [],
        )
        .map_err(|error| SessionStoreError::operation("copy legacy turns", database_path, error))?;
    Ok(())
}

fn copy_legacy_turn_events(
    transaction: &Transaction<'_>,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    transaction
        .execute(
            "INSERT INTO legacy_turn_events
             SELECT turn_id, sequence, kind, state, part_kind, call_id, name, input, content, is_error
             FROM completed_turn_events ORDER BY turn_id, sequence",
            [],
        )
        .map_err(|error| SessionStoreError::operation("copy legacy turn events", database_path, error))?;
    Ok(())
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

fn finalize_v2_migration(
    transaction: &Transaction<'_>,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    transaction
        .execute_batch(&format!(
            "{}
                 DROP TABLE completed_turn_events;
              DROP TABLE completed_turns;
              PRAGMA user_version = 4;",
            normalized_session_schema_v4()
        ))
        .map_err(|error| {
            SessionStoreError::operation("finalize v1 migration", database_path, error)
        })?;

    validate_v4_schema(transaction, database_path)
}

fn validate_v2_schema(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    validate_legacy_archive(connection, database_path)?;
    validate_normalized_session_schema(connection, database_path, NORMALIZED_SESSION_SCHEMA_V2)
}

fn validate_v3_schema(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    validate_legacy_archive(connection, database_path)?;
    validate_normalized_session_schema(connection, database_path, &normalized_session_schema_v3())
}

fn validate_v4_schema(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), SessionStoreError> {
    validate_legacy_archive(connection, database_path)?;
    validate_normalized_session_schema(connection, database_path, &normalized_session_schema_v4())
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

fn completed_turn_events_indexes_match(connection: &Connection) -> rusqlite::Result<bool> {
    indexes_match(
        connection,
        "completed_turn_events",
        &COMPLETED_TURN_EVENTS_INDEXES,
    )
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

        if columns.len() != COMPLETED_TURN_EVENTS_INDEX_COLUMNS.len()
            || !columns.iter().zip(COMPLETED_TURN_EVENTS_INDEX_COLUMNS).all(
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

fn completed_turns_indexes_match(connection: &Connection) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare("PRAGMA index_list('completed_turns')")?;
    let indexes = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(indexes.is_empty())
}

fn v1_manifest(connection: &Connection) -> rusqlite::Result<String> {
    let mut statement = connection.prepare(
        "SELECT 'schema|' || type || '|' || name || '|' || quote(sql)
         FROM sqlite_schema WHERE type IN ('index', 'table') AND name LIKE 'completed_turn%'
          UNION ALL SELECT 'turn_count|' || count(*) FROM completed_turns
          UNION ALL SELECT 'event_count|' || count(*) FROM completed_turn_events
          UNION ALL SELECT 'completed_turns|' || id FROM completed_turns
          UNION ALL SELECT 'completed_turn_events|' || turn_id || '|' || sequence || '|' ||
             quote(kind) || '|' || quote(state) || '|' || quote(part_kind) || '|' ||
             quote(call_id) || '|' || quote(name) || '|' || quote(input) || '|' ||
             quote(content) || '|' || quote(is_error)
         FROM completed_turn_events ORDER BY 1",
    )?;
    let lines = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(lines.join("\n"))
}

fn verify_v1_backup(
    source_manifest: &str,
    backup: &Connection,
    backup_path: &Path,
) -> Result<String, SessionStoreError> {
    let quick_check: String = backup
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|error| SessionStoreError::operation("quick check backup", backup_path, error))?;
    validate_v1_schema(backup, backup_path)?;
    let backup_manifest = v1_manifest(backup)
        .map_err(|error| SessionStoreError::operation("read backup", backup_path, error))?;

    if quick_check != "ok" || backup_manifest != source_manifest {
        return Err(SessionStoreError::operation(
            "verify backup",
            backup_path,
            "backup does not match the v1 snapshot",
        ));
    }

    Ok(backup_manifest)
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
    }
}

fn persistable_turn_event(event: &TurnEvent) -> bool {
    !matches!(event, TurnEvent::Usage(_))
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

#[cfg(test)]
mod tests {
    use super::*;
    use agens_core::Usage;

    fn create_v1_fixture(connection: &Connection) {
        connection
            .execute_batch(
                "CREATE TABLE completed_turns (id INTEGER PRIMARY KEY);
                 CREATE TABLE completed_turn_events (
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
                 CREATE UNIQUE INDEX completed_turn_events_turn_sequence
                 ON completed_turn_events(turn_id, sequence);
                 PRAGMA user_version = 1;
                 INSERT INTO completed_turns(id) VALUES(7), (8);
                 INSERT INTO completed_turn_events
                 VALUES(7, 1, 'provider_part', NULL, 'text', NULL, NULL, NULL,
                        'original', NULL);",
            )
            .unwrap();
    }

    #[test]
    fn rejects_tampered_v1_backup_content_and_eventless_turn_id() {
        let source = Connection::open_in_memory().unwrap();
        let backup = Connection::open_in_memory().unwrap();
        create_v1_fixture(&source);
        create_v1_fixture(&backup);

        let source_manifest = v1_manifest(&source).unwrap();
        assert!(verify_v1_backup(&source_manifest, &backup, Path::new("backup.db")).is_ok());

        backup
            .execute("UPDATE completed_turn_events SET content = 'tampered'", [])
            .unwrap();
        assert!(verify_v1_backup(&source_manifest, &backup, Path::new("backup.db")).is_err());

        backup
            .execute("UPDATE completed_turn_events SET content = 'original'", [])
            .unwrap();
        backup
            .execute("UPDATE completed_turns SET id = 9 WHERE id = 8", [])
            .unwrap();
        assert!(verify_v1_backup(&source_manifest, &backup, Path::new("backup.db")).is_err());
    }

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

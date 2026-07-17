use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

use agens_core::{PermissionDecision, PermissionPattern, ProjectPermissionGrant};
use rusqlite::{Connection, Transaction, params};

const PERMISSIONS_DATABASE: &str = "rust-permissions.db";
const PERMISSIONS_SCHEMA_VERSION: i64 = 1;

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
        0 => connection
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
            }),
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
    connection
        .prepare(
            "SELECT project, decision, tool_kind, tool_value, target_kind, target_value
             FROM permission_grants LIMIT 0",
        )
        .map(|_| ())
        .map_err(|error| {
            PermissionGrantStoreError::operation("verify schema", database_path, error)
        })
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

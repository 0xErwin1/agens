//! The unified `agens.db` open path: permissions, pragmas, and the migration ledger.
//!
//! Everything in this module is crate-private. The three store types in `lib.rs` each call
//! [`open_unified_database`] and keep their own `Connection` to the resulting file.

use std::{
    collections::BTreeSet,
    fmt, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, TransactionBehavior, params};

const UNIFIED_DATABASE: &str = "agens.db";

/// A store-agnostic failure from the shared open path.
///
/// Each store maps this into its own error type via a `from_database` constructor so that the
/// rendered message keeps that store's domain prefix and the existing error category.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DatabaseError {
    operation: String,
    path: PathBuf,
    detail: String,
}

impl DatabaseError {
    fn new(operation: impl Into<String>, path: &Path, detail: impl fmt::Display) -> Self {
        Self {
            operation: operation.into(),
            path: path.to_path_buf(),
            detail: detail.to_string(),
        }
    }

    pub(crate) fn operation(&self) -> &str {
        &self.operation
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }
}

struct Migration {
    id: &'static str,
    ddl: fn() -> String,
}

const MIGRATIONS: [Migration; 1] = [Migration {
    id: "0001_permission_grants",
    ddl: permission_grants_ddl,
}];

/// Opens the single `agens.db` file inside `data_directory`, applying the full open contract on
/// every call: directory and file permissions, `busy_timeout`, `foreign_keys`, `journal_mode`,
/// and any pending migration.
///
/// This does not yet reject a foreign or unrecognized file layout; that guard is added on top of
/// this open path once it exists.
pub(crate) fn open_unified_database(
    data_directory: &Path,
) -> Result<(PathBuf, Connection), DatabaseError> {
    fs::create_dir_all(data_directory)
        .map_err(|error| DatabaseError::new("create data directory", data_directory, error))?;
    restrict_permissions(data_directory, 0o700)?;

    let database_path = data_directory.join(UNIFIED_DATABASE);
    let mut connection = Connection::open(&database_path)
        .map_err(|error| DatabaseError::new("open database", &database_path, error))?;
    restrict_permissions(&database_path, 0o600)?;

    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| DatabaseError::new("configure busy timeout", &database_path, error))?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|error| DatabaseError::new("enable foreign keys", &database_path, error))?;

    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|error| DatabaseError::new("enable WAL", &database_path, error))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(DatabaseError::new(
            "enable WAL",
            &database_path,
            format!("journal mode is {mode}"),
        ));
    }

    run_pending_migrations(&mut connection, &database_path)?;

    Ok((database_path, connection))
}

/// Applies every migration in [`MIGRATIONS`] whose id is not yet present in the
/// `schema_migrations` ledger, each DDL and its ledger row inside one transaction.
///
/// Migrations are append-only: once released, neither a migration's id nor its DDL may change.
/// Every migration shipped so far is additive `CREATE`, which is why no backup, verification, or
/// fault-injection machinery exists here; the first destructive migration must reintroduce one.
fn run_pending_migrations(connection: &mut Connection, path: &Path) -> Result<(), DatabaseError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 id TEXT PRIMARY KEY,
                 applied_at INTEGER NOT NULL
             );",
        )
        .map_err(|error| DatabaseError::new("create migration ledger", path, error))?;

    let applied = read_applied_migration_ids(connection, path)?;

    for migration in MIGRATIONS {
        if applied.contains(migration.id) {
            continue;
        }

        let operation = format!("apply migration {}", migration.id);
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| DatabaseError::new(operation.clone(), path, error))?;
        transaction
            .execute_batch(&(migration.ddl)())
            .map_err(|error| DatabaseError::new(operation.clone(), path, error))?;
        transaction
            .execute(
                "INSERT INTO schema_migrations (id, applied_at)
                 VALUES (?1, CAST(strftime('%s','now') AS INTEGER))",
                params![migration.id],
            )
            .map_err(|error| DatabaseError::new(operation.clone(), path, error))?;
        transaction
            .commit()
            .map_err(|error| DatabaseError::new(operation, path, error))?;
    }

    Ok(())
}

fn read_applied_migration_ids(
    connection: &Connection,
    path: &Path,
) -> Result<BTreeSet<String>, DatabaseError> {
    let mut statement = connection
        .prepare("SELECT id FROM schema_migrations")
        .map_err(|error| DatabaseError::new("read migration ledger", path, error))?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| DatabaseError::new("read migration ledger", path, error))?
        .collect::<rusqlite::Result<BTreeSet<String>>>()
        .map_err(|error| DatabaseError::new("read migration ledger", path, error))?;

    Ok(ids)
}

fn permission_grants_ddl() -> String {
    "
    CREATE TABLE permission_grants (
        id INTEGER PRIMARY KEY,
        project TEXT NOT NULL,
        decision TEXT NOT NULL,
        tool_kind TEXT NOT NULL,
        tool_value TEXT,
        target_kind TEXT NOT NULL,
        target_value TEXT
    );
    CREATE INDEX permission_grants_project
        ON permission_grants(project, id);
    "
    .to_owned()
}

#[cfg(unix)]
fn restrict_permissions(path: &Path, maximum_mode: u32) -> Result<(), DatabaseError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::metadata(path)
        .map_err(|error| DatabaseError::new("inspect permissions", path, error))?;
    let current_mode = metadata.mode() & 0o777;
    let restricted_mode = current_mode & maximum_mode;

    if restricted_mode != current_mode {
        fs::set_permissions(path, fs::Permissions::from_mode(restricted_mode))
            .map_err(|error| DatabaseError::new("restrict permissions", path, error))?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_: &Path, _: u32) -> Result<(), DatabaseError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_directory() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let suffix = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "agens-store-database-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn busy_timeout_is_5000_milliseconds() {
        let directory = data_directory();
        let (_, connection) = open_unified_database(&directory).unwrap();

        let busy_timeout: i64 = connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();

        assert_eq!(busy_timeout, 5000);

        fs::remove_dir_all(directory).unwrap();
    }
}

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

const MIGRATIONS: [Migration; 2] = [
    Migration {
        id: "0001_permission_grants",
        ddl: permission_grants_ddl,
    },
    Migration {
        id: "0002_model_preference",
        ddl: model_preference_ddl,
    },
];

/// Opens the single `agens.db` file inside `data_directory`, applying the full open contract on
/// every call: directory and file permissions, `busy_timeout`, `foreign_keys`, the layout guard,
/// `journal_mode`, and any pending migration.
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

    guard_layout(&connection, &database_path)?;

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

/// Rejects a file that already holds user tables but no `schema_migrations` ledger.
///
/// A file in this shape is not an empty database and not one this open path has ever migrated —
/// most plausibly a legacy or foreign SQLite file placed at `agens.db` by hand. Counting ledger
/// ROWS rather than only checking for the ledger TABLE also covers a crash between the ledger's
/// own bootstrap and the first migration's commit: an empty ledger with no other user tables is
/// still a fresh, unmigrated database and is allowed to proceed.
fn guard_layout(connection: &Connection, path: &Path) -> Result<(), DatabaseError> {
    let user_table_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE type = 'table'
               AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
               AND name != 'schema_migrations'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| DatabaseError::new("check database layout", path, error))?;

    if user_table_count == 0 {
        return Ok(());
    }

    let ledger_table_exists: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE type = 'table' AND name = 'schema_migrations'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| DatabaseError::new("check database layout", path, error))?;

    let ledger_row_count: i64 = if ledger_table_exists == 0 {
        0
    } else {
        connection
            .query_row("SELECT count(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .map_err(|error| DatabaseError::new("check database layout", path, error))?
    };

    if ledger_row_count == 0 {
        return Err(DatabaseError::new(
            "check database layout",
            path,
            "unrecognized database layout",
        ));
    }

    Ok(())
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

fn model_preference_ddl() -> String {
    "
    CREATE TABLE model_preference (
        id INTEGER PRIMARY KEY CHECK(id = 1),
        model TEXT NOT NULL CHECK(model <> ''),
        reasoning_effort TEXT
    );
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
    fn migration_ids_are_unique_zero_padded_and_sorted() {
        let ids: Vec<&str> = MIGRATIONS.iter().map(|migration| migration.id).collect();
        let mut sorted_ids = ids.clone();
        sorted_ids.sort_unstable();

        assert_eq!(
            ids, sorted_ids,
            "MIGRATIONS must be declared in ascending lexicographic id order"
        );

        let unique_ids: BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique_ids.len(), ids.len(), "migration ids must be unique");

        for id in &ids {
            let prefix = id.split('_').next().unwrap();
            assert_eq!(
                prefix.len(),
                4,
                "migration id {id} must start with a 4-digit zero-padded prefix"
            );
            assert!(
                prefix.chars().all(|character| character.is_ascii_digit()),
                "migration id {id} must start with a numeric prefix"
            );
        }
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

    fn journal_mode(connection: &Connection) -> String {
        connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap()
    }

    fn foreign_keys(connection: &Connection) -> i64 {
        connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap()
    }

    /// Neither an integration test nor a store method can observe `foreign_keys`, since it is a
    /// per-connection setting with no on-disk trace and `open_unified_database` is `pub(crate)`.
    /// This is also the empirical resolution of the design's one open question: whether a SECOND
    /// connection issuing `PRAGMA journal_mode = WAL` while a first connection holds the same file
    /// open returns `wal` or fails. Measured here, not inferred: both connections stay open
    /// simultaneously and both must report the full pragma set.
    #[test]
    fn pragmas_are_uniform_regardless_of_which_store_opens_first() {
        let directory = data_directory();

        let (_, first_connection) = open_unified_database(&directory).unwrap();
        let (_, second_connection) = open_unified_database(&directory).unwrap();

        assert_eq!(journal_mode(&first_connection), "wal");
        assert_eq!(journal_mode(&second_connection), "wal");
        assert_eq!(foreign_keys(&first_connection), 1);
        assert_eq!(foreign_keys(&second_connection), 1);

        drop(first_connection);
        drop(second_connection);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_reopened_connection_observes_the_full_pragma_set() {
        let directory = data_directory();

        let (_, first_connection) = open_unified_database(&directory).unwrap();
        drop(first_connection);

        let (_, reopened_connection) = open_unified_database(&directory).unwrap();

        assert_eq!(journal_mode(&reopened_connection), "wal");
        assert_eq!(foreign_keys(&reopened_connection), 1);
        let busy_timeout: i64 = reopened_connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout, 5000);

        fs::remove_dir_all(directory).unwrap();
    }
}

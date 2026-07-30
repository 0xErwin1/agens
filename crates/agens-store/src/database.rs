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

const MIGRATIONS: [Migration; 7] = [
    Migration {
        id: "0001_permission_grants",
        ddl: permission_grants_ddl,
    },
    Migration {
        id: "0002_model_preference",
        ddl: model_preference_ddl,
    },
    Migration {
        id: "0003_sessions_v5",
        ddl: sessions_v5_ddl,
    },
    Migration {
        id: "0004_tool_result_facts",
        ddl: tool_result_facts_ddl,
    },
    Migration {
        id: "0005_session_confinement_root",
        ddl: session_confinement_root_ddl,
    },
    Migration {
        id: "0006_session_bypass_permission_prompts",
        ddl: session_bypass_permission_prompts_ddl,
    },
    Migration {
        id: "0007_model_preference_by_source",
        ddl: model_preference_by_source_ddl,
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

/// The remembered model and effort, keyed by the model source that produced them.
///
/// Supersedes the single-row `model_preference` table, whose one slot made a pick from any source
/// the pick for every source: the next session on a different provider read a model that provider
/// cannot serve and had to announce a fallback. The superseded table is left in place because
/// migrations here are append-only and additive; nothing reads or writes it any more, and its one
/// row is not carried over, since the source it was chosen under was never recorded.
fn model_preference_by_source_ddl() -> String {
    "
    CREATE TABLE model_preference_by_source (
        source TEXT PRIMARY KEY CHECK(source <> ''),
        model TEXT NOT NULL CHECK(model <> ''),
        reasoning_effort TEXT
    );
    "
    .to_owned()
}

/// The archive tables that hold session history predating the normalized `sessions`/`turns`
/// schema. Every unified database creates them empty; `SessionStore::list_completed_turns` and
/// `load_completed_turn_for_resume` still read them, so migration 0003 must keep creating them
/// verbatim rather than dropping them as unused.
const LEGACY_ARCHIVE_SCHEMA: &str = "
    CREATE TABLE legacy_turns (
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
        ON legacy_turn_events(turn_id, sequence);
";

fn sessions_v5_ddl() -> String {
    format!(
        "{LEGACY_ARCHIVE_SCHEMA}{}",
        crate::normalized_session_schema_v5()
    )
}

/// The evidence ledger a running turn writes tool-result facts to. Keyed by
/// `(attempt_id, sequence)` rather than by turn: a `turns` row only exists once
/// a turn completes, but facts are produced mid-turn, so the attempt is the
/// only durable key available at write time. `tool_call_id` is a correlation
/// column, not part of that key. `path_status` distinguishes a variant that
/// carries no path (`not_applicable`) from one whose reported path violated
/// the `FactPath` contract (`unrepresentable`) — collapsing both to a NULL
/// `path` would make an unrepresentable path indistinguishable from an absent
/// one.
fn tool_result_facts_ddl() -> String {
    "
    CREATE TABLE tool_result_facts (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL,
        attempt_id INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence > 0),
        tool_call_id TEXT NOT NULL,
        tool TEXT NOT NULL CHECK(tool IN ('write','edit','bash','read','search')),
        outcome TEXT NOT NULL CHECK(outcome IN ('succeeded','failed','denied')),
        path TEXT,
        path_status TEXT NOT NULL CHECK(path_status IN ('relative','unrepresentable','not_applicable')),
        exit_code INTEGER,
        is_new_file INTEGER,
        bytes_written INTEGER,
        lines_written INTEGER,
        lines_added INTEGER,
        lines_removed INTEGER,
        match_count INTEGER,
        truncated INTEGER,
        recorded_at INTEGER NOT NULL,
        FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
        FOREIGN KEY(attempt_id) REFERENCES session_attempts(id) ON DELETE CASCADE
    );
    CREATE UNIQUE INDEX tool_result_facts_attempt_sequence
        ON tool_result_facts(attempt_id, sequence);
    CREATE INDEX tool_result_facts_call
        ON tool_result_facts(session_id, tool_call_id);
    "
    .to_owned()
}

/// The literal filesystem root a session's tools are confined to, distinct from `project`: the
/// grouping/display/permission-grant key produced by the lossy `Path::display()`. NULL on every
/// pre-existing row, which the read path falls back to `project` for.
fn session_confinement_root_ddl() -> String {
    "ALTER TABLE sessions ADD COLUMN confinement_root TEXT;".to_owned()
}

/// The recorded per-session bypass-permission-prompts value, distinct from the
/// `agent.bypass_permission_prompts` configuration setting: NULL means "never recorded" and the
/// read path falls back to configuration, exactly as `confinement_root` falls back to `project`.
fn session_bypass_permission_prompts_ddl() -> String {
    "ALTER TABLE sessions ADD COLUMN bypass_permission_prompts INTEGER;".to_owned()
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
    fn migration_0004_is_purely_additive_and_creates_the_tool_result_facts_table() {
        let directory = data_directory();
        let (_, connection) = open_unified_database(&directory).unwrap();

        for pre_existing_table in [
            "sessions",
            "session_attempts",
            "permission_grants",
            "model_preference",
        ] {
            let column_count: i64 = connection
                .query_row(
                    &format!("SELECT count(*) FROM pragma_table_info('{pre_existing_table}')"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                column_count > 0,
                "{pre_existing_table} must be unchanged by migration 0004"
            );
        }

        let mut statement = connection
            .prepare("SELECT name FROM pragma_table_info('tool_result_facts') ORDER BY cid")
            .unwrap();
        let columns: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        assert_eq!(
            columns,
            vec![
                "id",
                "session_id",
                "attempt_id",
                "sequence",
                "tool_call_id",
                "tool",
                "outcome",
                "path",
                "path_status",
                "exit_code",
                "is_new_file",
                "bytes_written",
                "lines_written",
                "lines_added",
                "lines_removed",
                "match_count",
                "truncated",
                "recorded_at",
            ]
        );

        fs::remove_dir_all(directory).unwrap();
    }

    fn ordered_table_columns(connection: &Connection, table: &str) -> Vec<String> {
        let mut statement = connection
            .prepare(&format!(
                "SELECT name FROM pragma_table_info('{table}') ORDER BY cid"
            ))
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn migration_0005_is_purely_additive_and_adds_a_nullable_confinement_root_column() {
        let directory = data_directory();
        let (_, connection) = open_unified_database(&directory).unwrap();

        // Every table migration 0005 must leave untouched keeps the exact column set (name and
        // order) it already had after migration 0004 — not merely a non-empty column count, which
        // would stay green even if a migration dropped or renamed columns.
        for (unaffected_table, expected_columns) in [
            (
                "session_attempts",
                vec![
                    "id",
                    "session_id",
                    "sequence",
                    "status",
                    "failure_kind",
                    "retry_prompt",
                    "started_at",
                    "finished_at",
                    "completed_turn_sequence",
                ],
            ),
            (
                "permission_grants",
                vec![
                    "id",
                    "project",
                    "decision",
                    "tool_kind",
                    "tool_value",
                    "target_kind",
                    "target_value",
                ],
            ),
            ("model_preference", vec!["id", "model", "reasoning_effort"]),
            (
                "tool_result_facts",
                vec![
                    "id",
                    "session_id",
                    "attempt_id",
                    "sequence",
                    "tool_call_id",
                    "tool",
                    "outcome",
                    "path",
                    "path_status",
                    "exit_code",
                    "is_new_file",
                    "bytes_written",
                    "lines_written",
                    "lines_added",
                    "lines_removed",
                    "match_count",
                    "truncated",
                    "recorded_at",
                ],
            ),
        ] {
            assert_eq!(
                ordered_table_columns(&connection, unaffected_table),
                expected_columns,
                "{unaffected_table} must be unchanged by migration 0005"
            );
        }

        // `sessions` keeps every pre-existing column, unchanged and in the same order. This test
        // opens a freshly created database, so every later migration (including 0006) has already
        // applied too — it asserts confinement_root's own position, not the full column set.
        assert_eq!(
            ordered_table_columns(&connection, "sessions"),
            vec![
                "id",
                "project",
                "title",
                "active_agent",
                "created_at",
                "updated_at",
                "completed_turn_count",
                "resumable",
                "provider_id",
                "model_id",
                "reasoning_effort",
                "confinement_root",
                "bypass_permission_prompts",
            ]
        );

        let (not_null, default_value): (i64, Option<String>) = connection
            .query_row(
                "SELECT \"notnull\", dflt_value FROM pragma_table_info('sessions')
                 WHERE name = 'confinement_root'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(not_null, 0, "confinement_root must be nullable");
        assert_eq!(default_value, None);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migration_0006_is_purely_additive_and_adds_a_nullable_bypass_permission_prompts_column() {
        let directory = data_directory();
        let (_, connection) = open_unified_database(&directory).unwrap();

        // Every table migration 0006 must leave untouched keeps the exact column set (name and
        // order) it already had after migration 0005 — not merely a non-empty column count, which
        // would stay green even if a migration dropped or renamed columns.
        for (unaffected_table, expected_columns) in [
            (
                "session_attempts",
                vec![
                    "id",
                    "session_id",
                    "sequence",
                    "status",
                    "failure_kind",
                    "retry_prompt",
                    "started_at",
                    "finished_at",
                    "completed_turn_sequence",
                ],
            ),
            (
                "permission_grants",
                vec![
                    "id",
                    "project",
                    "decision",
                    "tool_kind",
                    "tool_value",
                    "target_kind",
                    "target_value",
                ],
            ),
            ("model_preference", vec!["id", "model", "reasoning_effort"]),
            (
                "tool_result_facts",
                vec![
                    "id",
                    "session_id",
                    "attempt_id",
                    "sequence",
                    "tool_call_id",
                    "tool",
                    "outcome",
                    "path",
                    "path_status",
                    "exit_code",
                    "is_new_file",
                    "bytes_written",
                    "lines_written",
                    "lines_added",
                    "lines_removed",
                    "match_count",
                    "truncated",
                    "recorded_at",
                ],
            ),
        ] {
            assert_eq!(
                ordered_table_columns(&connection, unaffected_table),
                expected_columns,
                "{unaffected_table} must be unchanged by migration 0006"
            );
        }

        // `sessions` keeps every pre-existing column, unchanged and in the same order, with
        // exactly one new column appended.
        assert_eq!(
            ordered_table_columns(&connection, "sessions"),
            vec![
                "id",
                "project",
                "title",
                "active_agent",
                "created_at",
                "updated_at",
                "completed_turn_count",
                "resumable",
                "provider_id",
                "model_id",
                "reasoning_effort",
                "confinement_root",
                "bypass_permission_prompts",
            ]
        );

        let (not_null, default_value): (i64, Option<String>) = connection
            .query_row(
                "SELECT \"notnull\", dflt_value FROM pragma_table_info('sessions')
                 WHERE name = 'bypass_permission_prompts'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(not_null, 0, "bypass_permission_prompts must be nullable");
        assert_eq!(default_value, None);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migration_0007_is_purely_additive_and_leaves_the_superseded_preference_table_intact() {
        let directory = data_directory();
        let (_, connection) = open_unified_database(&directory).unwrap();

        assert_eq!(
            ordered_table_columns(&connection, "model_preference"),
            vec!["id", "model", "reasoning_effort"],
            "migration 0007 must not rewrite the superseded preference table"
        );
        assert_eq!(
            ordered_table_columns(&connection, "model_preference_by_source"),
            vec!["source", "model", "reasoning_effort"]
        );

        connection
            .execute_batch(
                "INSERT INTO model_preference_by_source (source, model, reasoning_effort)
                     VALUES ('moonshot-api', 'kimi-k3', NULL);
                 INSERT INTO model_preference_by_source (source, model, reasoning_effort)
                     VALUES ('chatgpt-subscription', 'gpt-5.5', NULL);",
            )
            .expect("distinct sources must coexist");

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

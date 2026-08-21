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
    /// Tables whose row count the migration must carry across unchanged.
    ///
    /// Empty for an additive `CREATE`; a migration that rebuilds a populated table lists it
    /// here so a copy that loses rows aborts the transaction instead of committing the loss.
    preserved_tables: &'static [&'static str],
}

const MIGRATIONS: [Migration; 14] = [
    Migration {
        id: "0001_permission_grants",
        ddl: permission_grants_ddl,
        preserved_tables: &[],
    },
    Migration {
        id: "0002_model_preference",
        ddl: model_preference_ddl,
        preserved_tables: &[],
    },
    Migration {
        id: "0003_sessions_v5",
        ddl: sessions_v5_ddl,
        preserved_tables: &[],
    },
    Migration {
        id: "0004_tool_result_facts",
        ddl: tool_result_facts_ddl,
        preserved_tables: &[],
    },
    Migration {
        id: "0005_session_confinement_root",
        ddl: session_confinement_root_ddl,
        preserved_tables: &[],
    },
    Migration {
        id: "0006_session_bypass_permission_prompts",
        ddl: session_bypass_permission_prompts_ddl,
        preserved_tables: &[],
    },
    Migration {
        id: "0007_model_preference_by_source",
        ddl: model_preference_by_source_ddl,
        preserved_tables: &[],
    },
    Migration {
        id: "0008_prompt_memory",
        ddl: prompt_memory_ddl,
        preserved_tables: &[],
    },
    Migration {
        id: "0009_media",
        ddl: media_ddl,
        // `tool_result_facts` is not rebuilt, but its rows hang off `session_attempts` by an
        // `ON DELETE CASCADE` the rebuild's `DROP TABLE` would fire, so the count must hold too.
        preserved_tables: &["message_parts", "session_attempts", "tool_result_facts"],
    },
    Migration {
        id: "0010_prompt_memory_media",
        ddl: prompt_memory_media_ddl,
        preserved_tables: &["prompt_history", "prompt_stash"],
    },
    Migration {
        id: "0011_session_fork_lineage",
        ddl: session_fork_lineage_ddl,
        preserved_tables: &[],
    },
    Migration {
        id: "0012_directives",
        ddl: directives_ddl,
        preserved_tables: &[],
    },
    Migration {
        id: "0013_supervisor_role",
        ddl: supervisor_role_ddl,
        // No preserved-table guard: it counts rows before the DDL runs, and a
        // database that never carried sessions has no `messages` to count. The
        // rebuild proves preservation in a test of its own instead
        // (`a_supervisor_role_migration_keeps_every_message_and_part`).
        preserved_tables: &[],
    },
    Migration {
        id: "0014_directive_child_target",
        ddl: directive_child_target_ddl,
        preserved_tables: &["directives"],
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
///
/// Most migrations are additive `CREATE`, but some rebuild a populated table by copying it into a
/// replacement and dropping the original. Those declare the tables they must carry across in
/// [`Migration::preserved_tables`], and the row count of each is compared before and after the DDL
/// inside the same transaction: a copy that lost rows fails the migration and rolls back, so a
/// broken rebuild leaves the old schema and the user's rows in place rather than committing the
/// loss. No backup or fault-injection machinery exists beyond that.
///
/// That row guard only sees the tables a migration declares, so it cannot see a table emptied
/// behind the migration's back: with foreign keys enforced, `DROP TABLE` performs an implicit
/// `DELETE FROM` that fires `ON DELETE CASCADE` into children the rebuild never mentions —
/// dropping `session_attempts` mid-rebuild takes `tool_result_facts` with it. Enforcement is a
/// per-connection setting that cannot be changed inside a transaction, so each migration runs
/// with it suspended and its result is validated by `PRAGMA foreign_key_check` before the
/// transaction commits.
fn run_pending_migrations(connection: &mut Connection, path: &Path) -> Result<(), DatabaseError> {
    apply_migrations(connection, path, &MIGRATIONS)
}

fn apply_migrations(
    connection: &mut Connection,
    path: &Path,
    migrations: &[Migration],
) -> Result<(), DatabaseError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 id TEXT PRIMARY KEY,
                 applied_at INTEGER NOT NULL
             );",
        )
        .map_err(|error| DatabaseError::new("create migration ledger", path, error))?;

    let applied = read_applied_migration_ids(connection, path)?;

    for migration in migrations {
        if applied.contains(migration.id) {
            continue;
        }

        set_foreign_keys(connection, path, false)?;
        let outcome = apply_migration(connection, path, migration);
        let restored = set_foreign_keys(connection, path, true);

        outcome?;
        restored?;
    }

    Ok(())
}

fn apply_migration(
    connection: &mut Connection,
    path: &Path,
    migration: &Migration,
) -> Result<(), DatabaseError> {
    let operation = format!("apply migration {}", migration.id);
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| DatabaseError::new(operation.clone(), path, error))?;

    let mut counts_before = Vec::with_capacity(migration.preserved_tables.len());
    for table in migration.preserved_tables {
        counts_before.push(count_rows(&transaction, table, &operation, path)?);
    }

    transaction
        .execute_batch(&(migration.ddl)())
        .map_err(|error| DatabaseError::new(operation.clone(), path, error))?;

    for (table, before) in migration.preserved_tables.iter().zip(counts_before) {
        let after = count_rows(&transaction, table, &operation, path)?;
        if after != before {
            return Err(DatabaseError::new(
                operation,
                path,
                format!("{table} kept {after} of {before} rows across the rebuild"),
            ));
        }
    }

    let violations: i64 = transaction
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .map_err(|error| DatabaseError::new(operation.clone(), path, error))?;
    if violations > 0 {
        return Err(DatabaseError::new(
            operation,
            path,
            format!("the migrated schema holds {violations} foreign key violation(s)"),
        ));
    }

    transaction
        .execute(
            "INSERT INTO schema_migrations (id, applied_at)
             VALUES (?1, CAST(strftime('%s','now') AS INTEGER))",
            params![migration.id],
        )
        .map_err(|error| DatabaseError::new(operation.clone(), path, error))?;
    transaction
        .commit()
        .map_err(|error| DatabaseError::new(operation, path, error))
}

/// Sets foreign key enforcement on this connection and verifies it took effect.
///
/// The pragma is silently ignored inside a transaction, so reading it back is the only way to
/// know a migration is not about to run under the enforcement it asked to suspend.
fn set_foreign_keys(
    connection: &Connection,
    path: &Path,
    enabled: bool,
) -> Result<(), DatabaseError> {
    connection
        .pragma_update(None, "foreign_keys", if enabled { "ON" } else { "OFF" })
        .map_err(|error| DatabaseError::new("configure foreign keys", path, error))?;

    let effective: i64 = connection
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .map_err(|error| DatabaseError::new("configure foreign keys", path, error))?;
    if (effective == 1) != enabled {
        return Err(DatabaseError::new(
            "configure foreign keys",
            path,
            format!("foreign_keys is {effective}"),
        ));
    }

    Ok(())
}

fn count_rows(
    connection: &Connection,
    table: &str,
    operation: &str,
    path: &Path,
) -> Result<i64, DatabaseError> {
    // Table names come from the crate-private MIGRATIONS table only.
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .map_err(|error| DatabaseError::new(operation.to_owned(), path, error))
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

/// Widens the message role check so a supervisor message can be persisted.
///
/// A rebuild rather than an `ALTER`: SQLite cannot change a `CHECK`, and a
/// constraint that rejects a role the domain now has means the whole turn fails
/// to save, not just the one message. `message_parts` is rebuilt with it because
/// its cascade fires on the drop.
fn supervisor_role_ddl() -> String {
    "
    -- A database that never carried sessions still runs this migration, so the
    -- rebuild starts by making its inputs exist. Both are no-ops when the
    -- tables are already there, and create them empty when they are not.
    CREATE TABLE IF NOT EXISTS messages (
        session_id INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence > 0),
        turn_sequence INTEGER NOT NULL CHECK(turn_sequence > 0),
        role TEXT NOT NULL CHECK(role IN('system', 'user', 'assistant', 'tool')),
        PRIMARY KEY(session_id, sequence),
        FOREIGN KEY(session_id, turn_sequence) REFERENCES turns(session_id, sequence) ON DELETE CASCADE
    );
    CREATE TABLE IF NOT EXISTS message_parts (
        session_id INTEGER NOT NULL,
        message_sequence INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence >= 0),
        kind TEXT NOT NULL CHECK(kind IN('text', 'reasoning', 'tool_call', 'tool_result', 'media')),
        text TEXT,
        call_id TEXT,
        name TEXT,
        input_json TEXT,
        content TEXT,
        is_error INTEGER CHECK(is_error IN(0, 1)),
        media_id INTEGER,
        mime TEXT,
        PRIMARY KEY(session_id, message_sequence, sequence)
    );
    CREATE TABLE message_parts_keep AS SELECT * FROM message_parts;
    CREATE TABLE messages_keep AS SELECT * FROM messages;
    DROP TABLE message_parts;
    DROP TABLE messages;
    CREATE TABLE messages (
        session_id INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence > 0),
        turn_sequence INTEGER NOT NULL CHECK(turn_sequence > 0),
        role TEXT NOT NULL CHECK(role IN('system', 'user', 'assistant', 'tool', 'supervisor')),
        PRIMARY KEY(session_id, sequence),
        FOREIGN KEY(session_id, turn_sequence) REFERENCES turns(session_id, sequence) ON DELETE CASCADE
    );
    INSERT INTO messages SELECT * FROM messages_keep;
    DROP TABLE messages_keep;
    CREATE INDEX messages_turn_order ON messages(session_id, turn_sequence, sequence);
    CREATE TABLE message_parts (
        session_id INTEGER NOT NULL,
        message_sequence INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence >= 0),
        kind TEXT NOT NULL CHECK(kind IN('text', 'reasoning', 'tool_call', 'tool_result', 'media')),
        text TEXT,
        call_id TEXT,
        name TEXT,
        input_json TEXT,
        content TEXT,
        is_error INTEGER CHECK(is_error IN(0, 1)),
        media_id INTEGER,
        mime TEXT,
        PRIMARY KEY(session_id, message_sequence, sequence),
        FOREIGN KEY(session_id, message_sequence) REFERENCES messages(session_id, sequence) ON DELETE CASCADE,
        FOREIGN KEY(media_id) REFERENCES media(id),
        CHECK((kind IN('text', 'reasoning') AND text IS NOT NULL AND call_id IS NULL AND name IS NULL AND input_json IS NULL AND content IS NULL AND is_error IS NULL AND media_id IS NULL AND mime IS NULL) OR (kind = 'tool_call' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NOT NULL AND name <> '' AND input_json IS NOT NULL AND content IS NULL AND is_error IS NULL AND media_id IS NULL AND mime IS NULL) OR (kind = 'tool_result' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NULL AND input_json IS NULL AND content IS NOT NULL AND is_error IS NOT NULL AND media_id IS NULL AND mime IS NULL) OR (kind = 'media' AND text IS NULL AND call_id IS NULL AND name IS NULL AND input_json IS NULL AND content IS NULL AND is_error IS NULL AND media_id IS NOT NULL AND mime IS NOT NULL AND mime <> ''))
    );
    INSERT INTO message_parts SELECT * FROM message_parts_keep;
    DROP TABLE message_parts_keep;
    CREATE INDEX parts_message_order ON message_parts(session_id, message_sequence, sequence);
    "
    .to_owned()
}

/// The durable queue a running turn drains at a safe point.
///
/// Durable rather than in-memory so a message encoded while the turn was
/// working is not lost if the process dies before the turn reaches its next
/// boundary — which, measured on this repo, can be half an hour away.
///
/// `delivered_at` is set rather than the row deleted: a turn that was steered
/// mid-flight is only explicable afterwards if the steering is still there to
/// read.
fn directives_ddl() -> String {
    "
    CREATE TABLE directives (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL,
        source TEXT NOT NULL CHECK(source IN ('human', 'supervisor')),
        grain TEXT NOT NULL CHECK(grain IN ('tool_call', 'turn')),
        text TEXT NOT NULL CHECK(text <> ''),
        created_at TEXT NOT NULL,
        delivered_at TEXT
    );
    CREATE INDEX directives_pending
        ON directives(session_id, grain, id)
        WHERE delivered_at IS NULL;
    "
    .to_owned()
}

/// Gives a directive a second kind of addressee: a delegated child turn.
///
/// A child cannot read its parent session's queue. Several children run under
/// one session at a time, so whichever drained first would take a message meant
/// for another, and the parent turn would lose whatever a child got to first.
/// The two columns are exclusive rather than a target kind plus one opaque id,
/// because a session addressee is an integer with a foreign key's shape and a
/// child addressee is the reference its own diagnostics publish.
fn directive_child_target_ddl() -> String {
    "
    CREATE TABLE directives_keep AS SELECT * FROM directives;
    DROP TABLE directives;
    CREATE TABLE directives (
        id INTEGER PRIMARY KEY,
        session_id INTEGER,
        child TEXT,
        source TEXT NOT NULL CHECK(source IN ('human', 'supervisor')),
        grain TEXT NOT NULL CHECK(grain IN ('tool_call', 'turn')),
        text TEXT NOT NULL CHECK(text <> ''),
        created_at TEXT NOT NULL,
        delivered_at TEXT,
        CHECK((session_id IS NULL) <> (child IS NULL))
    );
    INSERT INTO directives (id, session_id, child, source, grain, text, created_at, delivered_at)
        SELECT id, session_id, NULL, source, grain, text, created_at, delivered_at
        FROM directives_keep;
    DROP TABLE directives_keep;
    CREATE INDEX directives_pending
        ON directives(session_id, grain, id)
        WHERE delivered_at IS NULL;
    CREATE INDEX directives_child_pending
        ON directives(child, grain, id)
        WHERE delivered_at IS NULL;
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

/// Global composer prompt history and independent LIFO stash.
///
/// Text-only native columns; history is chronological by `id` ASC, stash is LIFO with the highest
/// `id` as the stack top. No product-level row cap.
fn prompt_memory_ddl() -> String {
    "
    CREATE TABLE prompt_history (
        id INTEGER PRIMARY KEY,
        text TEXT NOT NULL CHECK(text <> ''),
        created_at INTEGER NOT NULL
    );
    CREATE INDEX prompt_history_id ON prompt_history(id);

    CREATE TABLE prompt_stash (
        id INTEGER PRIMARY KEY,
        text TEXT NOT NULL CHECK(text <> ''),
        created_at INTEGER NOT NULL
    );
    CREATE INDEX prompt_stash_id ON prompt_stash(id);
    "
    .to_owned()
}

/// Durable media index plus message-part and retry-boundary support for multimodal attachments.
///
/// Blobs live at `{data_directory}/media/{sha256}` outside SQLite. `message_parts` is rebuilt so
/// existing CHECK constraints can admit `kind = 'media'` with `media_id`/`mime`; pre-existing
/// parts copy through with those columns NULL. `retry_media_ids` is JSON text of media ids only —
/// never source paths — so resume/retry can resolve blobs without the original file path.
fn media_ddl() -> String {
    "
    CREATE TABLE media (
        id INTEGER PRIMARY KEY,
        sha256 TEXT NOT NULL UNIQUE CHECK(length(sha256) = 64),
        mime TEXT NOT NULL CHECK(mime <> ''),
        byte_len INTEGER NOT NULL CHECK(byte_len > 0 AND byte_len <= 10485760),
        created_at INTEGER NOT NULL
    );

    CREATE TABLE message_parts_new (
        session_id INTEGER NOT NULL,
        message_sequence INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence >= 0),
        kind TEXT NOT NULL CHECK(kind IN('text', 'reasoning', 'tool_call', 'tool_result', 'media')),
        text TEXT,
        call_id TEXT,
        name TEXT,
        input_json TEXT,
        content TEXT,
        is_error INTEGER CHECK(is_error IN(0, 1)),
        media_id INTEGER,
        mime TEXT,
        PRIMARY KEY(session_id, message_sequence, sequence),
        FOREIGN KEY(session_id, message_sequence) REFERENCES messages(session_id, sequence) ON DELETE CASCADE,
        FOREIGN KEY(media_id) REFERENCES media(id),
        CHECK((kind IN('text', 'reasoning') AND text IS NOT NULL AND call_id IS NULL AND name IS NULL AND input_json IS NULL AND content IS NULL AND is_error IS NULL AND media_id IS NULL AND mime IS NULL) OR (kind = 'tool_call' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NOT NULL AND name <> '' AND input_json IS NOT NULL AND content IS NULL AND is_error IS NULL AND media_id IS NULL AND mime IS NULL) OR (kind = 'tool_result' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NULL AND input_json IS NULL AND content IS NOT NULL AND is_error IS NOT NULL AND media_id IS NULL AND mime IS NULL) OR (kind = 'media' AND text IS NULL AND call_id IS NULL AND name IS NULL AND input_json IS NULL AND content IS NULL AND is_error IS NULL AND media_id IS NOT NULL AND mime IS NOT NULL AND mime <> ''))
    );
    INSERT INTO message_parts_new (
        session_id, message_sequence, sequence, kind, text, call_id, name, input_json, content, is_error, media_id, mime
    )
    SELECT
        session_id, message_sequence, sequence, kind, text, call_id, name, input_json, content, is_error, NULL, NULL
    FROM message_parts;
    DROP TABLE message_parts;
    CREATE TABLE message_parts (
        session_id INTEGER NOT NULL,
        message_sequence INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence >= 0),
        kind TEXT NOT NULL CHECK(kind IN('text', 'reasoning', 'tool_call', 'tool_result', 'media')),
        text TEXT,
        call_id TEXT,
        name TEXT,
        input_json TEXT,
        content TEXT,
        is_error INTEGER CHECK(is_error IN(0, 1)),
        media_id INTEGER,
        mime TEXT,
        PRIMARY KEY(session_id, message_sequence, sequence),
        FOREIGN KEY(session_id, message_sequence) REFERENCES messages(session_id, sequence) ON DELETE CASCADE,
        FOREIGN KEY(media_id) REFERENCES media(id),
        CHECK((kind IN('text', 'reasoning') AND text IS NOT NULL AND call_id IS NULL AND name IS NULL AND input_json IS NULL AND content IS NULL AND is_error IS NULL AND media_id IS NULL AND mime IS NULL) OR (kind = 'tool_call' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NOT NULL AND name <> '' AND input_json IS NOT NULL AND content IS NULL AND is_error IS NULL AND media_id IS NULL AND mime IS NULL) OR (kind = 'tool_result' AND text IS NULL AND call_id IS NOT NULL AND call_id <> '' AND name IS NULL AND input_json IS NULL AND content IS NOT NULL AND is_error IS NOT NULL AND media_id IS NULL AND mime IS NULL) OR (kind = 'media' AND text IS NULL AND call_id IS NULL AND name IS NULL AND input_json IS NULL AND content IS NULL AND is_error IS NULL AND media_id IS NOT NULL AND mime IS NOT NULL AND mime <> ''))
    );
    INSERT INTO message_parts (
        session_id, message_sequence, sequence, kind, text, call_id, name, input_json, content, is_error, media_id, mime
    )
    SELECT
        session_id, message_sequence, sequence, kind, text, call_id, name, input_json, content, is_error, media_id, mime
    FROM message_parts_new;
    DROP TABLE message_parts_new;
    CREATE INDEX parts_message_order ON message_parts(session_id, message_sequence, sequence);

    ALTER TABLE session_attempts ADD COLUMN retry_media_ids TEXT
        CHECK(retry_media_ids IS NULL OR (json_valid(retry_media_ids) AND json_type(retry_media_ids) = 'array'));

    -- Allow empty retry_prompt for media-only turns (length 0). Application code still rejects
    -- empty prompt when media_ids is also empty. Rebuild without RENAME so sqlite_schema keeps
    -- the unquoted table name the normalized schema validator expects.
    CREATE TABLE session_attempts_media (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence > 0),
        status TEXT NOT NULL CHECK(status IN('running', 'completed', 'cancelled', 'failed', 'provider_error', 'interrupted')),
        failure_kind TEXT CHECK(failure_kind IN('cancelled', 'failed', 'provider_error', 'interrupted')),
        retry_prompt TEXT CHECK(retry_prompt IS NULL OR (length(CAST(retry_prompt AS BLOB)) BETWEEN 0 AND 65536)),
        started_at INTEGER NOT NULL,
        finished_at INTEGER,
        completed_turn_sequence INTEGER, retry_media_ids TEXT
        CHECK(retry_media_ids IS NULL OR (json_valid(retry_media_ids) AND json_type(retry_media_ids) = 'array')),
        FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
        FOREIGN KEY(session_id, completed_turn_sequence) REFERENCES turns(session_id, sequence) ON DELETE SET NULL,
        CHECK((status = 'running' AND failure_kind IS NULL AND retry_prompt IS NOT NULL AND finished_at IS NULL AND completed_turn_sequence IS NULL) OR
              (status = 'completed' AND failure_kind IS NULL AND retry_prompt IS NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NOT NULL) OR
              (status = 'cancelled' AND failure_kind = 'cancelled' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
              (status = 'failed' AND failure_kind = 'failed' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
              (status = 'provider_error' AND failure_kind = 'provider_error' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
              (status = 'interrupted' AND failure_kind = 'interrupted' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL))
    );
    INSERT INTO session_attempts_media (
        id, session_id, sequence, status, failure_kind, retry_prompt, started_at, finished_at,
        completed_turn_sequence, retry_media_ids
    )
    SELECT
        id, session_id, sequence, status, failure_kind, retry_prompt, started_at, finished_at,
        completed_turn_sequence, retry_media_ids
    FROM session_attempts;
    DROP TABLE session_attempts;
    CREATE TABLE session_attempts (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL,
        sequence INTEGER NOT NULL CHECK(sequence > 0),
        status TEXT NOT NULL CHECK(status IN('running', 'completed', 'cancelled', 'failed', 'provider_error', 'interrupted')),
        failure_kind TEXT CHECK(failure_kind IN('cancelled', 'failed', 'provider_error', 'interrupted')),
        retry_prompt TEXT CHECK(retry_prompt IS NULL OR (length(CAST(retry_prompt AS BLOB)) BETWEEN 0 AND 65536)),
        started_at INTEGER NOT NULL,
        finished_at INTEGER,
        completed_turn_sequence INTEGER, retry_media_ids TEXT
        CHECK(retry_media_ids IS NULL OR (json_valid(retry_media_ids) AND json_type(retry_media_ids) = 'array')),
        FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
        FOREIGN KEY(session_id, completed_turn_sequence) REFERENCES turns(session_id, sequence) ON DELETE SET NULL,
        CHECK((status = 'running' AND failure_kind IS NULL AND retry_prompt IS NOT NULL AND finished_at IS NULL AND completed_turn_sequence IS NULL) OR
              (status = 'completed' AND failure_kind IS NULL AND retry_prompt IS NULL AND finished_at IS NOT NULL AND completed_turn_sequence IS NOT NULL) OR
              (status = 'cancelled' AND failure_kind = 'cancelled' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
              (status = 'failed' AND failure_kind = 'failed' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
              (status = 'provider_error' AND failure_kind = 'provider_error' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL) OR
              (status = 'interrupted' AND failure_kind = 'interrupted' AND finished_at IS NOT NULL AND completed_turn_sequence IS NULL))
    );
    INSERT INTO session_attempts (
        id, session_id, sequence, status, failure_kind, retry_prompt, started_at, finished_at,
        completed_turn_sequence, retry_media_ids
    )
    SELECT
        id, session_id, sequence, status, failure_kind, retry_prompt, started_at, finished_at,
        completed_turn_sequence, retry_media_ids
    FROM session_attempts_media;
    DROP TABLE session_attempts_media;
    CREATE UNIQUE INDEX session_attempts_session_sequence ON session_attempts(session_id, sequence);
    CREATE UNIQUE INDEX session_attempts_one_running ON session_attempts(session_id) WHERE status = 'running';
    CREATE INDEX session_attempts_latest ON session_attempts(session_id, sequence DESC, id DESC);
    "
    .to_owned()
}

/// Staged attachments for prompt history and stash rows.
///
/// `attachments` is JSON text of `[media_id, mime]` pairs (durable ids only, never source paths),
/// `NULL` for text-only entries — every pre-existing row copies through as `NULL`. The old
/// `text <> ''` CHECK is relaxed so an attachments-only entry (empty text) can be stashed;
/// application code still rejects an entry that is empty on both sides. The rebuild uses the same
/// double shuffle as `media_ddl` so `sqlite_schema` keeps unquoted table names.
fn prompt_memory_media_ddl() -> String {
    "
    CREATE TABLE prompt_history_media (
        id INTEGER PRIMARY KEY,
        text TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        attachments TEXT CHECK(attachments IS NULL OR (json_valid(attachments) AND json_type(attachments) = 'array')),
        CHECK(text <> '' OR attachments IS NOT NULL)
    );
    INSERT INTO prompt_history_media (id, text, created_at, attachments)
    SELECT id, text, created_at, NULL FROM prompt_history;
    DROP TABLE prompt_history;
    CREATE TABLE prompt_history (
        id INTEGER PRIMARY KEY,
        text TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        attachments TEXT CHECK(attachments IS NULL OR (json_valid(attachments) AND json_type(attachments) = 'array')),
        CHECK(text <> '' OR attachments IS NOT NULL)
    );
    INSERT INTO prompt_history (id, text, created_at, attachments)
    SELECT id, text, created_at, attachments FROM prompt_history_media;
    DROP TABLE prompt_history_media;
    CREATE INDEX prompt_history_id ON prompt_history(id);

    CREATE TABLE prompt_stash_media (
        id INTEGER PRIMARY KEY,
        text TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        attachments TEXT CHECK(attachments IS NULL OR (json_valid(attachments) AND json_type(attachments) = 'array')),
        CHECK(text <> '' OR attachments IS NOT NULL)
    );
    INSERT INTO prompt_stash_media (id, text, created_at, attachments)
    SELECT id, text, created_at, NULL FROM prompt_stash;
    DROP TABLE prompt_stash;
    CREATE TABLE prompt_stash (
        id INTEGER PRIMARY KEY,
        text TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        attachments TEXT CHECK(attachments IS NULL OR (json_valid(attachments) AND json_type(attachments) = 'array')),
        CHECK(text <> '' OR attachments IS NOT NULL)
    );
    INSERT INTO prompt_stash (id, text, created_at, attachments)
    SELECT id, text, created_at, attachments FROM prompt_stash_media;
    DROP TABLE prompt_stash_media;
    CREATE INDEX prompt_stash_id ON prompt_stash(id);
    "
    .to_owned()
}

/// The lineage a forked session carries: the session its history was copied from, and the
/// message-sequence cut point it was copied up to. Both are NULL on a session that was started
/// rather than forked, and the pair is written together so a fork can never lose its origin or
/// its cut point.
///
/// The column carries no `REFERENCES sessions(id)` clause on purpose. A real foreign key would
/// force one of three answers to "what happens to a fork when its parent is deleted", and all
/// three change behavior this migration has no mandate to change: `NO ACTION` makes deleting a
/// forked-from session fail, `CASCADE` silently deletes the forks with it, and `SET NULL` clears
/// the parent while leaving `fork_message_count` behind, breaking the both-or-neither invariant
/// the read path validates. A dangling parent id simply reads as a fork whose parent is gone.
///
/// `sessions_list` is `(resumable, updated_at DESC, id DESC)`, which no lineage query can use;
/// the forest index makes reading a session's children an index lookup instead of a table scan.
fn session_fork_lineage_ddl() -> String {
    "
    ALTER TABLE sessions ADD COLUMN parent_session_id INTEGER;
    ALTER TABLE sessions ADD COLUMN fork_message_count INTEGER;
    CREATE INDEX sessions_forest ON sessions(parent_session_id, id);
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

    /// A rebuild that drops rows must abort instead of committing the loss: the ledger stays
    /// clean and the table the user already had is still there, untouched, on the next open.
    #[test]
    fn a_rebuild_that_loses_rows_fails_the_migration_and_rolls_back() {
        let directory = data_directory();
        let path = directory.join("guard.db");
        let mut connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE parked (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
                 INSERT INTO parked (text) VALUES ('kept'), ('lost');",
            )
            .unwrap();

        let lossy = [Migration {
            id: "9999_lossy_rebuild",
            ddl: || {
                "
                CREATE TABLE parked_rebuilt (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
                INSERT INTO parked_rebuilt (id, text) SELECT id, text FROM parked WHERE id = 1;
                DROP TABLE parked;
                ALTER TABLE parked_rebuilt RENAME TO parked;
                "
                .to_owned()
            },
            preserved_tables: &["parked"],
        }];

        let error = apply_migrations(&mut connection, &path, &lossy).expect_err("must refuse");
        assert!(
            error.detail().contains("parked kept 1 of 2 rows"),
            "the failure must name what was lost: {}",
            error.detail()
        );

        let rows: i64 = connection
            .query_row("SELECT count(*) FROM parked", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2, "the original table must survive the rollback");
        assert!(
            read_applied_migration_ids(&connection, &path)
                .unwrap()
                .is_empty()
        );

        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }

    /// A connection in the shape [`open_unified_database`] leaves behind — foreign keys on —
    /// holding a database migrated up to, but not including, `pending`.
    fn database_before(directory: &Path, pending: &[Migration]) -> (PathBuf, Connection) {
        let path = directory.join("upgrade.db");
        let mut connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();

        let applied = MIGRATIONS
            .iter()
            .position(|migration| migration.id == pending[0].id)
            .expect("the pending migrations must belong to MIGRATIONS");
        apply_migrations(&mut connection, &path, &MIGRATIONS[..applied]).unwrap();

        (path, connection)
    }

    fn populate_session_with_a_fact(connection: &Connection) {
        connection
            .execute_batch(
                "INSERT INTO sessions (id, project, title, active_agent, created_at, updated_at,
                                       completed_turn_count, resumable)
                     VALUES (1, '/project', 'titled', 'primary', 1, 1, 1, 1);
                 INSERT INTO turns (session_id, sequence, completed_at) VALUES (1, 1, 1);
                 INSERT INTO messages (session_id, sequence, turn_sequence, role)
                     VALUES (1, 1, 1, 'user');
                 INSERT INTO message_parts (session_id, message_sequence, sequence, kind, text)
                     VALUES (1, 1, 0, 'text', 'hello');
                 INSERT INTO session_attempts (id, session_id, sequence, status, retry_prompt,
                                               started_at)
                     VALUES (1, 1, 1, 'running', 'retry me', 1);
                 INSERT INTO tool_result_facts (id, session_id, attempt_id, sequence, tool_call_id,
                                                tool, outcome, path_status, recorded_at)
                     VALUES (1, 1, 1, 1, 'call-1', 'read', 'succeeded', 'not_applicable', 1);",
            )
            .unwrap();
    }

    /// Suspending enforcement for the rebuild must not let a migration commit a dangling child:
    /// what the pragma stops policing before the DDL, `PRAGMA foreign_key_check` polices after it,
    /// and enforcement is restored even though the migration failed.
    #[test]
    fn a_migration_that_orphans_a_child_row_fails_the_foreign_key_check_and_rolls_back() {
        let directory = data_directory();
        let path = directory.join("orphan.db");
        let mut connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        connection
            .execute_batch(
                "CREATE TABLE parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE child (
                     id INTEGER PRIMARY KEY,
                     parent_id INTEGER NOT NULL,
                     FOREIGN KEY(parent_id) REFERENCES parent(id) ON DELETE CASCADE
                 );
                 INSERT INTO parent (id) VALUES (1);
                 INSERT INTO child (id, parent_id) VALUES (1, 1);",
            )
            .unwrap();

        let orphaning = [Migration {
            id: "9999_orphaning_rebuild",
            ddl: || {
                "DROP TABLE parent;
                 CREATE TABLE parent (id INTEGER PRIMARY KEY);"
                    .to_owned()
            },
            preserved_tables: &[],
        }];

        let error = apply_migrations(&mut connection, &path, &orphaning).expect_err("must refuse");
        assert!(
            error.detail().contains("1 foreign key violation"),
            "the failure must name what the check found: {}",
            error.detail()
        );

        assert_eq!(foreign_keys(&connection), 1);
        let parents: i64 = connection
            .query_row("SELECT count(*) FROM parent", [], |row| row.get(0))
            .unwrap();
        let children: i64 = connection
            .query_row("SELECT count(*) FROM child", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            (parents, children),
            (1, 1),
            "the rollback must restore both"
        );
        assert!(
            read_applied_migration_ids(&connection, &path)
                .unwrap()
                .is_empty()
        );

        drop(connection);
        fs::remove_dir_all(directory).unwrap();
    }

    /// `DROP TABLE` performs an implicit `DELETE FROM` with foreign keys on, so dropping the
    /// `session_attempts` original mid-rebuild cascades into `tool_result_facts`. The evidence
    /// ledger belongs to attempts that the rebuild puts straight back, so it must survive.
    #[test]
    fn migration_0009_keeps_the_facts_cascade_reachable_from_the_attempts_it_rebuilds() {
        let directory = data_directory();
        let media_migration = &MIGRATIONS[8..9];
        assert_eq!(media_migration[0].id, "0009_media");
        let (path, mut connection) = database_before(&directory, media_migration);
        populate_session_with_a_fact(&connection);

        apply_migrations(&mut connection, &path, media_migration).unwrap();

        let facts: i64 = connection
            .query_row("SELECT count(*) FROM tool_result_facts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            facts, 1,
            "the session_attempts rebuild must not cascade the evidence ledger away"
        );
        let attempts: i64 = connection
            .query_row("SELECT count(*) FROM session_attempts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(attempts, 1);

        drop(connection);
        fs::remove_dir_all(directory).unwrap();
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
                    "retry_media_ids",
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
                "parent_session_id",
                "fork_message_count",
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
                    "retry_media_ids",
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

        // `sessions` keeps every pre-existing column, unchanged and in the same order. This test
        // opens a freshly created database, so every later migration has already applied too — it
        // asserts bypass_permission_prompts's own position, not the full column set.
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
                "parent_session_id",
                "fork_message_count",
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

    /// The rebuild carries every queued directive across and leaves each one
    /// addressed exactly as it was: a message queued for a session before this
    /// migration must not come back looking like it was meant for a child.
    #[test]
    fn migration_0014_keeps_every_queued_directive_addressed_to_its_session() {
        let directory = data_directory();
        let (path, mut connection) = open_unified_database(&directory).unwrap();

        connection
            .execute_batch(
                "INSERT INTO directives (session_id, source, grain, text, created_at)
                 VALUES (3, 'supervisor', 'tool_call', 'queued before the rebuild', '1');",
            )
            .unwrap();

        // Re-running the ledgered migration is a no-op, so the rebuild is
        // replayed against the row above by forgetting it was ever applied.
        connection
            .execute(
                "DELETE FROM schema_migrations WHERE id = '0014_directive_child_target'",
                [],
            )
            .unwrap();
        run_pending_migrations(&mut connection, &path).unwrap();

        let (session_id, child, text): (Option<i64>, Option<String>, String) = connection
            .query_row(
                "SELECT session_id, child, text FROM directives",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(session_id, Some(3));
        assert_eq!(child, None);
        assert_eq!(text, "queued before the rebuild");

        // Exactly one addressee, enforced by the schema rather than by the
        // callers: a row naming both or neither is unroutable.
        for values in [
            "(1, 'a1b2c3d4', 'human', 'tool_call', 'both', '1')",
            "(NULL, NULL, 'human', 'tool_call', 'neither', '1')",
        ] {
            connection
                .execute_batch(&format!(
                    "INSERT INTO directives
                         (session_id, child, source, grain, text, created_at)
                     VALUES {values};"
                ))
                .expect_err("a directive names exactly one addressee");
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migration_0011_is_purely_additive_and_adds_a_nullable_fork_lineage_pair() {
        let directory = data_directory();
        let (_, connection) = open_unified_database(&directory).unwrap();

        for column in ["parent_session_id", "fork_message_count"] {
            let (declared_type, not_null, default_value): (String, i64, Option<String>) =
                connection
                    .query_row(
                        "SELECT type, \"notnull\", dflt_value FROM pragma_table_info('sessions')
                     WHERE name = ?1",
                        params![column],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .unwrap();
            assert_eq!(declared_type, "INTEGER");
            assert_eq!(not_null, 0, "{column} must be nullable");
            assert_eq!(default_value, None);
        }

        let forest_index: String = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'index' AND name = 'sessions_forest'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(forest_index.contains("sessions(parent_session_id, id)"));

        // A session that was started rather than forked carries neither half of the lineage, so
        // no pre-existing row needs a backfill.
        connection
            .execute_batch(
                "INSERT INTO sessions (project, title, active_agent, created_at, updated_at)
                 VALUES ('project', 'title', 'primary', 1, 1);",
            )
            .unwrap();
        let lineage: (Option<i64>, Option<i64>) = connection
            .query_row(
                "SELECT parent_session_id, fork_message_count FROM sessions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(lineage, (None, None));

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

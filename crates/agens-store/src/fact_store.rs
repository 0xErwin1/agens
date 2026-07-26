//! The evidence ledger: a durable record of the facts a running turn's tools
//! reported, keyed by the attempt that produced them rather than by turn.
//!
//! This store is distinct from the control plane's own event journal: it is
//! written by the harness at tool-execution time, an INPUT a coordinator
//! reads rather than a coordinator's own OUTPUT, and it computes no
//! aggregate, status, or health signal of its own — every column holds a
//! value the executing tool already produced.

use std::{fmt, path::Path, path::PathBuf};

use agens_core::{ToolOutcome, ToolResultFacts};
use rusqlite::{Connection, params};

use crate::database;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolFactStoreError {
    message: String,
}

impl ToolFactStoreError {
    fn operation(operation: &str, path: &Path, error: impl fmt::Display) -> Self {
        Self {
            message: format!(
                "tool result facts {operation} at {}: {error}",
                path.display()
            ),
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

impl fmt::Display for ToolFactStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToolFactStoreError {}

/// The evidence ledger's own connection to the shared `agens.db` file.
///
/// It owns a `Connection` distinct from `SessionStore`'s, because the attempt
/// lifecycle holds `&mut SessionStore` for the whole runtime closure and a
/// ledger sink installed alongside it cannot also borrow that connection.
/// WAL serializes writers within the process, per the same discipline every
/// other unified-database store already follows.
pub struct ToolFactStore {
    database_path: PathBuf,
    connection: Connection,
}

impl ToolFactStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, ToolFactStoreError> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(ToolFactStoreError::from_database)?;

        Ok(Self {
            database_path,
            connection,
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.database_path.clone()
    }

    /// Records one fact as a row keyed by `(attempt_id, sequence)`.
    ///
    /// `session_id` and `attempt_id` MUST be concrete: a fact whose identity
    /// carries no session or attempt (a subagent child turn) is not
    /// ledger-writable and MUST NOT be passed here — that filtering is the
    /// caller's decision, not this store's, since the store has no way to
    /// tell "no attempt" from "attempt not yet supplied".
    pub fn record(
        &mut self,
        session_id: i64,
        attempt_id: i64,
        sequence: u64,
        tool_call_id: &str,
        facts: &ToolResultFacts,
    ) -> Result<(), ToolFactStoreError> {
        let row = FactRow::from_facts(facts)?;

        self.connection
            .execute(
                "INSERT INTO tool_result_facts (
                     session_id, attempt_id, sequence, tool_call_id, tool, outcome,
                     path, path_status, exit_code, is_new_file, bytes_written,
                     lines_written, lines_added, lines_removed, match_count, truncated,
                     recorded_at
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     CAST(strftime('%s','now') AS INTEGER)
                 )",
                params![
                    session_id,
                    attempt_id,
                    i64::try_from(sequence)
                        .map_err(|_| ToolFactStoreError::detail("sequence exceeds i64 range"))?,
                    tool_call_id,
                    row.tool,
                    row.outcome,
                    row.path,
                    row.path_status,
                    row.exit_code,
                    row.is_new_file,
                    row.bytes_written,
                    row.lines_written,
                    row.lines_added,
                    row.lines_removed,
                    row.match_count,
                    row.truncated,
                ],
            )
            .map_err(|error| {
                ToolFactStoreError::operation("record fact", &self.database_path, error)
            })?;

        Ok(())
    }
}

/// The typed-column decomposition of one `ToolResultFacts` value, computing
/// nothing beyond that mapping: every field here traces to a value the
/// variant already carried.
struct FactRow {
    tool: &'static str,
    outcome: &'static str,
    path: Option<String>,
    path_status: &'static str,
    exit_code: Option<i32>,
    is_new_file: Option<bool>,
    bytes_written: Option<i64>,
    lines_written: Option<i64>,
    lines_added: Option<i64>,
    lines_removed: Option<i64>,
    match_count: Option<i64>,
    truncated: Option<bool>,
}

impl FactRow {
    fn from_facts(facts: &ToolResultFacts) -> Result<Self, ToolFactStoreError> {
        let empty = Self::empty();

        match facts {
            ToolResultFacts::Write {
                path,
                outcome,
                written,
            } => {
                let (path, path_status) = path_columns(path);
                Ok(Self {
                    tool: "write",
                    outcome: outcome_label(*outcome)?,
                    path,
                    path_status,
                    is_new_file: written.map(|magnitude| magnitude.is_new_file),
                    bytes_written: written.map(|magnitude| magnitude.bytes_written as i64),
                    lines_written: written.map(|magnitude| magnitude.lines_written as i64),
                    ..empty
                })
            }
            ToolResultFacts::Edit {
                path,
                outcome,
                changed,
            } => {
                let (path, path_status) = path_columns(path);
                Ok(Self {
                    tool: "edit",
                    outcome: outcome_label(*outcome)?,
                    path,
                    path_status,
                    lines_added: changed.map(|magnitude| magnitude.lines_added as i64),
                    lines_removed: changed.map(|magnitude| magnitude.lines_removed as i64),
                    ..empty
                })
            }
            ToolResultFacts::Bash { outcome, exit_code } => Ok(Self {
                tool: "bash",
                outcome: outcome_label(*outcome)?,
                path_status: "not_applicable",
                exit_code: *exit_code,
                ..empty
            }),
            ToolResultFacts::Read { path, outcome } => {
                let (path, path_status) = path_columns(path);
                Ok(Self {
                    tool: "read",
                    outcome: outcome_label(*outcome)?,
                    path,
                    path_status,
                    ..empty
                })
            }
            ToolResultFacts::Search {
                outcome,
                match_count,
                truncated,
            } => Ok(Self {
                tool: "search",
                outcome: outcome_label(*outcome)?,
                path_status: "not_applicable",
                match_count: Some(*match_count as i64),
                truncated: Some(*truncated),
                ..empty
            }),
            _ => Err(ToolFactStoreError::detail(
                "unsupported tool result facts variant",
            )),
        }
    }

    const fn empty() -> Self {
        Self {
            tool: "",
            outcome: "",
            path: None,
            path_status: "not_applicable",
            exit_code: None,
            is_new_file: None,
            bytes_written: None,
            lines_written: None,
            lines_added: None,
            lines_removed: None,
            match_count: None,
            truncated: None,
        }
    }
}

fn path_columns(path: &agens_core::FactPath) -> (Option<String>, &'static str) {
    match path.relative() {
        Some(value) => (Some(value.to_owned()), "relative"),
        None => (None, "unrepresentable"),
    }
}

fn outcome_label(outcome: ToolOutcome) -> Result<&'static str, ToolFactStoreError> {
    match outcome {
        ToolOutcome::Succeeded => Ok("succeeded"),
        ToolOutcome::Failed => Ok("failed"),
        ToolOutcome::Denied => Ok("denied"),
        _ => Err(ToolFactStoreError::detail("unsupported tool outcome")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agens_core::{FactPath, WriteMagnitude};

    use super::*;

    fn data_directory() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "agens-store-fact-store-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn recorded_row(
        connection: &Connection,
        attempt_id: i64,
        sequence: u64,
    ) -> (String, String, Option<String>, String) {
        connection
            .query_row(
                "SELECT tool, outcome, path, path_status
                 FROM tool_result_facts WHERE attempt_id = ?1 AND sequence = ?2",
                params![attempt_id, sequence as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .unwrap()
    }

    fn seed_session_and_attempt(connection: &Connection) -> (i64, i64) {
        connection
            .execute(
                "INSERT INTO sessions (id, project, title, active_agent, created_at, updated_at)
                 VALUES (1, 'project', 'title', 'build', 0, 0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session_attempts (id, session_id, sequence, status, retry_prompt, started_at)
                 VALUES (1, 1, 1, 'running', 'retry', 0)",
                [],
            )
            .unwrap();
        (1, 1)
    }

    #[test]
    fn a_write_fact_becomes_a_durable_row_keyed_by_attempt_and_sequence() {
        let directory = data_directory();
        let mut store = ToolFactStore::open(&directory).unwrap();
        let verification_connection = Connection::open(store.database_path()).unwrap();
        let (session_id, attempt_id) = seed_session_and_attempt(&verification_connection);

        store
            .record(
                session_id,
                attempt_id,
                1,
                "call-1",
                &ToolResultFacts::Write {
                    path: FactPath::new("notes.txt"),
                    outcome: ToolOutcome::Succeeded,
                    written: Some(WriteMagnitude {
                        is_new_file: true,
                        bytes_written: 12,
                        lines_written: 1,
                    }),
                },
            )
            .unwrap();

        let (tool, outcome, path, path_status) =
            recorded_row(&verification_connection, attempt_id, 1);
        assert_eq!(tool, "write");
        assert_eq!(outcome, "succeeded");
        assert_eq!(path.as_deref(), Some("notes.txt"));
        assert_eq!(path_status, "relative");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn an_unrepresentable_path_is_recorded_distinctly_from_a_pathless_variant() {
        let directory = data_directory();
        let mut store = ToolFactStore::open(&directory).unwrap();
        let verification_connection = Connection::open(store.database_path()).unwrap();
        let (session_id, attempt_id) = seed_session_and_attempt(&verification_connection);

        store
            .record(
                session_id,
                attempt_id,
                1,
                "call-1",
                &ToolResultFacts::Edit {
                    path: FactPath::new("../outside.txt"),
                    outcome: ToolOutcome::Failed,
                    changed: None,
                },
            )
            .unwrap();
        store
            .record(
                session_id,
                attempt_id,
                2,
                "call-2",
                &ToolResultFacts::Bash {
                    outcome: ToolOutcome::Succeeded,
                    exit_code: Some(0),
                },
            )
            .unwrap();

        let (_, _, edit_path, edit_path_status) =
            recorded_row(&verification_connection, attempt_id, 1);
        let (_, _, bash_path, bash_path_status) =
            recorded_row(&verification_connection, attempt_id, 2);

        assert_eq!(edit_path, None);
        assert_eq!(edit_path_status, "unrepresentable");
        assert_eq!(bash_path, None);
        assert_eq!(bash_path_status, "not_applicable");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_ledger_write_computes_no_aggregate_or_status_column() {
        let directory = data_directory();
        let mut store = ToolFactStore::open(&directory).unwrap();
        let verification_connection = Connection::open(store.database_path()).unwrap();
        let (session_id, attempt_id) = seed_session_and_attempt(&verification_connection);

        store
            .record(
                session_id,
                attempt_id,
                1,
                "call-1",
                &ToolResultFacts::Search {
                    outcome: ToolOutcome::Succeeded,
                    match_count: 3,
                    truncated: false,
                },
            )
            .unwrap();

        let mut statement = verification_connection
            .prepare("SELECT name FROM pragma_table_info('tool_result_facts')")
            .unwrap();
        let columns: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();

        for forbidden in ["run_health", "status", "score", "verdict", "health"] {
            assert!(
                !columns.iter().any(|column| column == forbidden),
                "no ledger column may hold an interpreted value, found one matching {forbidden}"
            );
        }

        std::fs::remove_dir_all(directory).unwrap();
    }
}

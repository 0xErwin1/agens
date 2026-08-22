//! The control plane's tables: runs, attempts, events, questions, findings,
//! providers and run health, plus the safe-point queue that already lives in
//! [`crate::directives`].
//!
//! One actor writes all of it. The state machines, the scheduler and the timer
//! wheel are only as simple as they are because there is no second writer to
//! reconcile against, so this store hands out one connection and no
//! cross-process coordination.
//!
//! Two boundaries the types here keep, both documented on the DDL that creates
//! the tables:
//!
//! - A run's `repo_id` is repository identity and is never joined against
//!   session identity (`sessions.project`, `permission_grants.project`), which
//!   is per worktree and narrow on purpose.
//! - `events` is this store's own journal; the harness's evidence ledger
//!   (`tool_result_facts`) is a separate table it reads as an input. An attempt
//!   reaches that ledger through [`AttemptRow::session_attempt_id`], which is
//!   why the column exists from the first migration rather than being added to
//!   a populated table later.
//!
//! Timestamps are epoch seconds and always come from the caller. The store
//! reads no clock, so a coordinator that is reconciling after a restart decides
//! what "now" means rather than inheriting this process's idea of it.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, Row, params};

use crate::database;

/// Declares an enum that is stored as one of a fixed set of SQL text values.
///
/// The mapping is written once per variant instead of twice, and the `parse`
/// arm is generated from the same list the `as_str` arm uses, so a value that
/// can be written can always be read back.
macro_rules! sql_enum {
    (
        $(#[$enum_meta:meta])*
        $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident => $text:literal),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name {
            $($(#[$variant_meta])* $variant,)+
        }

        impl $name {
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)+
                }
            }

            #[must_use]
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

sql_enum! {
    /// Where a run sits in its lifecycle.
    ///
    /// There is no transition back to `Draft`: an approved scope is never
    /// edited, because an unmoving scope is what makes divergence measurable.
    /// Replanning opens a new run that inherits worktree and lineage instead.
    RunState {
        Draft => "draft",
        Queued => "queued",
        Running => "running",
        AwaitingInput => "awaiting_input",
        AwaitingQuota => "awaiting_quota",
        Done => "done",
        Failed => "failed",
        Interrupted => "interrupted",
        Cancelled => "cancelled",
    }
}

sql_enum! {
    /// A worktree's disposition. `Reclaimable` is only ever set by re-deriving
    /// merge state from git, never by trusting a stored flag.
    WorktreeStatus {
        Active => "active",
        Reclaimable => "reclaimable",
        Cleaned => "cleaned",
    }
}

sql_enum! {
    /// How one attempt ended.
    ///
    /// `ChangesRequested` and `Interrupted` are separate from `Failed` because
    /// collapsing them charges the agent for a human review and for
    /// infrastructure waits. Nothing can reconstruct that attribution after the
    /// fact, so the distinction is recorded while the table is still empty.
    AttemptOutcome {
        Succeeded => "succeeded",
        Failed => "failed",
        ChangesRequested => "changes_requested",
        Interrupted => "interrupted",
    }
}

sql_enum! {
    /// Who asked for the retry this attempt belongs to.
    RetryTrigger {
        User => "user",
        Praetor => "praetor",
        Coordinator => "coordinator",
    }
}

sql_enum! {
    /// Whether an event describes agent behavior or the machinery around it.
    EventClass {
        Agent => "agent",
        Infra => "infra",
    }
}

sql_enum! {
    /// A plain question, or an authorization bound to specific bytes.
    QuestionKind {
        Question => "question",
        Approval => "approval",
    }
}

sql_enum! {
    /// Who answered. An approval can only be authorized by the user.
    QuestionAuthor {
        Praetor => "praetor",
        User => "user",
    }
}

sql_enum! {
    /// `Expired` is terminal and carries no answer: silence never authorizes,
    /// so an approval that ran out has to be distinguishable from one still
    /// waiting.
    QuestionState {
        Open => "open",
        Answered => "answered",
        Delivered => "delivered",
        Expired => "expired",
    }
}

sql_enum! {
    /// How well a finding's claim is backed. Only `Deterministic` credits
    /// progress; the other two are recorded without crediting it.
    EvidenceClass {
        Deterministic => "deterministic",
        Inferential => "inferential",
        Insufficient => "insufficient",
    }
}

sql_enum! {
    /// Whether the candidate caused the finding, or it was already there.
    CausalDisposition {
        CandidateCaused => "candidate_caused",
        PreExisting => "pre_existing",
        Unknown => "unknown",
    }
}

sql_enum! {
    /// Whether a provider will currently accept work.
    QuotaState {
        Ok => "ok",
        Capped => "capped",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlPlaneError {
    message: String,
}

impl ControlPlaneError {
    fn operation(operation: &str, path: &Path, error: impl fmt::Display) -> Self {
        Self {
            message: format!("control plane {operation} at {}: {error}", path.display()),
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

impl fmt::Display for ControlPlaneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ControlPlaneError {}

type Result<T> = std::result::Result<T, ControlPlaneError>;

/// One approved execution.
///
/// `id` is `None` until the row is written and carries the assigned rowid on
/// every row read back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRow {
    pub id: Option<i64>,
    /// Repository fingerprint. Persisted, never recomputed when reading
    /// history, and never joined against session identity.
    pub repo_id: String,
    /// Kept next to the fingerprint so a changed origin is diagnosable and the
    /// orphaned rows are recoverable by hand.
    pub repo_root: String,
    pub remote_url: Option<String>,
    /// Provenance of an imported task, not a foreign key: after the import the
    /// run never reads the source system again, and stays valid if the task is
    /// deleted or renumbered.
    pub external_ref: Option<String>,
    pub parent_run_id: Option<i64>,
    pub task: String,
    /// Frozen at approval, together with `dod`. The divergence detector
    /// measures the worker against this text, so a scope that moved under the
    /// worker is exactly the failure it exists to catch.
    pub scope: String,
    pub dod: String,
    /// JSON array, `None` until the first checkpoint that carries a diff
    /// freezes it from the paths actually touched. Planning only ever supplies
    /// tentative paths, which is why this is not filled in at approval.
    pub genesis_paths: Option<String>,
    pub state: RunState,
    pub priority: i64,
    pub dep_run_id: Option<i64>,
    pub provider: String,
    pub budget_tokens: Option<i64>,
    pub worktree_path: Option<String>,
    pub worktree_status: Option<WorktreeStatus>,
    pub created_at: i64,
    pub result: Option<String>,
}

/// One try at a run: a row with its own cost and duration, not a counter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptRow {
    pub id: Option<i64>,
    pub run_id: i64,
    /// Attempt number within the run, starting at 1.
    pub n: i64,
    pub session_id: Option<i64>,
    /// The physical execution this logical attempt ran as. It is also the join
    /// to the harness's evidence ledger, which is keyed by that same attempt;
    /// without it no reported fact can be attributed to a run.
    ///
    /// Nullable, and cleared rather than cascaded when the physical row goes:
    /// the control plane's accounting for an attempt outlives the session
    /// history it was executed in.
    pub session_attempt_id: Option<i64>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub outcome: Option<AttemptOutcome>,
    pub retry_trigger: Option<RetryTrigger>,
    pub tokens: Option<i64>,
    /// Cost in millionths of a currency unit. An integer because money in a
    /// float accumulates error over a table meant to be summed.
    pub cost_micros: Option<i64>,
}

/// One entry in the coordinator's append-only journal.
///
/// `id` doubles as the monotonic sequence: it is assigned in commit order by a
/// single writer, which is what a separate sequence column would have to
/// reproduce. Ordering and "everything before this point" both read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventRow {
    pub id: Option<i64>,
    /// `None` for a fact that belongs to no run, such as a provider's quota
    /// resetting.
    pub run_id: Option<i64>,
    pub event_type: String,
    pub class: EventClass,
    /// JSON object or value; the journal does not interpret it.
    pub payload: String,
    pub ts: i64,
}

/// A blocked decision waiting on a person or on Praetor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionRow {
    pub id: Option<i64>,
    pub run_id: i64,
    pub kind: QuestionKind,
    pub blocked_decision: String,
    /// JSON array of the options offered.
    pub options: String,
    pub recommendation: Option<String>,
    pub answer: Option<String>,
    pub author: Option<QuestionAuthor>,
    pub expires_at: Option<i64>,
    /// The receipt half of an approval: `HEAD^{tree}` frozen when the approval
    /// was created. Required for an approval and forbidden otherwise, because
    /// the pre-merge gate re-derives it and compares — a missing receipt would
    /// pass a comparison it was supposed to fail. The user approves bytes, not
    /// a run.
    pub tree_hash: Option<String>,
    /// The other half: a digest of the paths the worktree touched, frozen with
    /// the tree hash.
    pub paths_digest: Option<String>,
    pub state: QuestionState,
    pub created_at: i64,
}

/// A claim about the work, with the evidence behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindingRow {
    pub id: Option<i64>,
    pub run_id: i64,
    /// The journal entry for the checkpoint this came from, when it came from
    /// one. Checkpoints are events; there is no separate table for them.
    pub checkpoint_id: Option<i64>,
    pub description: String,
    pub evidence_class: EvidenceClass,
    /// JSON array of references to whatever backs the claim.
    pub proof_refs: String,
    pub causal_disposition: CausalDisposition,
    pub created_at: i64,
}

/// Whether a provider is currently serving, and when it will again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRow {
    pub provider: String,
    pub quota_state: QuotaState,
    /// When the cap lifts. `None` while capped means the provider named no
    /// reset, so nothing can wake the parked runs on a timer.
    pub reset_at: Option<i64>,
    pub updated_at: i64,
}

/// Passive health signals for one run. Derived, and recomputable from the
/// journal and the evidence ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunHealthRow {
    pub run_id: i64,
    pub last_progress_turn: Option<i64>,
    pub noop_turns: i64,
    pub failing_test_signature: Option<String>,
    pub tokens_since_progress: i64,
    pub updated_at: i64,
}

/// The control plane's own connection to the shared `agens.db` file.
pub struct ControlPlaneStore {
    database_path: PathBuf,
    connection: Connection,
}

impl ControlPlaneStore {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self> {
        let (database_path, connection) = database::open_unified_database(data_directory.as_ref())
            .map_err(ControlPlaneError::from_database)?;

        Ok(Self {
            database_path,
            connection,
        })
    }

    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.database_path.clone()
    }

    pub fn insert_run(&mut self, run: &RunRow) -> Result<i64> {
        self.insert(
            "insert run",
            "INSERT INTO runs (
                 repo_id, repo_root, remote_url, external_ref, parent_run_id, task, scope, dod,
                 genesis_paths, state, priority, dep_run_id, provider, budget_tokens,
                 worktree_path, worktree_status, created_at, result
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18
             )",
            params![
                run.repo_id,
                run.repo_root,
                run.remote_url,
                run.external_ref,
                run.parent_run_id,
                run.task,
                run.scope,
                run.dod,
                run.genesis_paths,
                run.state.as_str(),
                run.priority,
                run.dep_run_id,
                run.provider,
                run.budget_tokens,
                run.worktree_path,
                run.worktree_status.map(WorktreeStatus::as_str),
                run.created_at,
                run.result,
            ],
        )
    }

    pub fn load_run(&self, id: i64) -> Result<Option<RunRow>> {
        self.load_optional(
            "load run",
            &format!("{RUN_SELECT} WHERE id = ?1"),
            params![id],
        )
    }

    /// Every run of one repository, oldest first.
    ///
    /// Grouping is by fingerprint, never by session identity: all the worktrees
    /// of a repository share it, and a session's confinement root does not.
    pub fn runs_for_repo(&self, repo_id: &str) -> Result<Vec<RunRow>> {
        self.load_all(
            "load runs for repo",
            &format!("{RUN_SELECT} WHERE repo_id = ?1 ORDER BY id"),
            params![repo_id],
        )
    }

    pub fn insert_attempt(&mut self, attempt: &AttemptRow) -> Result<i64> {
        self.insert(
            "insert attempt",
            "INSERT INTO attempts (
                 run_id, n, session_id, session_attempt_id, started_at, ended_at, outcome,
                 retry_trigger, tokens, cost_micros
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                attempt.run_id,
                attempt.n,
                attempt.session_id,
                attempt.session_attempt_id,
                attempt.started_at,
                attempt.ended_at,
                attempt.outcome.map(AttemptOutcome::as_str),
                attempt.retry_trigger.map(RetryTrigger::as_str),
                attempt.tokens,
                attempt.cost_micros,
            ],
        )
    }

    pub fn attempts_for_run(&self, run_id: i64) -> Result<Vec<AttemptRow>> {
        self.load_all(
            "load attempts",
            &format!("{ATTEMPT_SELECT} WHERE run_id = ?1 ORDER BY n"),
            params![run_id],
        )
    }

    pub fn append_event(&mut self, event: &EventRow) -> Result<i64> {
        self.insert(
            "append event",
            "INSERT INTO events (run_id, type, class, payload, ts)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.run_id,
                event.event_type,
                event.class.as_str(),
                event.payload,
                event.ts,
            ],
        )
    }

    /// One run's journal in commit order.
    pub fn events_for_run(&self, run_id: i64) -> Result<Vec<EventRow>> {
        self.load_all(
            "load events",
            &format!("{EVENT_SELECT} WHERE run_id = ?1 ORDER BY id"),
            params![run_id],
        )
    }

    pub fn insert_question(&mut self, question: &QuestionRow) -> Result<i64> {
        self.insert(
            "insert question",
            "INSERT INTO questions (
                 run_id, kind, blocked_decision, options, recommendation, answer, author,
                 expires_at, tree_hash, paths_digest, state, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                question.run_id,
                question.kind.as_str(),
                question.blocked_decision,
                question.options,
                question.recommendation,
                question.answer,
                question.author.map(QuestionAuthor::as_str),
                question.expires_at,
                question.tree_hash,
                question.paths_digest,
                question.state.as_str(),
                question.created_at,
            ],
        )
    }

    pub fn load_question(&self, id: i64) -> Result<Option<QuestionRow>> {
        self.load_optional(
            "load question",
            &format!("{QUESTION_SELECT} WHERE id = ?1"),
            params![id],
        )
    }

    pub fn questions_for_run(&self, run_id: i64) -> Result<Vec<QuestionRow>> {
        self.load_all(
            "load questions",
            &format!("{QUESTION_SELECT} WHERE run_id = ?1 ORDER BY id"),
            params![run_id],
        )
    }

    pub fn insert_finding(&mut self, finding: &FindingRow) -> Result<i64> {
        self.insert(
            "insert finding",
            "INSERT INTO findings (
                 run_id, checkpoint_id, description, evidence_class, proof_refs,
                 causal_disposition, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                finding.run_id,
                finding.checkpoint_id,
                finding.description,
                finding.evidence_class.as_str(),
                finding.proof_refs,
                finding.causal_disposition.as_str(),
                finding.created_at,
            ],
        )
    }

    pub fn findings_for_run(&self, run_id: i64) -> Result<Vec<FindingRow>> {
        self.load_all(
            "load findings",
            &format!("{FINDING_SELECT} WHERE run_id = ?1 ORDER BY id"),
            params![run_id],
        )
    }

    /// Records a provider's quota state, replacing whatever was known before.
    pub fn record_provider(&mut self, provider: &ProviderRow) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO providers (provider, quota_state, reset_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(provider) DO UPDATE SET
                     quota_state = excluded.quota_state,
                     reset_at = excluded.reset_at,
                     updated_at = excluded.updated_at",
                params![
                    provider.provider,
                    provider.quota_state.as_str(),
                    provider.reset_at,
                    provider.updated_at,
                ],
            )
            .map_err(|error| {
                ControlPlaneError::operation("record provider", &self.database_path, error)
            })?;

        Ok(())
    }

    pub fn load_provider(&self, provider: &str) -> Result<Option<ProviderRow>> {
        self.load_optional(
            "load provider",
            &format!("{PROVIDER_SELECT} WHERE provider = ?1"),
            params![provider],
        )
    }

    /// Records the derived health signals of one run, replacing the previous
    /// snapshot. Derived state has no history worth keeping: it is recomputable
    /// from the journal and the evidence ledger.
    pub fn record_run_health(&mut self, health: &RunHealthRow) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO run_health (
                     run_id, last_progress_turn, noop_turns, failing_test_signature,
                     tokens_since_progress, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(run_id) DO UPDATE SET
                     last_progress_turn = excluded.last_progress_turn,
                     noop_turns = excluded.noop_turns,
                     failing_test_signature = excluded.failing_test_signature,
                     tokens_since_progress = excluded.tokens_since_progress,
                     updated_at = excluded.updated_at",
                params![
                    health.run_id,
                    health.last_progress_turn,
                    health.noop_turns,
                    health.failing_test_signature,
                    health.tokens_since_progress,
                    health.updated_at,
                ],
            )
            .map_err(|error| {
                ControlPlaneError::operation("record run health", &self.database_path, error)
            })?;

        Ok(())
    }

    pub fn load_run_health(&self, run_id: i64) -> Result<Option<RunHealthRow>> {
        self.load_optional(
            "load run health",
            &format!("{RUN_HEALTH_SELECT} WHERE run_id = ?1"),
            params![run_id],
        )
    }

    fn insert(
        &self,
        operation: &str,
        statement: &str,
        parameters: impl rusqlite::Params,
    ) -> Result<i64> {
        self.connection
            .execute(statement, parameters)
            .map_err(|error| ControlPlaneError::operation(operation, &self.database_path, error))?;

        Ok(self.connection.last_insert_rowid())
    }

    fn load_optional<T: FromRow>(
        &self,
        operation: &str,
        statement: &str,
        parameters: impl rusqlite::Params,
    ) -> Result<Option<T>> {
        Ok(self.load_all(operation, statement, parameters)?.pop())
    }

    fn load_all<T: FromRow>(
        &self,
        operation: &str,
        statement: &str,
        parameters: impl rusqlite::Params,
    ) -> Result<Vec<T>> {
        let mut prepared = self
            .connection
            .prepare(statement)
            .map_err(|error| ControlPlaneError::operation(operation, &self.database_path, error))?;

        let rows = prepared
            .query_map(parameters, |row| Ok(T::from_row(row)))
            .map_err(|error| ControlPlaneError::operation(operation, &self.database_path, error))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| ControlPlaneError::operation(operation, &self.database_path, error))?;

        rows.into_iter().collect()
    }
}

/// Reads one row of a table into its typed struct.
///
/// The column order each implementation reads is the order of the matching
/// `SELECT` constant, and nothing else selects those tables, so the two cannot
/// drift apart unnoticed.
trait FromRow: Sized {
    fn from_row(row: &Row<'_>) -> Result<Self>;
}

fn column<T: rusqlite::types::FromSql>(row: &Row<'_>, index: usize) -> Result<T> {
    row.get(index)
        .map_err(|error| ControlPlaneError::detail(format!("unreadable column {index}: {error}")))
}

fn enum_column<T>(row: &Row<'_>, index: usize, parse: fn(&str) -> Option<T>) -> Result<T> {
    let text: String = column(row, index)?;
    parse(&text).ok_or_else(|| ControlPlaneError::detail(format!("unknown value {text:?}")))
}

fn optional_enum_column<T>(
    row: &Row<'_>,
    index: usize,
    parse: fn(&str) -> Option<T>,
) -> Result<Option<T>> {
    let Some(text) = column::<Option<String>>(row, index)? else {
        return Ok(None);
    };

    parse(&text)
        .map(Some)
        .ok_or_else(|| ControlPlaneError::detail(format!("unknown value {text:?}")))
}

const RUN_SELECT: &str = "SELECT
     id, repo_id, repo_root, remote_url, external_ref, parent_run_id, task, scope, dod,
     genesis_paths, state, priority, dep_run_id, provider, budget_tokens, worktree_path,
     worktree_status, created_at, result
 FROM runs";

impl FromRow for RunRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: column(row, 0)?,
            repo_id: column(row, 1)?,
            repo_root: column(row, 2)?,
            remote_url: column(row, 3)?,
            external_ref: column(row, 4)?,
            parent_run_id: column(row, 5)?,
            task: column(row, 6)?,
            scope: column(row, 7)?,
            dod: column(row, 8)?,
            genesis_paths: column(row, 9)?,
            state: enum_column(row, 10, RunState::parse)?,
            priority: column(row, 11)?,
            dep_run_id: column(row, 12)?,
            provider: column(row, 13)?,
            budget_tokens: column(row, 14)?,
            worktree_path: column(row, 15)?,
            worktree_status: optional_enum_column(row, 16, WorktreeStatus::parse)?,
            created_at: column(row, 17)?,
            result: column(row, 18)?,
        })
    }
}

const ATTEMPT_SELECT: &str = "SELECT
     id, run_id, n, session_id, session_attempt_id, started_at, ended_at, outcome,
     retry_trigger, tokens, cost_micros
 FROM attempts";

impl FromRow for AttemptRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: column(row, 0)?,
            run_id: column(row, 1)?,
            n: column(row, 2)?,
            session_id: column(row, 3)?,
            session_attempt_id: column(row, 4)?,
            started_at: column(row, 5)?,
            ended_at: column(row, 6)?,
            outcome: optional_enum_column(row, 7, AttemptOutcome::parse)?,
            retry_trigger: optional_enum_column(row, 8, RetryTrigger::parse)?,
            tokens: column(row, 9)?,
            cost_micros: column(row, 10)?,
        })
    }
}

const EVENT_SELECT: &str = "SELECT id, run_id, type, class, payload, ts FROM events";

impl FromRow for EventRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: column(row, 0)?,
            run_id: column(row, 1)?,
            event_type: column(row, 2)?,
            class: enum_column(row, 3, EventClass::parse)?,
            payload: column(row, 4)?,
            ts: column(row, 5)?,
        })
    }
}

const QUESTION_SELECT: &str = "SELECT
     id, run_id, kind, blocked_decision, options, recommendation, answer, author, expires_at,
     tree_hash, paths_digest, state, created_at
 FROM questions";

impl FromRow for QuestionRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: column(row, 0)?,
            run_id: column(row, 1)?,
            kind: enum_column(row, 2, QuestionKind::parse)?,
            blocked_decision: column(row, 3)?,
            options: column(row, 4)?,
            recommendation: column(row, 5)?,
            answer: column(row, 6)?,
            author: optional_enum_column(row, 7, QuestionAuthor::parse)?,
            expires_at: column(row, 8)?,
            tree_hash: column(row, 9)?,
            paths_digest: column(row, 10)?,
            state: enum_column(row, 11, QuestionState::parse)?,
            created_at: column(row, 12)?,
        })
    }
}

const FINDING_SELECT: &str = "SELECT
     id, run_id, checkpoint_id, description, evidence_class, proof_refs, causal_disposition,
     created_at
 FROM findings";

impl FromRow for FindingRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: column(row, 0)?,
            run_id: column(row, 1)?,
            checkpoint_id: column(row, 2)?,
            description: column(row, 3)?,
            evidence_class: enum_column(row, 4, EvidenceClass::parse)?,
            proof_refs: column(row, 5)?,
            causal_disposition: enum_column(row, 6, CausalDisposition::parse)?,
            created_at: column(row, 7)?,
        })
    }
}

const PROVIDER_SELECT: &str = "SELECT provider, quota_state, reset_at, updated_at FROM providers";

impl FromRow for ProviderRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            provider: column(row, 0)?,
            quota_state: enum_column(row, 1, QuotaState::parse)?,
            reset_at: column(row, 2)?,
            updated_at: column(row, 3)?,
        })
    }
}

const RUN_HEALTH_SELECT: &str = "SELECT
     run_id, last_progress_turn, noop_turns, failing_test_signature, tokens_since_progress,
     updated_at
 FROM run_health";

impl FromRow for RunHealthRow {
    fn from_row(row: &Row<'_>) -> Result<Self> {
        Ok(Self {
            run_id: column(row, 0)?,
            last_progress_turn: column(row, 1)?,
            noop_turns: column(row, 2)?,
            failing_test_signature: column(row, 3)?,
            tokens_since_progress: column(row, 4)?,
            updated_at: column(row, 5)?,
        })
    }
}

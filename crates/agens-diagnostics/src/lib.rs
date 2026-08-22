//! Sanitized, capacity-bounded diagnostics capture: a rotating JSONL log
//! plus the reference-scoped `ProviderDiagnostics` handles that write to it.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agens_core::{TurnEvent, TurnProgressSink, TurnRetryReason};
use agens_providers::{
    DiagnosticRef, ProviderDiagnosticClass, ProviderDiagnosticComponent, ProviderDiagnosticEvent,
    ProviderDiagnosticKind, ProviderDiagnosticScope, ProviderDiagnostics, ReplayBudgetDimension,
};

use agens_bootstrap::Bootstrap;
use agens_error::CliError;

pub const DIAGNOSTIC_FILE_LIMIT_BYTES: u64 = 1024 * 1024;
pub const DIAGNOSTIC_FILE_COUNT_LIMIT: usize = 4;
pub static DIAGNOSTIC_REFERENCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static DIAGNOSTIC_FILE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static BEST_EFFORT_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Discards a fallible side effect that must not replace the caller's primary
/// result. Failures are counted in process memory and never written back into
/// the diagnostics log: that path is how we got here.
pub fn best_effort<T, E>(result: Result<T, E>) {
    if result.is_err() {
        BEST_EFFORT_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn best_effort_failures() -> u64 {
    BEST_EFFORT_FAILURES.load(Ordering::Relaxed)
}

/// How a turn ended, as a fact rather than as prose a supervisor has to parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl TurnOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The events a supervisor needs to follow a session without reading its
/// terminal: whether a turn is running, how it ended, which tool failed, and
/// whether the session is waiting on a permission decision.
///
/// Each variant carries only the fields that apply to it, so a recorded line is
/// readable on its own and no reader has to know which fields are meaningful
/// for which event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionLifecycle<'a> {
    /// Carries the session it started so a supervisor learns the id from the
    /// event itself. The first turn of a new session is otherwise unaddressable
    /// while it runs: nothing has completed yet, so the session is absent from
    /// every listing a supervisor could poll.
    TurnStarted {
        model: &'a str,
        session: i64,
    },
    /// A delegated child turn starting.
    ///
    /// It carries no session, because a child has none: it holds no session
    /// row, appears in no listing, and ends without leaving one. What addresses
    /// it is this line's own `reference`, so the moment it starts is the only
    /// moment its identity becomes knowable from outside the process running
    /// it.
    ChildTurnStarted {
        agent: &'a str,
        model: &'a str,
    },
    TurnEnded {
        outcome: TurnOutcome,
    },
    ToolFailed {
        tool: &'a str,
        class: ProviderDiagnosticClass,
    },
    /// The session cannot proceed until someone decides. This is the one state
    /// an external observer cannot infer from anything else: the process is
    /// alive, spending nothing, and making no progress.
    ///
    /// The blocked target is deliberately absent. A permission target carries
    /// the argument that triggered it — a whole shell command, a path — and
    /// this file is read by the audit overlay, so recording it would publish
    /// whatever secret the argument happened to contain. Which tool and which
    /// access level is what a supervisor needs to act.
    PermissionBlocked {
        tool: &'a str,
        access: &'a str,
    },
    /// A tool moved the session out of the directory it was working in.
    ///
    /// The directory is recorded, unlike a permission target, because it is
    /// the whole of the event: a supervisor reading this file to find out
    /// where a session is working learns nothing from the fact that it moved.
    /// It names a directory the session was already confined to, and carries
    /// no argument the caller composed.
    WorkingDirectoryChanged {
        directory: &'a str,
    },
    /// The provider refused the request because the history no longer fits the
    /// model's window.
    ///
    /// Recorded as its own fact rather than left inside a generic provider
    /// failure, for the same reason a quota limit carries its reset time: a
    /// supervisor can act on an exhausted window — it is unblocked by a
    /// compaction — and cannot act on an opaque rejection. Nothing else a
    /// provider rejects has an unblocking action attached to it.
    ContextExhausted {
        model: &'a str,
    },
    CompactionStarted {
        reason: CompactionReason,
    },
    /// How a compaction ended, including every way it declined to happen.
    ///
    /// A refusal is recorded, not swallowed: a compaction that did not run
    /// leaves the history exactly as it was, and a reader watching the thread
    /// jump would otherwise have no way to learn that the recovery it was
    /// counting on never took place.
    CompactionEnded {
        outcome: CompactionRecord,
    },
}

/// What asked for a compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionReason {
    /// A person asked for it.
    Manual,
    /// The history crossed the configured share of the window.
    Threshold,
    /// The provider already refused the request.
    Overflow,
}

impl CompactionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Threshold => "threshold",
            Self::Overflow => "overflow",
        }
    }
}

/// How a compaction ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionRecord {
    /// The history was replaced. `summarized` counts the messages the summary
    /// stands for and `kept` the ones that survived verbatim.
    Compacted { summarized: usize, kept: usize },
    /// Nothing was replaced. `reason` names which refusal applied.
    Refused { reason: &'static str },
}

impl SessionLifecycle<'_> {
    const fn kind(self) -> ProviderDiagnosticKind {
        match self {
            Self::TurnStarted { .. } | Self::ChildTurnStarted { .. } => {
                ProviderDiagnosticKind::TurnStarted
            }
            Self::TurnEnded { .. } => ProviderDiagnosticKind::TurnEnded,
            Self::ToolFailed { .. } => ProviderDiagnosticKind::ToolFailed,
            Self::PermissionBlocked { .. } => ProviderDiagnosticKind::PermissionBlocked,
            Self::WorkingDirectoryChanged { .. } => ProviderDiagnosticKind::WorkingDirectoryChanged,
            Self::ContextExhausted { .. } => ProviderDiagnosticKind::ContextExhausted,
            Self::CompactionStarted { .. } => ProviderDiagnosticKind::CompactionStarted,
            Self::CompactionEnded { .. } => ProviderDiagnosticKind::CompactionEnded,
        }
    }

    fn detail(self) -> serde_json::Value {
        match self {
            Self::TurnStarted { model, session } => {
                serde_json::json!({ "model": model, "session": session })
            }
            Self::ChildTurnStarted { agent, model } => {
                serde_json::json!({ "model": model, "agent": agent })
            }
            Self::TurnEnded { outcome } => serde_json::json!({ "outcome": outcome.as_str() }),
            Self::ToolFailed { tool, class } => {
                serde_json::json!({ "tool": tool, "class": class.as_str() })
            }
            Self::PermissionBlocked { tool, access } => {
                serde_json::json!({ "tool": tool, "access": access })
            }
            Self::WorkingDirectoryChanged { directory } => {
                serde_json::json!({ "directory": directory })
            }
            Self::ContextExhausted { model } => serde_json::json!({ "model": model }),
            Self::CompactionStarted { reason } => {
                serde_json::json!({ "reason": reason.as_str() })
            }
            Self::CompactionEnded { outcome } => match outcome {
                CompactionRecord::Compacted { summarized, kept } => serde_json::json!({
                    "outcome": "compacted",
                    "summarized": summarized,
                    "kept": kept,
                }),
                CompactionRecord::Refused { reason } => {
                    serde_json::json!({ "outcome": "refused", "reason": reason })
                }
            },
        }
    }
}

#[derive(Clone)]
pub struct SafeDiagnosticStore {
    directory: PathBuf,
    enabled: bool,
}

impl SafeDiagnosticStore {
    /// Capture is what `options.debug` switches: disabled, nothing about a
    /// failure is written to disk.
    pub fn with_capture(data_directory: PathBuf, enabled: bool) -> Self {
        Self {
            directory: data_directory.join("diagnostics"),
            enabled,
        }
    }

    pub fn record(&self, event: &ProviderDiagnosticEvent) {
        if !self.enabled {
            return;
        }
        let _guard = DIAGNOSTIC_FILE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        best_effort(self.write(event));
    }

    pub fn record_subagent_model_unavailable(
        &self,
        event: &ProviderDiagnosticEvent,
        agent: &str,
        requested_model: &str,
        fallback_model: &str,
    ) {
        if !self.enabled {
            return;
        }
        let _guard = DIAGNOSTIC_FILE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        best_effort(self.write_subagent_model_unavailable(
            event,
            agent,
            requested_model,
            fallback_model,
        ));
    }

    /// Records why a delegated subagent's declared tool surface was refused.
    ///
    /// The caller has no error type able to carry the reason to its own
    /// caller, so the record is the only place the offending declaration
    /// survives; without it the operator sees an opaque runtime failure and
    /// cannot tell a typo from a genuine over-grant.
    pub fn record_subagent_surface_rejection(&self, event: &ProviderDiagnosticEvent, reason: &str) {
        if !self.enabled {
            return;
        }
        let _guard = DIAGNOSTIC_FILE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        best_effort(self.write_subagent_surface_rejection(event, reason));
    }

    /// Records one session-lifecycle event.
    ///
    /// Kept apart from [`Self::record`] because a lifecycle event has no
    /// attempt, no retry budget and no HTTP status: forcing it through the
    /// provider event shape would write a line whose fields are mostly null and
    /// whose meaning a reader has to guess.
    pub fn record_session_lifecycle(
        &self,
        reference: &DiagnosticRef,
        scope: ProviderDiagnosticScope,
        event: SessionLifecycle<'_>,
    ) {
        if !self.enabled {
            return;
        }
        let _guard = DIAGNOSTIC_FILE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        best_effort(self.write_session_lifecycle(reference, scope, event));
    }

    fn write_session_lifecycle(
        &self,
        reference: &DiagnosticRef,
        scope: ProviderDiagnosticScope,
        event: SessionLifecycle<'_>,
    ) -> std::io::Result<()> {
        self.append(session_lifecycle_json_line(reference, scope, event)?)
    }

    fn write(&self, event: &ProviderDiagnosticEvent) -> std::io::Result<()> {
        self.append(diagnostic_json_line(event)?)
    }

    fn write_subagent_model_unavailable(
        &self,
        event: &ProviderDiagnosticEvent,
        agent: &str,
        requested_model: &str,
        fallback_model: &str,
    ) -> std::io::Result<()> {
        self.append(subagent_model_unavailable_json_line(
            event,
            agent,
            requested_model,
            fallback_model,
        )?)
    }

    fn write_subagent_surface_rejection(
        &self,
        event: &ProviderDiagnosticEvent,
        reason: &str,
    ) -> std::io::Result<()> {
        self.append(subagent_surface_rejection_json_line(event, reason)?)
    }

    /// Appends one already-serialized record to the active log, rotating first
    /// when the record would push the file past its size limit. The log is
    /// opened without following symlinks and kept owner-only.
    fn append(&self, line: Vec<u8>) -> std::io::Result<()> {
        ensure_private_diagnostics_directory(&self.directory)?;
        let active = self.active_path();
        let existing_size = match fs::symlink_metadata(&active) {
            Ok(metadata) if metadata.file_type().is_file() => metadata.len(),
            Ok(_) => {
                return Err(std::io::Error::other(
                    "diagnostics path is not a regular file",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        if existing_size.saturating_add(line.len() as u64) > DIAGNOSTIC_FILE_LIMIT_BYTES {
            self.rotate()?;
        }

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(active)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&line)
    }

    fn active_path(&self) -> PathBuf {
        self.directory
            .join(format!("agens-{}.jsonl", std::process::id()))
    }

    fn rotated_path(&self, generation: usize) -> PathBuf {
        self.directory
            .join(format!("agens-{}.{}.jsonl", std::process::id(), generation))
    }

    fn rotate(&self) -> std::io::Result<()> {
        let oldest = self.rotated_path(DIAGNOSTIC_FILE_COUNT_LIMIT - 1);
        match fs::remove_file(oldest) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        for generation in (1..DIAGNOSTIC_FILE_COUNT_LIMIT - 1).rev() {
            let source = self.rotated_path(generation);
            let destination = self.rotated_path(generation + 1);
            match fs::rename(source, destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        match fs::rename(self.active_path(), self.rotated_path(1)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn ensure_private_diagnostics_directory(directory: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        }
        Ok(_) => Err(std::io::Error::other("diagnostics path is not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Recursive because the data directory itself may not exist yet: a
            // run's first diagnostic is emitted before anything opens the
            // store, and a non-recursive create fails there, silently dropping
            // the one event that says the session started.
            //
            // The mode applies to every directory this creates, so a data
            // directory born here is private like the rest of the run's state.
            // An existing directory keeps whatever mode it already had.
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.recursive(true);
            builder.create(directory)
        }
        Err(error) => Err(error),
    }
}

fn diagnostic_json_line(event: &ProviderDiagnosticEvent) -> std::io::Result<Vec<u8>> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut line = serde_json::to_vec(&serde_json::json!({
        "timestamp_ms": u64::try_from(timestamp_ms).unwrap_or(u64::MAX),
        "reference": event.reference.as_str(),
        "scope": event.scope.as_str(),
        "component": event.component.as_str(),
        "event": event.event.as_str(),
        "attempt": event.attempt,
        "max_attempts": event.max_attempts,
        "delay_ms": event.delay_ms,
        "status": event.status,
        "class": event.class.map(ProviderDiagnosticClass::as_str),
        "input_class": event.input_class.map(ProviderDiagnosticClass::as_str),
        "budget_dimension": event.budget_dimension.map(ReplayBudgetDimension::as_str),
        "observed": event.observed,
        "limit": event.limit,
    }))
    .map_err(std::io::Error::other)?;
    line.push(b'\n');
    Ok(line)
}

fn session_lifecycle_json_line(
    reference: &DiagnosticRef,
    scope: ProviderDiagnosticScope,
    event: SessionLifecycle<'_>,
) -> std::io::Result<Vec<u8>> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut line = serde_json::json!({
        "timestamp_ms": u64::try_from(timestamp_ms).unwrap_or(u64::MAX),
        "reference": reference.as_str(),
        "scope": scope.as_str(),
        "component": ProviderDiagnosticComponent::Session.as_str(),
        "event": event.kind().as_str(),
    });
    if let (Some(line), Some(detail)) = (line.as_object_mut(), event.detail().as_object()) {
        line.extend(detail.clone());
    }

    let mut line = serde_json::to_vec(&line).map_err(std::io::Error::other)?;
    line.push(b'\n');
    Ok(line)
}

fn subagent_model_unavailable_json_line(
    event: &ProviderDiagnosticEvent,
    agent: &str,
    requested_model: &str,
    fallback_model: &str,
) -> std::io::Result<Vec<u8>> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut line = serde_json::to_vec(&serde_json::json!({
        "timestamp_ms": u64::try_from(timestamp_ms).unwrap_or(u64::MAX),
        "reference": event.reference.as_str(),
        "scope": event.scope.as_str(),
        "component": event.component.as_str(),
        "event": event.event.as_str(),
        "attempt": event.attempt,
        "max_attempts": event.max_attempts,
        "delay_ms": event.delay_ms,
        "status": event.status,
        "class": event.class.map(ProviderDiagnosticClass::as_str),
        "agent": agent,
        "requested_model": requested_model,
        "fallback_model": fallback_model,
    }))
    .map_err(std::io::Error::other)?;
    line.push(b'\n');
    Ok(line)
}

fn subagent_surface_rejection_json_line(
    event: &ProviderDiagnosticEvent,
    reason: &str,
) -> std::io::Result<Vec<u8>> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut line = serde_json::to_vec(&serde_json::json!({
        "timestamp_ms": u64::try_from(timestamp_ms).unwrap_or(u64::MAX),
        "reference": event.reference.as_str(),
        "scope": event.scope.as_str(),
        "component": event.component.as_str(),
        "event": "surface_rejection",
        "attempt": event.attempt,
        "max_attempts": event.max_attempts,
        "delay_ms": event.delay_ms,
        "status": event.status,
        "class": event.class.map(ProviderDiagnosticClass::as_str),
        "reason": reason,
    }))
    .map_err(std::io::Error::other)?;
    line.push(b'\n');
    Ok(line)
}

pub struct OperationDiagnostics {
    pub reference: String,
    pub provider: ProviderDiagnostics,
}

pub fn operation_diagnostics(
    bootstrap: &Bootstrap,
    scope: ProviderDiagnosticScope,
    reference: Option<&str>,
) -> OperationDiagnostics {
    operation_diagnostics_with_progress(bootstrap, scope, reference, None)
}

/// Same as [`operation_diagnostics`], with the retry events also delivered to
/// the turn's progress sink.
///
/// The JSONL store is capture-gated by `--debug`, so it can never be the only
/// consumer of a retry: a user watching a backoff needs the status line to say
/// so whether or not capture is on. Only parent-scope retries are forwarded —
/// a subagent's backoff must not rewrite the label of the turn that owns it.
pub fn operation_diagnostics_with_progress(
    bootstrap: &Bootstrap,
    scope: ProviderDiagnosticScope,
    reference: Option<&str>,
    progress: Option<TurnProgressSink>,
) -> OperationDiagnostics {
    let reference = reference.map_or_else(next_diagnostic_reference, str::to_owned);
    let store = diagnostic_store(bootstrap);
    let forwards_retries = matches!(scope, ProviderDiagnosticScope::Parent);
    let sink = Arc::new(move |event: ProviderDiagnosticEvent| {
        store.record(&event);
        if !forwards_retries {
            return;
        }
        if let Some(progress) = progress.as_ref()
            && let Some(retry) = retry_progress_event(&event)
        {
            progress(retry);
        }
    });
    let provider = ProviderDiagnostics::new(reference.clone(), scope, sink)
        .expect("generated diagnostics references are valid");
    OperationDiagnostics {
        reference,
        provider,
    }
}

/// The progress event a diagnostic carries, when it is a scheduled retry.
fn retry_progress_event(event: &ProviderDiagnosticEvent) -> Option<TurnEvent> {
    if event.event != ProviderDiagnosticKind::RetryScheduled {
        return None;
    }

    Some(TurnEvent::ProviderRetry {
        attempt: event.attempt,
        max_attempts: (event.max_attempts > 0).then_some(event.max_attempts),
        delay: event.delay_ms.map(Duration::from_millis),
        reason: retry_reason(event.class, event.status),
    })
}

fn retry_reason(class: Option<ProviderDiagnosticClass>, status: Option<u16>) -> TurnRetryReason {
    match class {
        Some(ProviderDiagnosticClass::RateLimited) => TurnRetryReason::RateLimited,
        Some(ProviderDiagnosticClass::Server) => TurnRetryReason::ServerError,
        Some(ProviderDiagnosticClass::Network) => TurnRetryReason::Network,
        Some(ProviderDiagnosticClass::Deadline) => TurnRetryReason::Timeout,
        _ if status == Some(429) => TurnRetryReason::RateLimited,
        _ if status.is_some_and(|status| status >= 500) => TurnRetryReason::ServerError,
        _ => TurnRetryReason::Transient,
    }
}

pub fn diagnostic_store(bootstrap: &Bootstrap) -> SafeDiagnosticStore {
    SafeDiagnosticStore::with_capture(bootstrap.data_directory().to_path_buf(), bootstrap.debug())
}

pub fn next_diagnostic_reference() -> String {
    let sequence = DIAGNOSTIC_REFERENCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mixed = timestamp
        .rotate_left(17)
        .wrapping_add(sequence.wrapping_mul(0x9e37_79b9))
        ^ u64::from(std::process::id());
    format!("{:08x}", mixed as u32)
}

pub fn record_subagent_model_unavailable(
    bootstrap: &Bootstrap,
    reference: &str,
    agent: &str,
    requested_model: &str,
    fallback_model: &str,
) {
    let Ok(reference) = DiagnosticRef::new(reference.to_owned()) else {
        return;
    };
    let event = ProviderDiagnosticEvent {
        reference,
        scope: ProviderDiagnosticScope::Subagent,
        component: ProviderDiagnosticComponent::Subagent,
        event: ProviderDiagnosticKind::Terminal,
        attempt: 0,
        max_attempts: 0,
        delay_ms: None,
        status: None,
        class: Some(ProviderDiagnosticClass::ModelUnavailable),
        input_class: None,
        budget_dimension: None,
        observed: None,
        limit: None,
    };
    diagnostic_store(bootstrap).record_subagent_model_unavailable(
        &event,
        agent,
        requested_model,
        fallback_model,
    );
}

pub fn record_subagent_terminal(
    bootstrap: &Bootstrap,
    reference: &str,
    class: ProviderDiagnosticClass,
    input_class: Option<ProviderDiagnosticClass>,
) {
    let Ok(reference) = DiagnosticRef::new(reference.to_owned()) else {
        return;
    };
    diagnostic_store(bootstrap).record(&ProviderDiagnosticEvent {
        reference,
        scope: ProviderDiagnosticScope::Subagent,
        component: ProviderDiagnosticComponent::Subagent,
        event: ProviderDiagnosticKind::Terminal,
        attempt: 0,
        max_attempts: 0,
        delay_ms: None,
        status: None,
        class: Some(class),
        input_class,
        budget_dimension: None,
        observed: None,
        limit: None,
    });
}

/// Records a refused subagent tool surface against `reference`, naming the
/// declaration that caused it. See
/// [`SafeDiagnosticStore::record_subagent_surface_rejection`].
pub fn record_subagent_surface_rejection(bootstrap: &Bootstrap, reference: &str, reason: &str) {
    let Ok(reference) = DiagnosticRef::new(reference.to_owned()) else {
        return;
    };
    diagnostic_store(bootstrap).record_subagent_surface_rejection(
        &ProviderDiagnosticEvent {
            reference,
            scope: ProviderDiagnosticScope::Subagent,
            component: ProviderDiagnosticComponent::Subagent,
            event: ProviderDiagnosticKind::Terminal,
            attempt: 0,
            max_attempts: 0,
            delay_ms: None,
            status: None,
            class: Some(ProviderDiagnosticClass::Runtime),
            input_class: None,
            budget_dimension: None,
            observed: None,
            limit: None,
        },
        reason,
    );
}

pub fn record_parent_terminal(bootstrap: &Bootstrap, reference: &str, error: &CliError) {
    if error.message == agens_core::HeadlessTaskTerminal::ModelUnavailable.message() {
        return;
    }
    let class = match error.category {
        "auth" => ProviderDiagnosticClass::Authentication,
        "cancelled" => ProviderDiagnosticClass::Cancelled,
        "provider" => ProviderDiagnosticClass::Provider,
        "timeout" => ProviderDiagnosticClass::Deadline,
        "tool" => ProviderDiagnosticClass::Tool,
        _ => ProviderDiagnosticClass::Runtime,
    };
    let Ok(reference) = DiagnosticRef::new(reference.to_owned()) else {
        return;
    };
    diagnostic_store(bootstrap).record(&ProviderDiagnosticEvent {
        reference,
        scope: ProviderDiagnosticScope::Parent,
        component: ProviderDiagnosticComponent::Responses,
        event: ProviderDiagnosticKind::Terminal,
        attempt: 0,
        max_attempts: 0,
        delay_ms: None,
        status: None,
        class: Some(class),
        input_class: None,
        budget_dimension: None,
        observed: None,
        limit: None,
    });
}

/// Records a session-lifecycle event against this run's diagnostic store.
pub fn record_session_lifecycle(
    bootstrap: &Bootstrap,
    reference: &str,
    scope: ProviderDiagnosticScope,
    event: SessionLifecycle<'_>,
) {
    let Ok(reference) = DiagnosticRef::new(reference.to_owned()) else {
        return;
    };
    diagnostic_store(bootstrap).record_session_lifecycle(&reference, scope, event);
}

pub fn record_agent_diagnostic(bootstrap: &Bootstrap, event: ProviderDiagnosticKind) {
    let Ok(reference) = DiagnosticRef::new(next_diagnostic_reference()) else {
        return;
    };
    diagnostic_store(bootstrap).record(&ProviderDiagnosticEvent {
        reference,
        scope: ProviderDiagnosticScope::Parent,
        component: ProviderDiagnosticComponent::Agent,
        event,
        attempt: 0,
        max_attempts: 0,
        delay_ms: None,
        status: None,
        class: Some(ProviderDiagnosticClass::Runtime),
        input_class: None,
        budget_dimension: None,
        observed: None,
        limit: None,
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::atomic::Ordering;

    use super::*;

    #[test]
    fn a_degraded_terminal_records_input_and_output_class() {
        let temporary =
            std::env::temp_dir().join(format!("agens-diagnostic-degraded-{}", std::process::id()));
        std::fs::remove_dir_all(&temporary).ok();
        std::fs::create_dir_all(&temporary).expect("test data directory should be creatable");

        let store = SafeDiagnosticStore::with_capture(temporary.clone(), true);
        store.record(&ProviderDiagnosticEvent {
            reference: DiagnosticRef::new("abcd1234".to_owned()).unwrap(),
            scope: ProviderDiagnosticScope::Subagent,
            component: ProviderDiagnosticComponent::Subagent,
            event: ProviderDiagnosticKind::Terminal,
            attempt: 0,
            max_attempts: 0,
            delay_ms: None,
            status: None,
            class: Some(ProviderDiagnosticClass::Runtime),
            input_class: Some(ProviderDiagnosticClass::Permission),
            budget_dimension: None,
            observed: None,
            limit: None,
        });

        let recorded = std::fs::read_dir(temporary.join("diagnostics"))
            .expect("enabled capture should create the directory")
            .filter_map(Result::ok)
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
            .collect::<String>();

        assert!(
            recorded.contains(r#""class":"runtime""#),
            "output class must survive: {recorded}"
        );
        assert!(
            recorded.contains(r#""input_class":"permission""#),
            "input class must survive the degradation: {recorded}"
        );
        assert!(!recorded.contains("/home/"));
        assert!(!recorded.contains("authorization"));

        std::fs::remove_dir_all(&temporary).ok();
    }

    #[test]
    fn a_diagnostics_write_failure_is_counted_and_does_not_change_the_caller() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-diagnostic-write-fail-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&temporary).ok();
        std::fs::create_dir_all(&temporary).expect("test data directory should be creatable");
        std::fs::write(temporary.join("diagnostics"), b"not a directory")
            .expect("blocking file should be writable");

        let event = ProviderDiagnosticEvent {
            reference: DiagnosticRef::new("abcd1234".to_owned()).unwrap(),
            scope: ProviderDiagnosticScope::Parent,
            component: ProviderDiagnosticComponent::Responses,
            event: ProviderDiagnosticKind::Terminal,
            attempt: 0,
            max_attempts: 0,
            delay_ms: None,
            status: None,
            class: Some(ProviderDiagnosticClass::Provider),
            input_class: None,
            budget_dimension: None,
            observed: None,
            limit: None,
        };

        let before = best_effort_failures();
        let store = SafeDiagnosticStore::with_capture(temporary.clone(), true);

        store.record(&event);
        store.record(&event);

        assert!(
            best_effort_failures() >= before + 2,
            "write failures must be counted without writing them back into the log"
        );
        assert!(temporary.join("diagnostics").is_file());

        std::fs::remove_dir_all(&temporary).ok();
    }

    #[test]
    fn diagnostics_capture_follows_the_debug_setting() {
        let temporary =
            std::env::temp_dir().join(format!("agens-diagnostic-capture-{}", std::process::id()));
        std::fs::remove_dir_all(&temporary).ok();
        std::fs::create_dir_all(&temporary).expect("test data directory should be creatable");
        let event = ProviderDiagnosticEvent {
            reference: DiagnosticRef::new("abcd1234".to_owned()).unwrap(),
            scope: ProviderDiagnosticScope::Parent,
            component: ProviderDiagnosticComponent::Responses,
            event: ProviderDiagnosticKind::Terminal,
            attempt: 0,
            max_attempts: 0,
            delay_ms: None,
            status: None,
            class: Some(ProviderDiagnosticClass::Provider),
            input_class: None,
            budget_dimension: None,
            observed: None,
            limit: None,
        };

        SafeDiagnosticStore::with_capture(temporary.clone(), false).record(&event);
        assert!(!temporary.join("diagnostics").exists());

        SafeDiagnosticStore::with_capture(temporary.clone(), true).record(&event);
        assert!(
            std::fs::read_dir(temporary.join("diagnostics"))
                .expect("enabled capture should create the directory")
                .count()
                > 0
        );

        std::fs::remove_dir_all(&temporary).ok();
    }

    /// A subagent whose declared surface cannot be resolved never starts, so
    /// the reason names the offending declaration or it is lost: the caller
    /// collapses the failure into an opaque runtime error one frame later.
    #[test]
    fn a_rejected_subagent_surface_records_the_declaration_that_caused_it() {
        let temporary =
            std::env::temp_dir().join(format!("agens-surface-rejection-{}", std::process::id()));
        std::fs::remove_dir_all(&temporary).ok();
        std::fs::create_dir_all(&temporary).expect("test data directory should be creatable");
        let event = ProviderDiagnosticEvent {
            reference: DiagnosticRef::new("abcd1234".to_owned()).unwrap(),
            scope: ProviderDiagnosticScope::Subagent,
            component: ProviderDiagnosticComponent::Subagent,
            event: ProviderDiagnosticKind::Terminal,
            attempt: 0,
            max_attempts: 0,
            delay_ms: None,
            status: None,
            class: Some(ProviderDiagnosticClass::Runtime),
            input_class: None,
            budget_dimension: None,
            observed: None,
            limit: None,
        };

        SafeDiagnosticStore::with_capture(temporary.clone(), true)
            .record_subagent_surface_rejection(
                &event,
                "permission declaration grants a tool the parent does not hold: not_a_real_tool",
            );

        let recorded = std::fs::read_dir(temporary.join("diagnostics"))
            .expect("enabled capture should create the directory")
            .filter_map(Result::ok)
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
            .collect::<String>();

        assert!(
            recorded.contains("not_a_real_tool"),
            "the offending declaration must survive into the record, got: {recorded}"
        );
        assert!(recorded.contains("surface_rejection"));

        std::fs::remove_dir_all(&temporary).ok();
    }

    fn diagnostic(
        kind: ProviderDiagnosticKind,
        class: Option<ProviderDiagnosticClass>,
        status: Option<u16>,
    ) -> ProviderDiagnosticEvent {
        ProviderDiagnosticEvent {
            reference: DiagnosticRef::new("abcd1234".to_owned()).unwrap(),
            scope: ProviderDiagnosticScope::Parent,
            component: ProviderDiagnosticComponent::Responses,
            event: kind,
            attempt: 2,
            max_attempts: 3,
            delay_ms: Some(1500),
            status,
            class,
            input_class: None,
            budget_dimension: None,
            observed: None,
            limit: None,
        }
    }

    #[test]
    fn a_scheduled_retry_becomes_a_progress_event_carrying_its_attempt_and_reason() {
        let event = retry_progress_event(&diagnostic(
            ProviderDiagnosticKind::RetryScheduled,
            Some(ProviderDiagnosticClass::RateLimited),
            Some(429),
        ));

        assert_eq!(
            event,
            Some(TurnEvent::ProviderRetry {
                attempt: 2,
                max_attempts: Some(3),
                delay: Some(Duration::from_millis(1500)),
                reason: TurnRetryReason::RateLimited,
            })
        );
    }

    #[test]
    fn diagnostics_that_are_not_retries_stay_out_of_the_progress_stream() {
        assert_eq!(
            retry_progress_event(&diagnostic(
                ProviderDiagnosticKind::Attempt,
                Some(ProviderDiagnosticClass::RateLimited),
                Some(429),
            )),
            None
        );
        assert_eq!(
            retry_progress_event(&diagnostic(
                ProviderDiagnosticKind::Terminal,
                Some(ProviderDiagnosticClass::Server),
                Some(503),
            )),
            None
        );
    }

    #[test]
    fn an_unbounded_retry_reports_no_ceiling() {
        let mut event = diagnostic(ProviderDiagnosticKind::RetryScheduled, None, None);
        event.max_attempts = 0;

        let Some(TurnEvent::ProviderRetry { max_attempts, .. }) = retry_progress_event(&event)
        else {
            panic!("a scheduled retry should produce a progress event");
        };
        assert_eq!(max_attempts, None);
    }

    #[test]
    fn a_missing_class_falls_back_to_the_status_the_provider_reported() {
        assert_eq!(retry_reason(None, Some(429)), TurnRetryReason::RateLimited);
        assert_eq!(retry_reason(None, Some(503)), TurnRetryReason::ServerError);
        assert_eq!(retry_reason(None, None), TurnRetryReason::Transient);
        assert_eq!(
            retry_reason(Some(ProviderDiagnosticClass::Network), Some(429)),
            TurnRetryReason::Network
        );
    }

    #[test]
    fn unavailable_subagent_model_diagnostics_include_resolution_context() {
        let data_directory = std::env::temp_dir().join(format!(
            "agens-model-diagnostic-{}-{}",
            std::process::id(),
            DIAGNOSTIC_REFERENCE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&data_directory).expect("test data directory should be created");
        let store = SafeDiagnosticStore::with_capture(data_directory.clone(), true);
        let event = ProviderDiagnosticEvent {
            reference: DiagnosticRef::new("abc12345".into()).expect("reference should be valid"),
            scope: ProviderDiagnosticScope::Subagent,
            component: ProviderDiagnosticComponent::Subagent,
            event: ProviderDiagnosticKind::Terminal,
            attempt: 0,
            max_attempts: 0,
            delay_ms: None,
            status: None,
            class: Some(ProviderDiagnosticClass::ModelUnavailable),
            input_class: None,
            budget_dimension: None,
            observed: None,
            limit: None,
        };

        store.record_subagent_model_unavailable(
            &event,
            "worker",
            "unavailable-model",
            "session-model",
        );

        let active = data_directory
            .join("diagnostics")
            .join(format!("agens-{}.jsonl", std::process::id()));
        let line = std::fs::read_to_string(active).expect("diagnostic should be readable");
        let object =
            serde_json::from_str::<serde_json::Value>(&line).expect("diagnostic should be JSON");
        assert_eq!(object["agent"], "worker");
        assert_eq!(object["requested_model"], "unavailable-model");
        assert_eq!(object["fallback_model"], "session-model");
        assert_eq!(object["class"], "model_unavailable");
        assert!(object.get("input_class").is_none() || object["input_class"].is_null());

        std::fs::remove_dir_all(data_directory).expect("test directory should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_store_writes_only_allowlisted_jsonl_with_private_bounded_files() {
        use std::os::unix::fs::PermissionsExt;

        let data_directory = std::env::temp_dir().join(format!(
            "agens-safe-diagnostics-{}-{}",
            std::process::id(),
            DIAGNOSTIC_REFERENCE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&data_directory).expect("test data directory should be created");
        let store = SafeDiagnosticStore::with_capture(data_directory.clone(), true);
        let event = ProviderDiagnosticEvent {
            reference: DiagnosticRef::new("abc12345".into()).expect("reference should be valid"),
            scope: ProviderDiagnosticScope::Subagent,
            component: ProviderDiagnosticComponent::Responses,
            event: ProviderDiagnosticKind::RetryScheduled,
            attempt: 1,
            max_attempts: 3,
            delay_ms: Some(275),
            status: Some(429),
            class: Some(ProviderDiagnosticClass::RateLimited),
            input_class: None,
            budget_dimension: None,
            observed: None,
            limit: None,
        };

        store.record(&event);

        let diagnostics_directory = data_directory.join("diagnostics");
        assert_eq!(
            std::fs::metadata(&diagnostics_directory)
                .expect("diagnostics metadata should be readable")
                .permissions()
                .mode()
                & 0o077,
            0
        );
        let active = diagnostics_directory.join(format!("agens-{}.jsonl", std::process::id()));
        let line = std::fs::read_to_string(&active).expect("diagnostic should be readable");
        let object = serde_json::from_str::<serde_json::Value>(&line)
            .expect("diagnostic should be JSON")
            .as_object()
            .expect("diagnostic should be an object")
            .clone();
        assert_eq!(
            object.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "attempt",
                "budget_dimension",
                "class",
                "component",
                "delay_ms",
                "event",
                "input_class",
                "limit",
                "max_attempts",
                "observed",
                "reference",
                "scope",
                "status",
                "timestamp_ms",
            ])
        );
        assert_eq!(object["reference"], "abc12345");
        assert!(!line.contains("prompt"));
        assert!(!line.contains("authorization"));
        assert_eq!(
            std::fs::metadata(&active)
                .expect("diagnostic file metadata should be readable")
                .permissions()
                .mode()
                & 0o077,
            0
        );

        for _ in 0..4 {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&active)
                .expect("active diagnostics file should open")
                .set_len(DIAGNOSTIC_FILE_LIMIT_BYTES)
                .expect("test should fill diagnostics file");
            store.record(&event);
        }
        assert_eq!(
            std::fs::read_dir(&diagnostics_directory)
                .expect("diagnostics directory should be readable")
                .count(),
            DIAGNOSTIC_FILE_COUNT_LIMIT
        );
        assert!(
            std::fs::read_dir(&diagnostics_directory)
                .expect("diagnostics directory should be readable")
                .all(|entry| entry
                    .expect("diagnostic entry should be readable")
                    .metadata()
                    .expect("diagnostic metadata should be readable")
                    .len()
                    <= DIAGNOSTIC_FILE_LIMIT_BYTES)
        );

        std::fs::remove_dir_all(data_directory).expect("test directory should be removed");
    }

    /// A supervisor asks three questions of a running session, and until these
    /// events existed the only way to answer them was reading the terminal.
    #[test]
    fn a_session_lifecycle_is_recorded_as_typed_events() {
        let temporary =
            std::env::temp_dir().join(format!("agens-diagnostic-lifecycle-{}", std::process::id()));
        std::fs::remove_dir_all(&temporary).ok();
        std::fs::create_dir_all(&temporary).expect("test data directory should be creatable");

        let store = SafeDiagnosticStore::with_capture(temporary.clone(), true);
        let reference = DiagnosticRef::new("abcd1234".to_owned()).unwrap();
        store.record_session_lifecycle(
            &reference,
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::TurnStarted {
                model: "moonshotai/kimi-k3",
                session: 42,
            },
        );
        store.record_session_lifecycle(
            &reference,
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::ToolFailed {
                tool: "bash",
                class: ProviderDiagnosticClass::Tool,
            },
        );
        store.record_session_lifecycle(
            &reference,
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::PermissionBlocked {
                tool: "bash",
                access: "write",
            },
        );
        store.record_session_lifecycle(
            &reference,
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::TurnEnded {
                outcome: TurnOutcome::Failed,
            },
        );

        let recorded = recorded_lines(&temporary);
        let events: Vec<&str> = recorded
            .iter()
            .filter_map(|line| line["event"].as_str())
            .collect();

        assert_eq!(
            events,
            vec![
                "turn_started",
                "tool_failed",
                "permission_blocked",
                "turn_ended"
            ]
        );
        assert!(
            recorded.iter().all(|line| line["component"] == "session"
                && line["reference"] == "abcd1234"
                && line["scope"] == "parent"),
            "{recorded:?}"
        );
        assert_eq!(recorded[0]["model"], "moonshotai/kimi-k3");
        assert_eq!(recorded[0]["session"], 42);
        assert_eq!(recorded[1]["tool"], "bash");
        assert_eq!(recorded[1]["class"], "tool");
        assert_eq!(recorded[2]["access"], "write");
        assert!(
            recorded[2].get("target").is_none(),
            "a permission target can carry a secret and is never recorded: {:?}",
            recorded[2]
        );
        assert_eq!(recorded[3]["outcome"], "failed");

        std::fs::remove_dir_all(&temporary).ok();
    }

    /// Where a session is working is a question a supervisor answers from this
    /// file, so the move that changes the answer has to be in it.
    #[test]
    fn a_move_out_of_the_session_root_is_recorded_with_the_directory() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-diagnostic-working-directory-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&temporary).ok();
        std::fs::create_dir_all(&temporary).expect("test data directory should be creatable");

        let store = SafeDiagnosticStore::with_capture(temporary.clone(), true);
        store.record_session_lifecycle(
            &DiagnosticRef::new("d4c3b2a1".to_owned()).unwrap(),
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::WorkingDirectoryChanged {
                directory: "/data/worktrees/repo/feature",
            },
        );

        let recorded = recorded_lines(&temporary);

        assert_eq!(recorded[0]["event"], "working_directory_changed");
        assert_eq!(recorded[0]["directory"], "/data/worktrees/repo/feature");

        std::fs::remove_dir_all(&temporary).ok();
    }

    /// An exhausted window and the compaction that answers it are the one
    /// failure a supervisor can unblock without a person, so both the fact and
    /// what the recovery did with it are recorded — including a recovery that
    /// declined to run and left the history untouched.
    #[test]
    fn an_exhausted_context_and_its_compaction_are_recorded_as_typed_events() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-diagnostic-compaction-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&temporary).ok();
        std::fs::create_dir_all(&temporary).expect("test data directory should be creatable");

        let store = SafeDiagnosticStore::with_capture(temporary.clone(), true);
        let reference = DiagnosticRef::new("beef0001".to_owned()).unwrap();
        store.record_session_lifecycle(
            &reference,
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::ContextExhausted {
                model: "moonshotai/kimi-k3",
            },
        );
        store.record_session_lifecycle(
            &reference,
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::CompactionStarted {
                reason: CompactionReason::Overflow,
            },
        );
        store.record_session_lifecycle(
            &reference,
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::CompactionEnded {
                outcome: CompactionRecord::Compacted {
                    summarized: 12,
                    kept: 3,
                },
            },
        );
        store.record_session_lifecycle(
            &reference,
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::CompactionEnded {
                outcome: CompactionRecord::Refused {
                    reason: "summary was empty",
                },
            },
        );

        let recorded = recorded_lines(&temporary);
        let events: Vec<&str> = recorded
            .iter()
            .filter_map(|line| line["event"].as_str())
            .collect();

        assert_eq!(
            events,
            vec![
                "context_exhausted",
                "compaction_started",
                "compaction_ended",
                "compaction_ended"
            ]
        );
        assert_eq!(recorded[0]["model"], "moonshotai/kimi-k3");
        assert_eq!(recorded[1]["reason"], "overflow");
        assert_eq!(recorded[2]["outcome"], "compacted");
        assert_eq!(recorded[2]["summarized"], 12);
        assert_eq!(recorded[2]["kept"], 3);
        assert_eq!(recorded[3]["outcome"], "refused");
        assert_eq!(recorded[3]["reason"], "summary was empty");

        std::fs::remove_dir_all(&temporary).ok();
    }

    /// A delegated turn has no session id to carry, so the line's own reference
    /// is the address a supervisor writes to. If that reference were missing or
    /// the line named a session the child does not have, a supervisor would
    /// have nothing to aim at for the longest stretch of a delegation.
    #[test]
    fn a_child_turn_publishes_the_reference_that_addresses_it() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-diagnostic-child-turn-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&temporary).ok();
        std::fs::create_dir_all(&temporary).expect("test data directory should be creatable");

        let store = SafeDiagnosticStore::with_capture(temporary.clone(), true);
        store.record_session_lifecycle(
            &DiagnosticRef::new("a1b2c3d4".to_owned()).unwrap(),
            ProviderDiagnosticScope::Subagent,
            SessionLifecycle::ChildTurnStarted {
                agent: "reviewer",
                model: "moonshotai/kimi-k3",
            },
        );

        let recorded = recorded_lines(&temporary);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0]["event"], "turn_started");
        assert_eq!(recorded[0]["scope"], "subagent");
        assert_eq!(recorded[0]["reference"], "a1b2c3d4");
        assert_eq!(recorded[0]["agent"], "reviewer");
        assert_eq!(recorded[0]["model"], "moonshotai/kimi-k3");
        assert!(
            recorded[0].get("session").is_none(),
            "a delegated turn holds no session: {:?}",
            recorded[0]
        );

        std::fs::remove_dir_all(&temporary).ok();
    }

    /// The whole point is that a supervisor never has to read the terminal, so
    /// a lifecycle line has to be readable on its own.
    #[test]
    fn a_lifecycle_line_carries_only_fields_that_apply_to_it() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-diagnostic-lifecycle-shape-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&temporary).ok();
        std::fs::create_dir_all(&temporary).expect("test data directory should be creatable");

        let store = SafeDiagnosticStore::with_capture(temporary.clone(), true);
        store.record_session_lifecycle(
            &DiagnosticRef::new("abcd1234".to_owned()).unwrap(),
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::TurnEnded {
                outcome: TurnOutcome::Completed,
            },
        );

        let recorded = recorded_lines(&temporary);
        let line = recorded.first().expect("one line is recorded");

        assert_eq!(line["outcome"], "completed");
        assert!(line.get("tool").is_none(), "{line:?}");
        assert!(line.get("model").is_none(), "{line:?}");
        assert!(line["timestamp_ms"].is_u64(), "{line:?}");

        std::fs::remove_dir_all(&temporary).ok();
    }

    /// Capture stays the single switch: `options.debug` off writes nothing at
    /// all, lifecycle included.
    #[test]
    fn disabled_capture_records_no_lifecycle_event() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-diagnostic-lifecycle-off-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&temporary).ok();
        std::fs::create_dir_all(&temporary).expect("test data directory should be creatable");

        SafeDiagnosticStore::with_capture(temporary.clone(), false).record_session_lifecycle(
            &DiagnosticRef::new("abcd1234".to_owned()).unwrap(),
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::TurnEnded {
                outcome: TurnOutcome::Cancelled,
            },
        );

        assert!(!temporary.join("diagnostics").exists());

        std::fs::remove_dir_all(&temporary).ok();
    }

    /// The first diagnostic of a run happens before anything else has created
    /// the data directory, so a store that cannot create its own parent drops
    /// exactly the event that says a session started.
    #[test]
    fn a_missing_data_directory_is_created_rather_than_dropping_the_first_event() {
        let temporary = std::env::temp_dir().join(format!(
            "agens-diagnostic-absent-parent-{}/data",
            std::process::id()
        ));
        std::fs::remove_dir_all(temporary.parent().unwrap()).ok();
        assert!(!temporary.exists(), "the parent must be absent to start");

        SafeDiagnosticStore::with_capture(temporary.clone(), true).record_session_lifecycle(
            &DiagnosticRef::new("abcd1234".to_owned()).unwrap(),
            ProviderDiagnosticScope::Parent,
            SessionLifecycle::TurnStarted {
                model: "kimi-k3",
                session: 7,
            },
        );

        let recorded = recorded_lines(&temporary);

        assert_eq!(recorded.len(), 1, "{recorded:?}");
        // The recorded line is the whole proof. The best-effort failure counter
        // is process-wide, so a before/after comparison measures every test
        // running beside this one, not this write.
        assert_eq!(recorded[0]["event"], "turn_started");

        std::fs::remove_dir_all(temporary.parent().unwrap()).ok();
    }

    fn recorded_lines(directory: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_dir(directory.join("diagnostics"))
            .expect("enabled capture should create the directory")
            .filter_map(Result::ok)
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
            .collect::<String>()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect()
    }
}

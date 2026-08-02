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
    ProviderDiagnosticKind, ProviderDiagnosticScope, ProviderDiagnostics,
};

use agens_bootstrap::Bootstrap;
use agens_error::CliError;

pub const DIAGNOSTIC_FILE_LIMIT_BYTES: u64 = 1024 * 1024;
pub const DIAGNOSTIC_FILE_COUNT_LIMIT: usize = 4;
pub static DIAGNOSTIC_REFERENCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static DIAGNOSTIC_FILE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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
        let _ = self.write(event);
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
        let _ =
            self.write_subagent_model_unavailable(event, agent, requested_model, fallback_model);
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
        let _ = self.write_subagent_surface_rejection(event, reason);
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
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
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
    }))
    .map_err(std::io::Error::other)?;
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
    });
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
    });
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::atomic::Ordering;

    use super::*;

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
                "class",
                "component",
                "delay_ms",
                "event",
                "max_attempts",
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
}

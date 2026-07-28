//! Sanitized, capacity-bounded diagnostics capture: a rotating JSONL log
//! plus the reference-scoped `ProviderDiagnostics` handles that write to it.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

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

    fn write(&self, event: &ProviderDiagnosticEvent) -> std::io::Result<()> {
        ensure_private_diagnostics_directory(&self.directory)?;
        let line = diagnostic_json_line(event)?;
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

pub struct OperationDiagnostics {
    pub reference: String,
    pub provider: ProviderDiagnostics,
}

pub fn operation_diagnostics(
    bootstrap: &Bootstrap,
    scope: ProviderDiagnosticScope,
    reference: Option<&str>,
) -> OperationDiagnostics {
    let reference = reference.map_or_else(next_diagnostic_reference, str::to_owned);
    let store = diagnostic_store(bootstrap);
    let sink = Arc::new(move |event: ProviderDiagnosticEvent| store.record(&event));
    let provider = ProviderDiagnostics::new(reference.clone(), scope, sink)
        .expect("generated diagnostics references are valid");
    OperationDiagnostics {
        reference,
        provider,
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

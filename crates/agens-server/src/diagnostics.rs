//! The coordinator's second reader.
//!
//! The journal is the control plane's own record and stays authoritative, but
//! it is a SQLite table behind the facade: reading it means attaching a client,
//! and a supervisor that is watching a machine rather than a project has none.
//! The diagnostics log is the file that supervisor already tails, so what the
//! daemon admitted, deferred, ticked and detected is written there too.
//!
//! Three things follow from that being a *file*, and they are why this exists
//! at all rather than the journal being enough:
//!
//! - A fact about the daemon rather than about a run has nowhere else to go.
//!   The journal's rows hang off runs, and the core being unusable is not one.
//! - A control plane that cannot be written to still leaves a record.
//! - It is capture-gated, so a machine that did not ask for diagnostics writes
//!   nothing at all.
//!
//! Every field written from here is an integer or a name from a closed set the
//! daemon authored. Paths, branches, task text and worker messages stay in the
//! journal, which is not what the audit overlay reads.

use std::path::Path;

use agens_diagnostics::{CoordinatorEvent, SafeDiagnosticStore, next_diagnostic_reference};
use agens_providers::DiagnosticRef;
use agens_store::EventRow;

use crate::ingest::HealthSignal;
use crate::scheduler::{AdmissionFailure, Deferral, Ineligible, QueueReport};
use crate::timers::TimerTick;

/// One daemon's handle on the diagnostics log.
///
/// Cloned into every loop. The reference is minted once and shared by all of
/// them, because what these lines have in common is the daemon that wrote them:
/// a reference per loop would make the coordinator look like several unrelated
/// components to whoever is correlating the file.
#[derive(Clone)]
pub struct CoordinatorDiagnostics {
    store: SafeDiagnosticStore,
    reference: Option<DiagnosticRef>,
}

impl CoordinatorDiagnostics {
    pub fn new(data_directory: &Path, enabled: bool) -> Self {
        Self {
            store: SafeDiagnosticStore::with_capture(data_directory.to_path_buf(), enabled),
            // A reference that cannot be built is one nothing can be recorded
            // under, which turns this handle off rather than writing lines a
            // reader cannot correlate.
            reference: enabled
                .then(|| DiagnosticRef::new(next_diagnostic_reference()).ok())
                .flatten(),
        }
    }

    /// Whether anything written here reaches a file.
    ///
    /// Read by the publisher loop, which otherwise skips the journal's tail
    /// entirely while no client is subscribed — and a supervisor reading this
    /// file is exactly the reader that is not subscribed.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.reference.is_some()
    }

    /// Projects one journal entry, when it is one of the two a supervisor
    /// follows a run by.
    ///
    /// Read off the journal rather than emitted where the transition is applied
    /// so there is one producer: every path that moves a run — a client, the
    /// scheduler, a gate, the timer wheel, a worker's report — writes these two
    /// entries, and a call site per path would be a set of call sites that can
    /// disagree with the control plane.
    pub fn journal_entry(&self, event: &EventRow) {
        let Some(run_id) = event.run_id else {
            return;
        };

        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload) else {
            return;
        };

        match event.event_type.as_str() {
            "run_state_changed" => self.record(CoordinatorEvent::RunStateChanged {
                run_id,
                machine: text(&payload, "machine"),
                from: text(&payload, "from"),
                to: text(&payload, "to"),
                trigger: text(&payload, "trigger"),
            }),
            "gate_result" => self.record(CoordinatorEvent::GateResult {
                run_id,
                gate: text(&payload, "gate"),
                passed: payload
                    .get("passed")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or_default(),
                reason: payload.get("reason").and_then(serde_json::Value::as_str),
            }),
            _ => {}
        }
    }

    /// What one admission tick did and did not do.
    ///
    /// Every occurrence, unlike the journal's entry for the same condition: the
    /// journal records the condition and a file that rotates records the fact
    /// that it is still true, which is how a supervisor tells a queue that is
    /// stuck from one nobody has looked at since.
    pub fn admission(&self, report: &QueueReport) {
        for admitted in &report.admitted {
            self.record(CoordinatorEvent::RunAdmitted {
                run_id: admitted.run_id,
                resumed: admitted.resumed,
            });
        }

        for (run_id, deferral) in &report.deferred {
            self.record(CoordinatorEvent::RunDeferred {
                run_id: *run_id,
                reason: deferral_reason(deferral),
            });
        }

        for (run_id, failure) in &report.failures {
            self.record(CoordinatorEvent::AdmissionFailed {
                run_id: *run_id,
                reason: failure_reason(failure),
            });
        }
    }

    /// One pass of the timer wheel.
    ///
    /// A pass that raised nothing is not recorded: the wheel ticks on every
    /// heartbeat and a line per empty tick would be the whole file. What a
    /// supervisor reads from the absence of these lines is a wheel with nothing
    /// due, which is also what it reads from a wheel that stopped — the two are
    /// told apart by the daemon still being there.
    pub fn timers(&self, tick: &TimerTick) {
        if tick.quota_resets.is_empty()
            && tick.expired_questions.is_empty()
            && tick.overdue_checkpoints.is_empty()
        {
            return;
        }

        self.record(CoordinatorEvent::TimersTicked {
            quota_resets: tick.quota_resets.len(),
            expired_questions: tick.expired_questions.len(),
            overdue_checkpoints: tick.overdue_checkpoints.len(),
        });
    }

    /// What the detectors made of one run's evidence.
    ///
    /// A divergence's path is deliberately not carried. The signal and the
    /// reason are what a supervisor acts on, and the path itself is a file in
    /// somebody's repository.
    pub fn health_signal(&self, run_id: i64, signal: &HealthSignal) {
        self.record(CoordinatorEvent::HealthSignalRaised {
            run_id,
            signal: signal.event_type(),
            reason: signal_reason(signal),
        });
    }

    /// A fact a reporter could not hand to ingest.
    ///
    /// Written every occurrence the caller decides to record, which is once per
    /// run while a backlog stands: the journal says a run lost evidence, and
    /// this file says the queue is still full.
    pub fn ingest_backlogged(&self, run_id: i64, reporter: &str, fact: &str) {
        self.record(CoordinatorEvent::IngestBacklogged {
            run_id,
            reporter,
            fact,
        });
    }

    /// The service core was left poisoned, so the daemon is stopping.
    pub fn core_poisoned(&self, component: &str) {
        self.record(CoordinatorEvent::CorePoisoned { component });
    }

    fn record(&self, event: CoordinatorEvent<'_>) {
        if let Some(reference) = &self.reference {
            self.store.record_coordinator(reference, event);
        }
    }
}

/// A string field of a journal payload, or an empty one. The payload is the
/// daemon's own and every field is always present; reading it defensively
/// keeps a malformed row from being the thing that stops the loop.
fn text<'a>(payload: &'a serde_json::Value, field: &str) -> &'a str {
    payload
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
}

const fn deferral_reason(deferral: &Deferral) -> &'static str {
    match deferral {
        Deferral::Ineligible(Ineligible::ProviderCapped { .. }) => "provider_capped",
        Deferral::Ineligible(Ineligible::WorktreeNotReady { .. }) => "worktree_not_ready",
        Deferral::Ineligible(Ineligible::DependencyPending { .. }) => "dependency_pending",
        Deferral::MaxConcurrent { .. } => "max_concurrent",
        Deferral::WorktreeCeiling { .. } => "worktree_ceiling",
        Deferral::ProviderHeadroom { .. } => "provider_headroom",
    }
}

const fn failure_reason(failure: &AdmissionFailure) -> &'static str {
    match failure {
        AdmissionFailure::Launch(_) => "launch",
        AdmissionFailure::Refused(_) => "refused",
    }
}

const fn signal_reason(signal: &HealthSignal) -> &'static str {
    match signal {
        HealthSignal::WorkerLost { reason, .. } => reason.as_str(),
        HealthSignal::Divergence { .. } => "path_outside_genesis_paths",
        HealthSignal::UnrepresentableMutation => "unrepresentable_path",
    }
}

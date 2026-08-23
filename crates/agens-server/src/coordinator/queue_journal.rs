//! What one admission tick could not do, written down.
//!
//! The scheduler already reports every queued run that did not start and why,
//! and until now the admission loop read one boolean out of that report and
//! dropped the rest. A queue that stops moving is then invisible: the runs sit
//! at `queued`, no transition is applied, and nothing in the journal says which
//! ceiling they are sitting against or which launch keeps failing.
//!
//! Only changes are written. Admission ticks several times a second and a run
//! held by `max_concurrent` is held by it on every one of those ticks, so
//! journaling each occurrence would bury the journal under the same sentence.
//! What an operator needs is the condition and the moment it started, and the
//! moment it stopped is the entry that never comes for it again.

use std::collections::HashMap;

use agens_store::{EventClass, EventRow};

use crate::fsm::StateMachines;
use crate::scheduler::{AdmissionFailure, Deferral, Ineligible, QueueReport};

/// A queued run stayed queued, naming what held it.
pub const RUN_DEFERRED_EVENT: &str = "run_deferred";

/// A launch was attempted for a queued run and did not work.
pub const ADMISSION_FAILED_EVENT: &str = "admission_failed";

/// The last thing journaled about each run admission could not start.
///
/// Held by the admission loop for the life of the daemon and rebuilt from every
/// tick's report, so it is bounded by the depth of the queue rather than by the
/// number of runs the daemon has ever seen.
#[derive(Default)]
pub(super) struct QueueJournal {
    reported: HashMap<i64, String>,
}

impl QueueJournal {
    /// Journals what changed since the last tick.
    ///
    /// Failures are journaled through the same "only what changed" rule as
    /// deferrals. The admission loop already pauses after a failed launch, and
    /// a run whose launch fails the same way every time is one condition rather
    /// than one condition per pause.
    pub(super) fn record(&mut self, machines: &mut StateMachines, report: &QueueReport, now: i64) {
        let mut standing: HashMap<i64, String> = HashMap::new();
        let mut events = Vec::new();

        for (run_id, deferral) in &report.deferred {
            self.note(
                &mut standing,
                &mut events,
                *run_id,
                RUN_DEFERRED_EVENT,
                deferral_payload(deferral),
                now,
            );
        }

        for (run_id, failure) in &report.failures {
            self.note(
                &mut standing,
                &mut events,
                *run_id,
                ADMISSION_FAILED_EVENT,
                failure_payload(failure),
                now,
            );
        }

        self.reported = standing;

        if events.is_empty() {
            return;
        }

        // A journal write that failed is not worth failing the tick over: the
        // runs are where the tick left them, and the next one reports the same
        // condition because nothing has recorded it yet.
        if machines.journal(&events).is_err() {
            self.reported.clear();
        }
    }

    fn note(
        &self,
        standing: &mut HashMap<i64, String>,
        events: &mut Vec<EventRow>,
        run_id: i64,
        event_type: &str,
        payload: serde_json::Value,
        now: i64,
    ) {
        let payload = payload.to_string();
        let unchanged = self.reported.get(&run_id) == Some(&payload);

        standing.insert(run_id, payload.clone());

        if unchanged {
            return;
        }

        events.push(EventRow {
            id: None,
            run_id: Some(run_id),
            event_type: event_type.to_owned(),
            class: EventClass::Infra,
            payload,
            ts: now,
        });
    }
}

fn deferral_payload(deferral: &Deferral) -> serde_json::Value {
    match deferral {
        Deferral::Ineligible(Ineligible::ProviderCapped { provider }) => {
            serde_json::json!({ "reason": "provider_capped", "provider": provider })
        }
        Deferral::Ineligible(Ineligible::WorktreeNotReady { worktree_status }) => {
            serde_json::json!({
                "reason": "worktree_not_ready",
                "worktree_status": worktree_status.map(|status| status.as_str()),
            })
        }
        Deferral::Ineligible(Ineligible::DependencyPending {
            dep_run_id,
            worktree_status,
        }) => serde_json::json!({
            "reason": "dependency_pending",
            "dep_run_id": dep_run_id,
            "worktree_status": worktree_status.map(|status| status.as_str()),
        }),
        Deferral::MaxConcurrent { running, limit } => serde_json::json!({
            "reason": "max_concurrent",
            "running": running,
            "limit": limit,
        }),
        Deferral::WorktreeCeiling { held, limit } => serde_json::json!({
            "reason": "worktree_ceiling",
            "held": held,
            "limit": limit,
        }),
        Deferral::ProviderHeadroom {
            provider,
            running,
            headroom,
        } => serde_json::json!({
            "reason": "provider_headroom",
            "provider": provider,
            "running": running,
            "headroom": headroom,
        }),
    }
}

fn failure_payload(failure: &AdmissionFailure) -> serde_json::Value {
    match failure {
        AdmissionFailure::Launch(error) => {
            serde_json::json!({ "reason": "launch", "detail": error.to_string() })
        }
        AdmissionFailure::Refused(rejection) => {
            serde_json::json!({ "reason": "refused", "detail": rejection.to_string() })
        }
    }
}

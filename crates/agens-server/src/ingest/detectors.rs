//! The two detectors that read the derived signals: the lost worker, and the
//! mechanical half of divergence.
//!
//! Both are comparisons over typed facts, with no model anywhere: the team is
//! protected even with every provider capped. Neither moves a run. A detector
//! raises a signal, and what to do about it is Praetor's judgment — a slow but
//! correct worker costs more when it is killed than when it is watched.

use agens_store::RunHealthRow;

/// Where the thresholds come from. Conservative by construction, to be
/// calibrated against real use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthThresholds {
    /// Consecutive turns without observable progress before a checkpoint that
    /// still claims progress reads as a worker that has lost the thread.
    pub stall_turns: i64,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self { stall_turns: 5 }
    }
}

/// Why a worker reads as lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LostReason {
    /// An active checkpoint reports progress the passive layer never saw.
    ProgressClaimedWhileStalled,
    /// The promised checkpoint never arrived within its grace.
    CheckpointExpired,
}

impl LostReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProgressClaimedWhileStalled => "progress_claimed_while_stalled",
            Self::CheckpointExpired => "checkpoint_expired",
        }
    }
}

/// Something the coordinator noticed and Praetor has to judge.
///
/// A signal, never a verdict: it is journaled and handed up, and nothing in
/// this module reclaims a slot, fails an attempt or moves a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HealthSignal {
    WorkerLost {
        reason: LostReason,
        noop_turns: i64,
    },
    /// A path touched outside the frozen genesis paths. Raised on the fact that
    /// reported it, so the gate that consumes this signal can block before the
    /// work lands rather than after.
    Divergence {
        path: String,
    },
    /// A mutation whose path the harness could not represent — absolute, or
    /// reaching outside the session root. It cannot be shown to be inside the
    /// frozen set, and an uncomparable path is escalated rather than cleared.
    UnrepresentableMutation,
}

/// The journal entry a lost worker is recorded as.
///
/// Named because it is read as well as written: it is what a replay of the
/// journal reads the standing signal back from, so the fold a restart rebuilds
/// does not raise it a second time.
pub(super) const WORKER_LOST_EVENT: &str = "worker_lost";

impl HealthSignal {
    /// The journal entry name this signal is recorded under.
    #[must_use]
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::WorkerLost { .. } => WORKER_LOST_EVENT,
            Self::Divergence { .. } | Self::UnrepresentableMutation => "divergence_detected",
        }
    }
}

/// What the detector knows about the run's checkpoint that the health row does
/// not carry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckpointStanding {
    /// A checkpoint has been reported for this run, so the run has left the
    /// grace period in which no claim exists to contradict.
    pub active: bool,
    /// The last checkpoint said the work moved forward.
    pub claims_progress: bool,
    /// The timer wheel reported the promised checkpoint's grace as elapsed.
    pub expired: bool,
}

/// Whether the run reads as a lost worker.
///
/// Two conditions, both from the design: an active checkpoint claiming progress
/// while the passive layer counted at least `stall_turns` turns without any, or
/// a checkpoint that never arrived. A run with no checkpoint yet raises
/// nothing: there is no claim to contradict, and the first interval is grace.
#[must_use]
pub(crate) fn detect_worker_lost(
    health: &RunHealthRow,
    checkpoint: &CheckpointStanding,
    thresholds: &HealthThresholds,
) -> Option<HealthSignal> {
    if checkpoint.expired {
        return Some(HealthSignal::WorkerLost {
            reason: LostReason::CheckpointExpired,
            noop_turns: health.noop_turns,
        });
    }

    let stalled_claim = checkpoint.active
        && checkpoint.claims_progress
        && health.noop_turns >= thresholds.stall_turns;

    stalled_claim.then_some(HealthSignal::WorkerLost {
        reason: LostReason::ProgressClaimedWhileStalled,
        noop_turns: health.noop_turns,
    })
}

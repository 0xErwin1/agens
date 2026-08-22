//! What ingest reads from a checkpoint.
//!
//! The checkpoint tool owns the row: its identity, its promised next
//! checkpoint, the findings hanging off it. Ingest owns only the consequence of
//! one field, so it declares the half it consumes as a trait the row
//! implements rather than depending on the row's shape.

use agens_store::EvidenceClass;

/// One checkpoint's claim about the work, as the health derivation reads it.
///
/// Only a deterministic claim of progress credits progress. The other two
/// classes are recorded and change nothing, which is the whole mechanical
/// consequence of the evidence class: without a consumer the field would be
/// narrative with a verification label on it.
pub trait CheckpointClaim {
    fn evidence_class(&self) -> EvidenceClass;
    /// Whether the checkpoint says the work moved forward.
    fn claims_progress(&self) -> bool;
}

/// A claim as it travels over the ingest channel.
///
/// A value rather than a trait object so the channel stays a plain typed queue.
/// [`ReportedCheckpoint::from_claim`] is the door a stored checkpoint row comes
/// through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportedCheckpoint {
    pub evidence_class: EvidenceClass,
    pub claims_progress: bool,
}

impl ReportedCheckpoint {
    #[must_use]
    pub const fn new(evidence_class: EvidenceClass, claims_progress: bool) -> Self {
        Self {
            evidence_class,
            claims_progress,
        }
    }

    #[must_use]
    pub fn from_claim(claim: &impl CheckpointClaim) -> Self {
        Self {
            evidence_class: claim.evidence_class(),
            claims_progress: claim.claims_progress(),
        }
    }

    /// Whether this claim credits progress: deterministic evidence, and a claim
    /// that the work moved.
    #[must_use]
    pub const fn credits_progress(&self) -> bool {
        self.claims_progress && matches!(self.evidence_class, EvidenceClass::Deterministic)
    }
}

impl CheckpointClaim for ReportedCheckpoint {
    fn evidence_class(&self) -> EvidenceClass {
        self.evidence_class
    }

    fn claims_progress(&self) -> bool {
        self.claims_progress
    }
}

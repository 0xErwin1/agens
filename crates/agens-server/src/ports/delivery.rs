//! The safe-point queue a worker drains, written from the control plane's side.
//!
//! The queue is `agens-store`'s `directives` table, which is already the one a
//! running turn pulls from at a tool-call edge and the one the next prompt is
//! assembled from at a turn edge. Nothing new is invented here: the core says
//! what is being delivered and at which grain, and this addresses it.
//!
//! **A delivery is addressed to the run, not to a session.** The two moments
//! that queue one are answering a question and steering a run, and the first of
//! them happens while the run is parked — its worker has ended, and the session
//! that will read the answer has not started. Addressing the session that just
//! stopped would drop every answer a parked run ever receives. The queue's
//! addressee is therefore this run's mailbox, which the session executing the
//! run reads under the same name however many attempts it takes.

use std::sync::Mutex;

use agens_core::IntraTurnInputSource;
use agens_store::{DirectiveGrain, DirectiveKind, DirectiveStore, DirectiveTarget};

use crate::api::{Delivery, DeliveryGrain, DeliveryPayload, DeliveryQueue, PortError};

/// The name a run's queued messages are addressed under.
///
/// Stable across attempts, because what a delivery is meant for is the run: an
/// answer queued while the run was parked is read by whichever session picks
/// the run up next.
#[must_use]
pub(crate) fn run_mailbox(run_id: i64) -> String {
    format!("run:{run_id}")
}

/// The durable queue, over the daemon's directive store.
///
/// The store is behind a mutex because it holds one SQLite connection and the
/// core performs an effect from whichever thread the request arrived on.
pub(crate) struct RunDeliveries {
    store: Mutex<DirectiveStore>,
}

impl RunDeliveries {
    #[must_use]
    pub(crate) const fn new(store: DirectiveStore) -> Self {
        Self {
            store: Mutex::new(store),
        }
    }
}

impl DeliveryQueue for RunDeliveries {
    fn enqueue(&self, delivery: &Delivery) -> Result<(), PortError> {
        let target = DirectiveTarget::Child(run_mailbox(delivery.run_id));
        let (kind, text) = kind_and_text(&delivery.payload);

        self.store
            .lock()
            .map_err(|_| {
                PortError::new("delivery", "the queue is unusable after a failed enqueue")
            })?
            .enqueue_kind(
                &target,
                kind,
                // The coordinator relays; it is never the speaker. Whether a
                // person or Praetor authored the answer is recorded on the
                // question, which is where the authorship of a decision
                // belongs.
                IntraTurnInputSource::Supervisor,
                grain(delivery.grain),
                &text,
            )
            .map_err(|error| PortError::new("delivery", error.to_string()))
    }
}

/// What the queue stores, from what the core is delivering.
///
/// The question id is not carried in the text: the answer is already on the
/// question row, and a worker that needs to know which decision it is reading
/// reads the run's own questions rather than parsing a queue entry.
fn kind_and_text(payload: &DeliveryPayload) -> (DirectiveKind, String) {
    match payload {
        DeliveryPayload::Answer { text, .. } => (DirectiveKind::Answer, text.clone()),
        DeliveryPayload::Directive(text) => (DirectiveKind::Directive, text.clone()),
        // The queue refuses an entry with no instruction in it, and a resume
        // still has to say what it is, so the nudge carries its own word.
        DeliveryPayload::Continue => (DirectiveKind::Continue, "continue".to_owned()),
    }
}

const fn grain(grain: DeliveryGrain) -> DirectiveGrain {
    match grain {
        DeliveryGrain::ToolCall => DirectiveGrain::ToolCall,
        DeliveryGrain::Turn => DirectiveGrain::Turn,
    }
}

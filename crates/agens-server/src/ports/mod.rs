//! The five seams of the service core, over the daemon's real components.
//!
//! [`crate::Ports`] is what the core performs a transition's effects through.
//! These are the coordinator's own implementations of it: admission's toggle
//! and its wake-up, the git derivation of a run's worktree, the durable
//! safe-point queue, the sessions a run executes in, and the live fan-out of
//! the journal. A suite driving the core supplies stubs of the traits instead.
//!
//! None of them reaches back into the core. The core holds them and calls them
//! while it is locked, so a port that took that same lock to read a row would
//! deadlock the daemon on its own first effect. Each one that needs the control
//! plane opens its own read handle instead, which the store supports because
//! the state machines remain its single writer.

mod delivery;
mod feed;
mod scheduler;
mod sessions;
mod worktrees;

// Composed by the coordinator and named by nobody else. A port implementation
// on the daemon's surface would read as a seam a caller is meant to supply, and
// the seam a caller supplies is the trait.
pub(crate) use delivery::{RunDeliveries, run_mailbox};
pub(crate) use feed::JournalFeed;
pub(crate) use scheduler::Admissions;
pub(crate) use sessions::SupervisedSessions;
pub use worktrees::GitWorktreeGate;

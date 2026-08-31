//! The terminal application: everything between a keystroke and a turn.
//!
//! `agens-tui` draws widgets and owns the conversation projection. This crate is
//! what drives it — routing a submission or a slash command, resuming a session,
//! choosing a model, answering a permission question, and reporting what the
//! runtime is doing while a turn runs.
//!
//! It is a surface, so it may depend on logic and logic may never depend on it.

pub mod ask_user_prompt;
pub mod attached;
pub mod dialogs;
pub mod engine;
pub mod extensions;
pub mod files;
pub mod fork;
pub mod metrics;
pub mod models;
pub mod permission_prompt;
pub mod profiles;
pub mod repository;
pub mod resume;
pub mod router;
pub mod session;
pub mod team;
pub mod turn;
pub mod undo;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

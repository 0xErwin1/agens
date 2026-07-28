//! What a session is, independent of any surface that shows it.
//!
//! A session is an identity, the metadata and messages recorded for it, the
//! confinement root its tools are opened against, the provider it speaks to, and
//! the attempts it has made. None of that changes with whether a terminal is
//! attached, which is why it does not live with the CLI.

pub mod attempt;
pub mod context;
pub mod model;
pub mod provider;
pub mod root;
pub mod turns;

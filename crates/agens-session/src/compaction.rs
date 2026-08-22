//! Running one compaction of a session, end to end.
//!
//! The cut and the transcript are decided in `agens-core`, where they are pure
//! functions over a history. This is the part that has effects: it announces
//! the compaction, asks a model for the summary, records what was replaced, and
//! hands back the new history.
//!
//! The ordering is the whole point. The history the caller holds is replaced
//! only after a summary exists, is non-empty, and has been written down. Every
//! earlier failure returns a refusal and leaves the caller with exactly what it
//! had, because a run reaching this path is already failing and a half-applied
//! compaction would turn a recoverable turn into an unrecoverable session.

use agens_core::Message;
use agens_core::compaction::{
    CompactionBudget, CompactionError, CompactionSummary, apply_compaction, plan_compaction,
};
use agens_diagnostics::{CompactionReason, CompactionRecord, SafeDiagnosticStore, SessionLifecycle};
use agens_providers::{DiagnosticRef, ProviderDiagnosticScope};
use agens_store::{CompactionStore, CompactionStoreError};

/// Produces the summary that stands in for the stretch being compacted.
///
/// A trait rather than a provider handle: the summarizing call is an ordinary
/// turn against some model, and which model, on which credentials, is a
/// decision this module has no business making.
pub trait CompactionSummarizer {
    /// Returns the summary text for `prompt`, or the reason it could not.
    fn summarize(&self, prompt: &str) -> Result<String, String>;
}

/// Why a compaction did not happen.
///
/// Every variant means the caller's history is untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionFailure {
    /// No cut was available. Either the history already fits, or every cut
    /// inside the budget would separate a tool call from its result.
    Plan(CompactionError),
    /// The summarizing call itself failed.
    Summarizer(String),
    /// The summarizing call returned, but with nothing usable in it.
    Summary(CompactionError),
    /// The summary could not be recorded.
    ///
    /// Refused rather than applied anyway: the record is what makes a
    /// compaction reconstructable, and a history replaced by a summary nothing
    /// wrote down is indistinguishable from a history that was lost.
    Store(CompactionStoreError),
}

impl CompactionFailure {
    /// The short reason recorded on the diagnostics line.
    fn recorded_reason(&self) -> &'static str {
        match self {
            Self::Plan(CompactionError::NothingToCompact) => "nothing to compact",
            Self::Plan(CompactionError::NoValidCut) => "no cut keeps tool calls with their results",
            Self::Plan(CompactionError::EmptySummary) | Self::Summary(_) => "summary was empty",
            Self::Summarizer(_) => "summarizing call failed",
            Self::Store(_) => "summary could not be recorded",
        }
    }
}

impl std::fmt::Display for CompactionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plan(error) | Self::Summary(error) => error.fmt(formatter),
            Self::Summarizer(detail) => write!(formatter, "summarizing call failed: {detail}"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CompactionFailure {}

/// What a compaction produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactedHistory {
    /// The history to use from now on. The caller's own is unchanged.
    pub messages: Vec<Message>,
    pub summary: String,
    /// How many messages the summary stands for.
    pub summarized: usize,
    /// How many survived verbatim.
    pub kept: usize,
    /// Identifier of the appended record.
    pub entry: i64,
}

/// Compacts one session's history.
///
/// `reference` and `diagnostics` carry the observability: a compaction moves the
/// thread under a reader's feet, and a jump nothing announced reads as the
/// session having lost its place.
pub fn compact_session(
    store: &mut CompactionStore,
    diagnostics: &SafeDiagnosticStore,
    reference: &DiagnosticRef,
    session_id: i64,
    messages: &[Message],
    budget: CompactionBudget,
    reason: CompactionReason,
    summarizer: &dyn CompactionSummarizer,
) -> Result<CompactedHistory, CompactionFailure> {
    diagnostics.record_session_lifecycle(
        reference,
        ProviderDiagnosticScope::Parent,
        SessionLifecycle::CompactionStarted { reason },
    );

    let result = run(store, session_id, messages, budget, summarizer);

    let record = match &result {
        Ok(compacted) => CompactionRecord::Compacted {
            summarized: compacted.summarized,
            kept: compacted.kept,
        },
        Err(failure) => CompactionRecord::Refused {
            reason: failure.recorded_reason(),
        },
    };
    diagnostics.record_session_lifecycle(
        reference,
        ProviderDiagnosticScope::Parent,
        SessionLifecycle::CompactionEnded { outcome: record },
    );

    result
}

fn run(
    store: &mut CompactionStore,
    session_id: i64,
    messages: &[Message],
    budget: CompactionBudget,
    summarizer: &dyn CompactionSummarizer,
) -> Result<CompactedHistory, CompactionFailure> {
    let plan = plan_compaction(messages, budget).map_err(CompactionFailure::Plan)?;

    let previous = store
        .latest(session_id)
        .map_err(CompactionFailure::Store)?
        .map(|entry| entry.summary);

    let text = summarizer
        .summarize(&plan.prompt(previous.as_deref()))
        .map_err(CompactionFailure::Summarizer)?;
    let summary = CompactionSummary::new(text).map_err(CompactionFailure::Summary)?;

    let entry = store
        .append(
            session_id,
            summary.as_str(),
            plan.first_kept() as i64,
        )
        .map_err(CompactionFailure::Store)?;

    Ok(CompactedHistory {
        messages: apply_compaction(messages, &plan, &summary),
        summary: summary.as_str().to_owned(),
        summarized: plan.first_kept() - plan.pinned(),
        kept: messages.len() - plan.first_kept(),
        entry,
    })
}

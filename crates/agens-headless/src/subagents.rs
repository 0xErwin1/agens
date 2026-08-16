//! Which subagents a turn asked for, and the note a caller shows when the
//! turn is interrupted before any of them finished.

use std::sync::Mutex;

use agens_core::{FactIdentity, ToolResultFacts, TurnEvent};
use agens_diagnostics::best_effort;
use agens_store::ToolFactStore;

use agens_session::turns::sanitize_subagent_summary;

const MAX_NOTED_REQUESTED_SUBAGENTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestedSubagent {
    pub agent: String,
    pub description: String,
}

/// Describes the turn as interrupted rather than cancelled by the user, because an expired
/// deadline reaches this path with the same terminal status as an explicit cancellation.
pub fn interrupted_turn_note(requested: &[RequestedSubagent]) -> String {
    let mut note = "[interrupted] The previous turn stopped before this assistant produced a \
                    result. Results of tools it had requested are unavailable, so their effects \
                    are unverified."
        .to_owned();
    if requested.is_empty() {
        return note;
    }

    note.push_str(" Subagents requested in that turn: ");
    note.push_str(
        &requested
            .iter()
            .map(|subagent| format!("{} — \"{}\"", subagent.agent, subagent.description))
            .collect::<Vec<_>>()
            .join("; "),
    );
    note.push('.');
    note
}

/// Notes a delegation the turn asked for, so an interrupted turn can say which
/// ones it left unfinished.
///
/// The name is reduced rather than compared against one spelling. This event
/// carries whatever named the call, and the two producers disagree: the task
/// tool is advertised to the provider as `task`, so a model-initiated
/// delegation arrives bare, while agens launching one on its own behalf spells
/// it `native::task`. Matching only the second silently recorded none of the
/// delegations a model asked for.
pub fn record_requested_subagent(requested: &Mutex<Vec<RequestedSubagent>>, event: &TurnEvent) {
    let TurnEvent::ToolCallRequested { name, input, .. } = event else {
        return;
    };
    if agens_core::bare_tool_name(name) != "task" {
        return;
    }
    let Some(subagent) = serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|value| {
            Some(RequestedSubagent {
                agent: sanitize_subagent_summary(value.get("agent")?.as_str()?),
                description: sanitize_subagent_summary(value.get("description")?.as_str()?),
            })
        })
    else {
        return;
    };

    if let Ok(mut requested) = requested.lock()
        && requested.len() < MAX_NOTED_REQUESTED_SUBAGENTS
        && !requested.contains(&subagent)
    {
        requested.push(subagent);
    }
}

/// Writes one fact to the evidence ledger, if its identity is ledger-eligible.
///
/// A fact with no `session_id`/`attempt_id` belongs to a turn running outside
/// a session attempt (a subagent child turn) and is intentionally not
/// ledger-writable, per the ledger's own key. A write failure is swallowed
/// rather than propagated: the ledger is evidence about the turn, not part of
/// the turn's own success criteria, so losing one row must never fail the
/// user's work.
pub fn record_tool_result_fact(
    store: &Mutex<ToolFactStore>,
    identity: &FactIdentity,
    facts: &ToolResultFacts,
) {
    let (Some(session_id), Some(attempt_id)) = (identity.session_id, identity.attempt_id) else {
        return;
    };

    if let Ok(mut store) = store.lock() {
        best_effort(store.record(
            session_id,
            attempt_id,
            identity.sequence,
            &identity.tool_call_id,
            facts,
        ));
    }
}

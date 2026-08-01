//! The registered scenario set.
//!
//! Adding a scenario is one module plus one entry in [`SCENARIOS`] — no
//! change to the harness, the writer, the schema, or the comparison tool.
//!
//! The syntax-highlight cache (`render::syntax_token_cache`) is
//! process-global, so a later scenario can observe an earlier one's cache
//! entries. Scenarios therefore run in this fixed order with disjoint
//! fixture content; reordering this array changes what each scenario
//! measures.

mod dense_tool_turn;
mod expand_collapse;
mod long_transcript;
mod short_transcript;
mod streaming_response;
mod streaming_with_spinner;
mod sustained_resize;

use super::Scenario;

pub const SCENARIOS: &[Scenario] = &[
    short_transcript::SCENARIO,
    long_transcript::SCENARIO,
    streaming_response::SCENARIO,
    streaming_with_spinner::SCENARIO,
    sustained_resize::SCENARIO,
    expand_collapse::SCENARIO,
    dense_tool_turn::SCENARIO,
];

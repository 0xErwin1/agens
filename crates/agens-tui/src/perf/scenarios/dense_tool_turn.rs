//! One long agentic turn, then ordinary typing and scrolling on top of it.
//!
//! This is the shape a real session reaches and the turn-count fixtures
//! cannot: history elision folds settled turns, so a hundred small turns cost
//! six, while a single turn with a hundred tool calls costs all hundred and
//! never folds.
//!
//! What it measures is the cost of interaction, not of arrival — every
//! keystroke and every scroll tick drives a frame, and the question is what
//! one of those frames pays for content that has not changed.

use std::io;

use crate::perf::fixtures;
use crate::perf::{Scenario, ScenarioContext};
use crate::{Event, Key, MouseWheelDirection};

const CALLS: usize = 200;
const KEYSTROKES: usize = 40;
const SCROLL_TICKS: usize = 40;

pub(super) const SCENARIO: Scenario = Scenario {
    name: "dense_tool_turn",
    run,
};

fn run(ctx: &mut ScenarioContext) -> io::Result<()> {
    let fixture = fixtures::tool_heavy_turn(CALLS);

    let _root = agens_perf::span!(
        "perf.scenario",
        scenario = SCENARIO.name,
        width = ctx.width(),
        height = ctx.height(),
        transcript_lines = fixture.lines as u64,
        iterations = (KEYSTROKES + SCROLL_TICKS) as u64,
    );

    ctx.load_transcript(&fixture.messages)?;
    ctx.render_frame(true)?;

    for index in 0..KEYSTROKES {
        let letter = char::from(b'a' + (index % 26) as u8);
        ctx.handle(Event::Key(Key::Char(letter)));
        ctx.render_frame(true)?;
    }

    for index in 0..SCROLL_TICKS {
        let direction = if index % 8 < 4 {
            MouseWheelDirection::Up
        } else {
            MouseWheelDirection::Down
        };
        ctx.handle(Event::MouseWheel(direction));
        ctx.render_frame(true)?;
    }

    Ok(())
}

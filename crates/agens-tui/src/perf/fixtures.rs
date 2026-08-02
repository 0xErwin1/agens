//! Deterministic transcript fixtures for scenario runs.
//!
//! A fixture is built from plain data, never from real provider output, so a
//! scenario's shape stays stable across machines and across time: the same
//! `(turns, lines_per_turn)` pair always produces the same messages.

use agens_core::{Message, MessagePart, Role};

/// A deterministic transcript and the line count it was built from.
pub struct TranscriptFixture {
    pub messages: Vec<Message>,
    pub lines: usize,
}

/// Builds one exchange whose assistant reply makes `calls` tool calls.
///
/// History elision folds settled *turns*, not lines, so a single turn is
/// never elided however large it grows. A long agentic turn is therefore the
/// shape that puts the most rows on screen at once, and the one a
/// turn-count-based fixture cannot produce.
pub fn tool_heavy_turn(calls: usize) -> TranscriptFixture {
    let mut messages = Vec::with_capacity(calls * 2 + 2);
    messages.push(Message {
        role: Role::User,
        parts: vec![MessagePart::Text("Do the whole thing.".to_owned())],
    });

    for call in 0..calls {
        let call_id = format!("call-{call}");
        messages.push(Message {
            role: Role::Assistant,
            parts: vec![MessagePart::ToolCall {
                id: call_id.clone(),
                name: "read".to_owned(),
                input: format!("{{\"path\":\"crate/module_{call}.rs\"}}"),
            }],
        });
        messages.push(Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: call_id,
                content: format!("pub fn item_{call}() -> u32 {{ {call} }}"),
                is_error: false,
            }],
        });
    }

    messages.push(Message {
        role: Role::Assistant,
        parts: vec![MessagePart::Text("All done.".to_owned())],
    });

    TranscriptFixture {
        messages,
        lines: calls * 2 + 2,
    }
}

/// Builds `turns` user/assistant exchanges, each assistant reply spanning
/// roughly `lines_per_turn` lines.
///
/// Every fifth turn's reply carries a fenced code block, so the fixture
/// exercises syntax highlighting the way a real session would. Every
/// seventh turn also carries a tool call with a matching result, so the
/// fixture exercises the settled tool-body cache too.
pub fn transcript(turns: usize, lines_per_turn: usize) -> TranscriptFixture {
    let mut messages = Vec::with_capacity(turns * 2);
    let mut lines = 0usize;

    for turn in 0..turns {
        messages.push(Message {
            role: Role::User,
            parts: vec![MessagePart::Text(format!("Prompt for turn {turn}."))],
        });

        let mut body = String::new();
        for line in 0..lines_per_turn {
            body.push_str(&format!("Line {line} of the response to turn {turn}.\n"));
        }
        lines += lines_per_turn;

        if turn % 5 == 0 {
            body.push_str("```rust\nfn example() -> u32 {\n    42\n}\n```\n");
            lines += 4;
        }

        messages.push(Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text(body)],
        });

        if turn % 7 == 0 {
            let call_id = format!("call-{turn}");
            messages.push(Message {
                role: Role::Assistant,
                parts: vec![MessagePart::ToolCall {
                    id: call_id.clone(),
                    name: "read".to_owned(),
                    input: "{\"path\":\"example.rs\"}".to_owned(),
                }],
            });
            messages.push(Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: call_id,
                    content: "fn example() -> u32 { 42 }".to_owned(),
                    is_error: false,
                }],
            });
            lines += 2;
        }
    }

    TranscriptFixture { messages, lines }
}

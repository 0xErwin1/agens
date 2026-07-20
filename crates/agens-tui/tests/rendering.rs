use std::time::Duration;

use agens_core::{Message, MessagePart, Role, TurnEvent, Usage};
use agens_tui::{
    ConversationEvent, DialogEntry, DialogView, DiffLine, DiffLineKind, Engine, Event, Key,
    PaletteEntry, PaletteEntryKind, RatatuiRenderer, Renderer, ToolResultState, Tui,
    TuiRuntimeEvent,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    style::{Color, Modifier},
};

#[derive(Default)]
struct FakeEngine;

impl Engine for FakeEngine {
    fn cancel(&mut self) {}
}

fn rendered_text(renderer: &RatatuiRenderer<TestBackend>) -> String {
    renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn cell_for_text<'a>(
    renderer: &'a RatatuiRenderer<TestBackend>,
    text: &str,
) -> &'a ratatui::buffer::Cell {
    let index = cell_index(renderer, text);
    &renderer.terminal().backend().buffer().content[index]
}

fn rendered_row(renderer: &RatatuiRenderer<TestBackend>, text: &str) -> usize {
    cell_index(renderer, text) / usize::from(renderer.terminal().backend().buffer().area.width)
}

fn rendered_column(renderer: &RatatuiRenderer<TestBackend>, text: &str) -> usize {
    cell_index(renderer, text) % usize::from(renderer.terminal().backend().buffer().area.width)
}

fn cell_index(renderer: &RatatuiRenderer<TestBackend>, text: &str) -> usize {
    let buffer = renderer.terminal().backend().buffer();
    let width = text.chars().count();
    buffer
        .content
        .windows(width)
        .position(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>() == text)
        .expect("text should be rendered")
}

#[test]
fn multiline_wrapped_user_message_uses_one_accented_identity() {
    let terminal = Terminal::new(TestBackend::new(44, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission(
        "A deliberately long user message wraps naturally in this narrow viewport.\nSecond source line.",
    );
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("answer".into())));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert_eq!(text.matches("You").count(), 1, "{text:?}");
    assert!(!text.contains("USER"), "{text:?}");
    assert!(text.contains("deliberately long user"), "{text:?}");
    assert!(text.contains("Second source line."), "{text:?}");
    let user = cell_for_text(&renderer, "You");
    assert_eq!(user.fg, Color::Cyan);
    assert!(user.modifier.contains(Modifier::BOLD));
}

#[test]
fn live_assistant_content_uses_the_user_body_column_at_normal_width() {
    assert_conversation_content_column(56, false);
}

#[test]
fn restored_assistant_content_uses_the_user_body_column_at_narrow_width() {
    assert_conversation_content_column(24, true);
}

fn assert_conversation_content_column(width: u16, restored: bool) {
    let terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    let content_width = usize::from(width - 4);
    let first_line = format!(
        "ASSISTANT_FIRST{}",
        "x".repeat(content_width - "ASSISTANT_FIRST".len())
    );
    let markdown = format!(
        "{first_line} ASSISTANT_WRAPPED\n\n```text\nASSISTANT_CODE\n```\n\n- ASSISTANT_LIST"
    );

    if restored {
        tui.replace_history(&[
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("USER_BODY".into())],
            },
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::Reasoning("THINKING_BODY".into()),
                    MessagePart::ToolCall {
                        id: "call-1".into(),
                        name: "native::read".into(),
                        input: "{}".into(),
                    },
                    MessagePart::Text(markdown),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: "TOOL_BODY".into(),
                    is_error: false,
                }],
            },
        ])
        .unwrap();
    } else {
        tui.begin_submission("USER_BODY");
        tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
            "THINKING_BODY".into(),
        )));
        tui.apply_progress(TurnEvent::ToolCallRequested {
            id: "call-1".into(),
            name: "native::read".into(),
            input: "{}".into(),
        });
        tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "call-1".into(),
            content: "TOOL_BODY".into(),
            is_error: false,
        }));
        tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(markdown)));
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    for content in [
        "USER_BODY",
        "ASSISTANT_FIRST",
        "ASSISTANT_WRAPPED",
        "ASSISTANT_CODE",
        "THINKING_BODY",
        "TOOL_BODY",
        "• ASSISTANT_LIST",
        "┌ native::read",
    ] {
        assert_eq!(
            rendered_column(&renderer, content),
            4,
            "{content}: {text:?}"
        );
    }
    assert_eq!(text.matches("You").count(), 1, "{text:?}");
    assert!(!text.contains("Assistant"), "{text:?}");
}

#[test]
fn thinking_renders_markdown_expanded_by_default_and_honors_collapse_setting() {
    let terminal = Terminal::new(TestBackend::new(64, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "**THOUGHTTOKEN**\n\n- inspect\n- verify".into(),
    )));
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("answer".into()));

    renderer.render(tui.view()).unwrap();
    let expanded = rendered_text(&renderer);
    assert_eq!(expanded.matches("Thinking").count(), 1, "{expanded:?}");
    assert!(!expanded.contains("THINKING"), "{expanded:?}");
    assert!(!expanded.contains("**"), "{expanded:?}");
    assert!(expanded.contains("THOUGHTTOKEN"), "{expanded:?}");
    assert!(
        cell_for_text(&renderer, "THOUGHTTOKEN")
            .modifier
            .contains(Modifier::BOLD)
    );

    tui.set_collapse_thinking(true);
    renderer.render(tui.view()).unwrap();
    let collapsed = rendered_text(&renderer);
    assert!(collapsed.contains("Thinking · collapsed"), "{collapsed:?}");
    assert!(!collapsed.contains("THOUGHTTOKEN"), "{collapsed:?}");
}

#[test]
fn local_info_renders_once_in_the_footer_without_a_conversation_row() {
    let terminal = Terminal::new(TestBackend::new(64, 16)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.apply_submission_outcome(agens_tui::TuiSubmissionOutcome::LocalInfo(
        "local-info-sentinel".into(),
    ));
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert_eq!(text.matches("local-info-sentinel").count(), 1, "{text:?}");
    assert!(tui.transcript().is_empty());
    assert!(tui.view().conversation.is_none());
}

#[test]
fn renderer_renders_practical_markdown_semantics() {
    let terminal = Terminal::new(TestBackend::new(72, 40)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "# Result\n\nUse **STRONGTOKEN** and *EMPHASISTOKEN* with `INLINE_TOKEN`.\n\n```rust\nfn example() {}\n```\n\n- first item\n- second item\n\n> quoted text\n\n[LINKTOKEN](https://example.com/docs)"
            .into(),
    )));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    for absent in [
        "ASSISTANT",
        "**",
        "*EMPHASISTOKEN*",
        "```",
        "`INLINE_TOKEN`",
    ] {
        assert!(!text.contains(absent), "found {absent:?} in {text:?}");
    }
    for expected in [
        "Result",
        "STRONGTOKEN",
        "EMPHASISTOKEN",
        "INLINE_TOKEN",
        "rust",
        "fn example() {}",
        "first item",
        "quoted text",
        "LINKTOKEN",
        "https://example.com/docs",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }

    assert!(
        cell_for_text(&renderer, "Result")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        cell_for_text(&renderer, "STRONGTOKEN")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        cell_for_text(&renderer, "EMPHASISTOKEN")
            .modifier
            .contains(Modifier::ITALIC)
    );
    assert_eq!(cell_for_text(&renderer, "INLINE_TOKEN").fg, Color::Yellow);
    let link = cell_for_text(&renderer, "LINKTOKEN");
    assert_eq!(link.fg, Color::Blue);
    assert!(link.modifier.contains(Modifier::UNDERLINED));
}

#[test]
fn streamed_and_final_markdown_share_one_stable_rendering_path() {
    let terminal = Terminal::new(TestBackend::new(64, 20)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    let markdown = "## Stable heading\n\nA **stable-answer-token**.";

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(markdown.into())));
    renderer.render(tui.view()).unwrap();
    let live = rendered_text(&renderer);
    let live_row = rendered_row(&renderer, "stable-answer-token");

    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed(markdown.into()));
    renderer.render(tui.view()).unwrap();
    let final_text = rendered_text(&renderer);

    assert_eq!(live.matches("stable-answer-token").count(), 1, "{live:?}");
    assert_eq!(
        final_text.matches("stable-answer-token").count(),
        1,
        "{final_text:?}"
    );
    assert_eq!(rendered_row(&renderer, "stable-answer-token"), live_row);
    assert!(!live.contains("##"), "{live:?}");
    assert!(!final_text.contains("**"), "{final_text:?}");
}

#[test]
fn renderer_projects_conversation_losslessly_by_call_id() {
    let backend = TestBackend::new(120, 50);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("review the patch");
    for event in [
        ConversationEvent::ReasoningDelta("inspect every changed line".into()),
        ConversationEvent::MarkdownDelta("stale live markdown".into()),
        ConversationEvent::MarkdownFinal("final **markdown**".into()),
        ConversationEvent::ToolCall {
            call_id: "read-1".into(),
            name: "native::read".into(),
            input: "src/render.rs".into(),
        },
        ConversationEvent::ToolCall {
            call_id: "write-2".into(),
            name: "native::write".into(),
            input: "src/render.rs".into(),
        },
        ConversationEvent::ToolResult {
            call_id: "write-2".into(),
            output: "write result".into(),
            is_error: false,
        },
        ConversationEvent::ToolResult {
            call_id: "read-1".into(),
            output: "```text\nread result\n```".into(),
            is_error: false,
        },
        ConversationEvent::Diff(vec![DiffLine::new(8, DiffLineKind::Added, "new line")]),
        ConversationEvent::Error {
            message: "Request failed safely".into(),
            action: "Check credentials and retry.".into(),
        },
    ] {
        tui.apply_conversation_event(event).unwrap();
    }
    tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
        call_id: "read-1".into(),
        duration: Some(Duration::from_millis(12)),
        result: ToolResultState::Success,
    });
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(3),
        output_tokens: Some(5),
        total_tokens: Some(8),
        context_window: Some(128),
    }));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    for expected in [
        "final markdown",
        "inspect every changed line",
        "native::read · read-1",
        "read result",
        "native::write · write-2",
        "write result",
        "12ms",
        "8 + new line",
        "tokens 8",
        "context 128",
        "Request failed safely",
        "Action: Check credentials and retry.",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }
    assert!(!text.contains("stale live markdown"), "{text:?}");
    assert!(!text.contains("**"), "{text:?}");
    assert!(!text.contains("```"), "{text:?}");
    assert_eq!(text.matches("Tools").count(), 1, "{text:?}");
    assert_eq!(text.matches("Error").count(), 1, "{text:?}");
    assert!(text.find("read-1").unwrap() < text.find("write-2").unwrap());
}

#[test]
fn lifecycle_metrics_render_in_footer_without_transcript_rows() {
    let backend = TestBackend::new(140, 24);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("answer".into())));
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        context_window: Some(8_192),
    }));
    tui.apply_runtime_event(TuiRuntimeEvent::TurnEnded {
        status: agens_core::TurnState::Completed,
        duration: Some(Duration::from_millis(25)),
    });

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(!text.contains("TURN"));
    assert!(!text.contains("USAGE"));
    assert!(text.contains("Completed"));
    assert!(text.contains("25ms"));
    assert!(text.contains("tokens 15"));
    assert!(text.contains("context 8192"));
}

#[test]
fn renderer_recovers_collapsed_long_tool_output_in_a_bounded_viewport() {
    let backend = TestBackend::new(48, 12);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "large.log".into(),
    })
    .unwrap();
    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "read-1".into(),
        output: format!("short preview\n{}", "full-output-sentinel ".repeat(12)),
        is_error: false,
    })
    .unwrap();
    tui.handle(Event::Key(Key::CtrlO));

    renderer.render(tui.view()).unwrap();
    let collapsed = rendered_text(&renderer);
    assert!(collapsed.contains("output collapsed"), "{collapsed:?}");
    assert!(!collapsed.contains("full-output-sentinel"), "{collapsed:?}");

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let expanded = rendered_text(&renderer);
    assert!(expanded.contains("full-output-sentinel"), "{expanded:?}");
}

#[test]
fn renderer_recovers_complete_long_output_through_production_scroll_offsets() {
    let backend = TestBackend::new(48, 12);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 48,
        height: 12,
    });

    tui.begin_submission("request");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "large.log".into(),
    })
    .unwrap();
    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "read-1".into(),
        output: format!(
            "output-start-sentinel\n{}\noutput-end-sentinel",
            (0..40)
                .map(|line| format!("output-middle-{line:02}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        is_error: false,
    })
    .unwrap();

    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("output-end-sentinel"));

    for _ in 0..100 {
        tui.handle(Event::Key(Key::PageUp));
        renderer.render(tui.view()).unwrap();
    }
    let mut traversal = rendered_text(&renderer);
    for _ in 0..100 {
        tui.handle(Event::Key(Key::PageDown));
        renderer.render(tui.view()).unwrap();
        traversal.push_str(&rendered_text(&renderer));
    }
    assert!(traversal.contains("output-start-sentinel"));
    assert!(rendered_text(&renderer).contains("output-end-sentinel"));
}

#[test]
fn renderer_retains_completed_turns_while_streaming_and_scrolling_the_next_turn() {
    let backend = TestBackend::new(52, 16);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 52,
        height: 16,
    });

    tui.begin_submission("first-user-sentinel");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "first-reasoning-sentinel".into(),
    )));
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "first-call".into(),
        name: "native::read".into(),
        input: "first-input".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "first-call".into(),
        content: "first-result-sentinel".into(),
        is_error: false,
    }));
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed(
        "first-answer-sentinel".into(),
    ));

    tui.begin_submission("second-user-sentinel");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "second-answer-sentinel".into(),
    )));

    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("second-answer-sentinel"));

    let mut history = rendered_text(&renderer);
    for _ in 0..30 {
        tui.handle(Event::Key(Key::PageUp));
        renderer.render(tui.view()).unwrap();
        history.push_str(&rendered_text(&renderer));
    }
    for expected in [
        "first-user-sentinel",
        "first-reasoning-sentinel",
        "first-call",
        "first-result-sentinel",
        "first-answer-sentinel",
    ] {
        assert!(history.contains(expected), "missing {expected:?}");
    }

    tui.handle(Event::Key(Key::CtrlO));
    let mut collapsed = String::new();
    for _ in 0..30 {
        tui.handle(Event::Key(Key::PageDown));
        renderer.render(tui.view()).unwrap();
        collapsed.push_str(&rendered_text(&renderer));
    }
    assert!(collapsed.contains("output collapsed"));
}

#[test]
fn restored_history_scroll_stays_fixed_while_streaming_and_end_resumes_follow() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(52, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    let mut messages = Vec::new();
    for turn in 0..12 {
        messages.push(Message {
            role: Role::User,
            parts: vec![MessagePart::Text(format!("restored-user-{turn:02}"))],
        });
        messages.push(Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text(format!("restored-answer-{turn:02}"))],
        });
    }
    tui.replace_history(&messages).unwrap();
    tui.begin_submission("live-user-sentinel");
    tui.handle(Event::Resize {
        width: 52,
        height: 14,
    });
    tui.handle(Event::Key(Key::ScrollUp));
    renderer.render(tui.view()).unwrap();
    let before = rendered_text(&renderer);
    assert!(before.contains("restored-user-11"), "{before:?}");
    assert!(before.contains("SCROLL"), "{before:?}");

    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        (0..20)
            .map(|line| format!("streaming-line-{line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )));
    renderer.render(tui.view()).unwrap();
    let streamed = rendered_text(&renderer);
    assert!(streamed.contains("restored-user-11"), "{streamed:?}");
    assert!(!tui.following_bottom());

    tui.handle(Event::Key(Key::Home));
    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("restored-user-00"));
    assert!(!tui.following_bottom());

    tui.handle(Event::Key(Key::End));
    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("streaming-line-19"));
    assert!(tui.following_bottom());
}

#[test]
fn restored_messages_render_every_turn_and_typed_part_in_persisted_order() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 50)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    let message = |role, parts| Message { role, parts };
    let text = |value: &str| vec![MessagePart::Text(value.into())];
    let messages = vec![
        message(Role::User, text("first user")),
        message(
            Role::Assistant,
            vec![
                MessagePart::Reasoning("first reasoning".into()),
                MessagePart::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: "{}".into(),
                },
                MessagePart::Text("first answer".into()),
            ],
        ),
        message(
            Role::Tool,
            vec![MessagePart::ToolResult {
                tool_call_id: "c1".into(),
                content: "first result".into(),
                is_error: false,
            }],
        ),
        message(Role::System, text("persisted reminder")),
        message(Role::User, text("second user")),
        message(Role::Assistant, text("second answer")),
    ];
    tui.replace_history(&messages).unwrap();
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    let order = "first user|first reasoning|read · c1|first answer|first result|persisted reminder|second user|second answer";
    let mut offset = 0;
    for expected in order.split('|') {
        let position = text[offset..].find(expected).expect(expected);
        offset += position + expected.len();
    }
    assert_eq!(text.matches("You").count(), 2, "{text:?}");
    assert_eq!(text.matches("Thinking").count(), 1, "{text:?}");
    for label in ["USER", "ASSISTANT", "THINKING"] {
        assert!(!text.contains(label), "found {label:?} in {text:?}");
    }
}

#[test]
fn renderer_sanitizes_runtime_errors_and_preserves_the_action() {
    let backend = TestBackend::new(120, 40);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_conversation_event(ConversationEvent::Error {
        message: "api_key=key-sentinel; Authorization: header-sentinel; path: /path-sentinel; prompt: prompt-sentinel".into(),
        action: "Retry after updating credentials.".into(),
    })
    .unwrap();

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    for secret in [
        "key-sentinel",
        "header-sentinel",
        "path-sentinel",
        "prompt-sentinel",
    ] {
        assert!(!text.contains(secret), "leaked {secret:?} in {text:?}");
    }
    assert!(text.contains("[redacted]"), "{text:?}");
    assert!(
        text.contains("Action: Retry after updating credentials."),
        "{text:?}"
    );
}

#[test]
fn renderer_clips_a_generic_dialog_inside_the_viewport() {
    let backend = TestBackend::new(42, 14);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.show_dialog(
        "Details",
        "A bounded dialog body that remains inside the viewport.",
    );
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("Details"), "{text:?}");
    assert!(text.contains("bounded dialog body"), "{text:?}");
}

#[test]
fn renderer_clips_selection_help_options_current_and_disabled_states_after_resize() {
    let backend = TestBackend::new(28, 8);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.show_selection_dialog(DialogView::selection(
        "Choose a model",
        Some("Up/Down navigate, Enter selects, Esc cancels"),
        vec![
            DialogEntry::action("gpt-4.1 (current)", "model:gpt-4.1"),
            DialogEntry::disabled("future-model", "Unavailable"),
            DialogEntry::action("o3", "model:o3"),
        ],
    ));

    tui.handle(Event::Resize {
        width: 28,
        height: 8,
    });
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("Choose a model"), "{text:?}");
    assert!(text.contains("gpt-4.1 (current)"), "{text:?}");
    assert!(text.contains("future-model"), "{text:?}");
    assert!(text.contains("disabled"), "{text:?}");
    assert!(!text.contains("model:gpt-4.1"), "{text:?}");
}

#[test]
fn long_selection_dialog_scrolls_each_input_and_keeps_selection_visible_after_resize() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(30, 8)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 30,
        height: 8,
    });
    tui.show_selection_dialog(DialogView::selection(
        "Choose",
        Some("Navigate"),
        (0..20)
            .map(|index| DialogEntry::action(format!("Option {index:02}"), format!("pick:{index}")))
            .collect(),
    ));

    for _ in 0..8 {
        tui.handle(Event::Key(Key::Down));
    }
    renderer.render(tui.view()).unwrap();
    let arrows = rendered_text(&renderer);
    assert!(arrows.contains("> Option 08"), "{arrows:?}");
    assert!(!arrows.contains("Option 00"), "{arrows:?}");

    tui.handle(Event::Key(Key::PageDown));
    renderer.render(tui.view()).unwrap();
    let page = rendered_text(&renderer);
    assert!(page.contains("> Option 11"), "{page:?}");

    tui.handle(Event::Key(Key::ScrollUp));
    renderer.render(tui.view()).unwrap();
    let wheel = rendered_text(&renderer);
    assert!(wheel.contains("> Option 10"), "{wheel:?}");

    tui.handle(Event::Resize {
        width: 24,
        height: 5,
    });
    renderer.render(tui.view()).unwrap();
    let resized = rendered_text(&renderer);
    assert!(resized.contains("> Option 10"), "{resized:?}");
}

#[test]
fn renderer_draws_a_bounded_palette_overlay_without_reflowing_the_conversation() {
    let backend = TestBackend::new(34, 10);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.add_info("conversation sentinel");
    tui.set_palette_entries(vec![
        PaletteEntry::new(
            "connect",
            "Connect to ChatGPT",
            "[--device-auth]",
            PaletteEntryKind::BuiltIn,
        ),
        PaletteEntry::new(
            "review",
            "Review the patch",
            "[scope]",
            PaletteEntryKind::Command,
        ),
        PaletteEntry::new(
            "resume",
            "Resume a session",
            "<id>",
            PaletteEntryKind::BuiltIn,
        ),
    ]);

    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("conversation sentinel"));
    let composer_row_before = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .position(|cell| cell.symbol() == "C")
        .unwrap()
        / 34;

    tui.handle(Event::Key(Key::Char('/')));
    tui.handle(Event::Key(Key::Char('r')));
    renderer.render(tui.view()).unwrap();
    let palette = rendered_text(&renderer);

    assert!(palette.contains("commands"), "{palette:?}");
    assert!(palette.contains("/review"), "{palette:?}");
    assert!(palette.contains("/resume"), "{palette:?}");
    assert!(!palette.contains("/connect"), "{palette:?}");
    let composer_row_after = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .position(|cell| cell.symbol() == "C")
        .unwrap()
        / 34;
    assert_eq!(composer_row_after, composer_row_before);

    tui.handle(Event::Key(Key::Escape));
    renderer.render(tui.view()).unwrap();
    assert!(tui.transcript().is_empty());
    assert!(tui.view().status.is_none());
}

#[test]
fn renderer_shows_complete_rich_turn_details_without_truncation() {
    let backend = TestBackend::new(120, 40);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("review the patch");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "inspect every changed line".into(),
    )));
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "live markdown stays visible".into(),
    )));
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "read-1".into(),
        name: "native::read".into(),
        input: "src/render.rs".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "read-1".into(),
        content: "first line\nsecond line".into(),
        is_error: false,
    }));
    tui.apply_runtime_event(TuiRuntimeEvent::ToolStarted {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "src/render.rs".into(),
    });
    tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
        call_id: "read-1".into(),
        duration: Some(Duration::from_millis(12)),
        result: ToolResultState::Success,
    });
    tui.apply_runtime_event(TuiRuntimeEvent::Diff {
        call_id: "read-1".into(),
        lines: vec![
            DiffLine::new(7, DiffLineKind::Removed, "old line"),
            DiffLine::new(8, DiffLineKind::Added, "new line"),
        ],
    });
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(3),
        output_tokens: Some(5),
        total_tokens: Some(8),
        context_window: Some(128),
    }));

    renderer.render(tui.view()).unwrap();
    let text = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    for expected in [
        "inspect every changed line",
        "live markdown stays visible",
        "native::read",
        "first line",
        "second line",
        "12ms",
        "7 - old line",
        "8 + new line",
        "tokens 8",
        "context 128",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }
}

#[test]
fn renderer_keeps_metrics_and_errors_readable_in_a_narrow_viewport() {
    let backend = TestBackend::new(42, 14);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.finish_submission(Err(
        "provider: request failed; retry after checking credentials".into(),
    ));
    tui.apply_runtime_event(TuiRuntimeEvent::TurnEnded {
        status: agens_core::TurnState::Failed,
        duration: Some(Duration::from_secs(2)),
    });

    renderer.render(tui.view()).unwrap();
    let text = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(
        text.contains("provider: request failed"),
        "error is missing: {text:?}"
    );
    assert!(
        text.contains("Action:"),
        "error action is missing: {text:?}"
    );
    assert!(text.contains("2s"), "turn duration is missing: {text:?}");
}

#[test]
fn renderer_scrolls_multiline_unicode_composer_and_keeps_cursor_visible() {
    let terminal = Terminal::new(TestBackend::new(30, 10)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    for character in "first\né🙂".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    renderer.render(tui.view()).unwrap();

    let cursor = renderer.terminal().backend().cursor_position();
    assert_eq!((cursor.x, cursor.y), (4, 7));
    assert!(rendered_text(&renderer).contains("2 lines · 8 chars"));

    let terminal = Terminal::new(TestBackend::new(5, 8)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    for character in "ab🙂".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    renderer.render(tui.view()).unwrap();
    let cursor = renderer.terminal().backend().cursor_position();
    assert!(
        cursor.x < 4,
        "cursor must remain inside the composer: {cursor:?}"
    );
}

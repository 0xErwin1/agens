use std::time::Duration;

use agens_core::{MessagePart, TurnEvent, Usage};
use agens_tui::{
    ConversationEvent, DiffLine, DiffLineKind, Engine, Event, Key, RatatuiRenderer, Renderer,
    ToolResultState, Tui, TuiRuntimeEvent,
};
use ratatui::{Terminal, backend::TestBackend};

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
            output: "read result".into(),
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
        "final **markdown**",
        "inspect every changed line",
        "read-1 native::read",
        "read result",
        "write-2 native::write",
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
    let mut recovered_start = false;
    for _ in 0..100 {
        tui.handle(Event::Key(Key::PageDown));
        renderer.render(tui.view()).unwrap();
        recovered_start |= rendered_text(&renderer).contains("output-start-sentinel");
    }
    assert!(
        recovered_start,
        "the scroll traversal never recovered the start"
    );
    assert!(rendered_text(&renderer).contains("output-end-sentinel"));
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

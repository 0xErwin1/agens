use agens_core::{SubagentErrorKind, SubagentStatus};
use std::time::Duration;

use agens_core::{Message, MessagePart, Role, TurnEvent, Usage};
use agens_tui::{
    Action, ConversationEvent, DialogEntry, DialogView, DiffLine, DiffLineKind, Engine, Event, Key,
    PaletteEntry, PaletteEntryKind, RatatuiRenderer, Renderer, SessionDialogCursor,
    SessionDialogRequest, ToolResultState, TranscriptId, Tui, TuiExecutionEvent, TuiExecutionState,
    TuiPresentation, TuiRuntimeEvent, TuiSubagentEvent, TuiSubmissionOutcome,
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

fn apply_subagent(tui: &mut Tui<FakeEngine>, event: TuiSubagentEvent) {
    tui.apply_runtime_event(TuiRuntimeEvent::SubagentExecution(event));
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

fn rendered_line(renderer: &RatatuiRenderer<TestBackend>, row: usize) -> String {
    let buffer = renderer.terminal().backend().buffer();
    let width = usize::from(buffer.area.width);
    buffer.content[row * width..(row + 1) * width]
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn rendered_column(renderer: &RatatuiRenderer<TestBackend>, text: &str) -> usize {
    cell_index(renderer, text) % usize::from(renderer.terminal().backend().buffer().area.width)
}

/// Columns of breathing room the bottom chrome keeps on both sides on terminals
/// wide enough to afford it. Every helper below assumes such a width.
const CHROME_GUTTER: u16 = 4;

fn composer_top_row(renderer: &RatatuiRenderer<TestBackend>) -> u16 {
    let buffer = renderer.terminal().backend().buffer();
    (0..buffer.area.height)
        .find(|row| buffer[(CHROME_GUTTER, *row)].symbol() == "┌")
        .expect("composer top border should be rendered")
}

fn composer_bottom_row(renderer: &RatatuiRenderer<TestBackend>) -> u16 {
    let buffer = renderer.terminal().backend().buffer();
    (0..buffer.area.height)
        .find(|row| buffer[(CHROME_GUTTER, *row)].symbol() == "└")
        .expect("composer bottom border should be rendered")
}

fn start_execution(tui: &mut Tui<FakeEngine>, id: u64, agent: &str) {
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: agent.into(),
        event: TuiExecutionEvent::ForegroundStarted { id },
    });
    apply_subagent(
        tui,
        TuiSubagentEvent::started(id, agent, "task", TuiExecutionState::ForegroundRunning),
    );
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
fn transcript_drag_selection_paints_exact_cells_and_preserves_original_text() {
    let terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("prompt");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "alpha café 🙂 omega".into(),
    )));
    tui.apply_progress(TurnEvent::StateChanged(agens_core::TurnState::Completed));
    renderer.render(tui.view()).unwrap();

    let row = rendered_row(&renderer, "café") as u16;
    let column = rendered_column(&renderer, "café") as u16;
    tui.handle(Event::MouseDown { column, row });
    tui.handle(Event::MouseDrag {
        column: column + 3,
        row,
    });
    tui.handle(Event::MouseUp {
        column: column + 3,
        row,
    });
    renderer.render(tui.view()).unwrap();

    assert_eq!(tui.selected_text(), Some("café"));
    for offset in 0..4 {
        assert_eq!(
            renderer.terminal().backend().buffer()[(column + offset, row)].bg,
            Color::Rgb(0x95, 0xe6, 0xcb)
        );
    }
    let rendered = rendered_text(&renderer);
    assert!(rendered.contains("alpha café"), "{rendered:?}");
    assert!(rendered.contains("omega"), "{rendered:?}");
}

#[test]
fn empty_composer_renders_a_complete_dock() {
    let width = 72_u16;
    let height = 14_u16;
    let mut renderer =
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    let tui = Tui::new(FakeEngine);

    renderer.render(tui.view()).unwrap();

    let buffer = renderer.terminal().backend().buffer();
    let composer_top = height - 6;
    let composer_bottom = height - 4;
    assert_eq!(buffer[(CHROME_GUTTER, composer_top)].symbol(), "┌");
    assert_eq!(
        buffer[(width - 1 - CHROME_GUTTER, composer_top)].symbol(),
        "┐"
    );
    assert_eq!(buffer[(CHROME_GUTTER, composer_bottom)].symbol(), "└");
    assert_eq!(
        buffer[(width - 1 - CHROME_GUTTER, composer_bottom)].symbol(),
        "┘"
    );
}

#[test]
fn bottom_chrome_bands_share_one_gutter_and_the_composer_keeps_both_edges_free() {
    let (width, height) = (120_u16, 24_u16);
    let mut renderer =
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width, height });
    tui.handle(Event::Key(Key::CtrlC));
    start_execution(&mut tui, 9, "explore");

    renderer.render(tui.view()).unwrap();
    let top = composer_top_row(&renderer);
    let bottom = composer_bottom_row(&renderer);
    let buffer = renderer.terminal().backend().buffer();

    assert_eq!(buffer[(CHROME_GUTTER, top)].symbol(), "┌");
    assert_eq!(buffer[(width - 1 - CHROME_GUTTER, top)].symbol(), "┐");
    assert_eq!(buffer[(CHROME_GUTTER, bottom)].symbol(), "└");
    assert_eq!(buffer[(width - 1 - CHROME_GUTTER, bottom)].symbol(), "┘");
    for row in top..=bottom {
        for column in 0..CHROME_GUTTER {
            assert_eq!(
                buffer[(column, row)].symbol(),
                " ",
                "left gutter must stay blank at column {column} row {row}"
            );
            assert_eq!(
                buffer[(width - 1 - column, row)].symbol(),
                " ",
                "right gutter must stay blank at column {} row {row}",
                width - 1 - column
            );
        }
    }

    assert_eq!(
        rendered_column(&renderer, "Tab focus"),
        usize::from(CHROME_GUTTER),
        "the subagent tree starts at the shared gutter"
    );
    assert_eq!(
        rendered_column(&renderer, "Press Ctrl+C again to exit"),
        usize::from(CHROME_GUTTER + 1),
        "the notice starts at the shared gutter plus its own leading space"
    );
    assert_eq!(
        rendered_row(&renderer, "model —") as u16,
        bottom,
        "the metadata rides the composer's bottom border"
    );
    assert_eq!(
        buffer[(width - 2 - CHROME_GUTTER, bottom)].symbol(),
        " ",
        "the metadata stops one column short of the closing corner"
    );
}

#[test]
fn responsive_layout_saturates_heights_one_through_six() {
    for height in 1_u16..=12 {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(40, height)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Key(Key::Char('x')));

        renderer.render(tui.view()).unwrap();
        let text = rendered_text(&renderer);

        assert_eq!(
            text.chars().count(),
            40 * usize::from(height),
            "height {height}"
        );
        assert!(!text.contains("Compose"), "height {height}: {text:?}");
        assert!(!text.contains("agens safe"), "height {height}: {text:?}");
        if height >= 12 {
            assert!(
                text.contains("Ready") || text.contains("gpt"),
                "height {height}: expected footer metrics: {text:?}"
            );
        }
    }

    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(40, 12)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    for character in (0..10)
        .map(|line| format!("line-{line:02}"))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
    {
        tui.handle(Event::Key(Key::Char(character)));
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    // Multiline composer input is still present without the old "N lines" chrome.
    assert!(text.contains("line-04"), "{text:?}");
    assert!(text.contains("line-09"), "{text:?}");
    assert!(!text.contains("10 lines"), "{text:?}");
}

#[test]
fn conversational_surface_uses_full_width_and_moves_context_to_footer() {
    for width in [80_u16, 100, 120] {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(width, 12)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.set_presentation("openai-api", "gpt-4.1", "session #42");
        tui.set_project("/home/iperez/dev/personal/agens");
        tui.set_reasoning_effort(Some("high"));
        tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
            input_tokens: Some(3),
            output_tokens: Some(5),
            total_tokens: Some(8),
            context_window: Some(128),
        }));

        renderer.render(tui.view()).unwrap();
        let text = rendered_text(&renderer);

        // Header no longer carries product/model/project chrome.
        assert!(!text.contains("agens safe"), "{text:?}");
        assert!(!text.contains("openai-api / gpt-4.1"), "{text:?}");
        if width == 120 {
            assert!(text.contains("gpt-4.1"), "footer model: {text:?}");
            assert!(text.contains("high"), "footer effort: {text:?}");
            assert!(text.contains("agens"), "footer project basename: {text:?}");
            assert!(text.contains("8/128"), "footer usage: {text:?}");
            assert!(text.contains('%'), "footer percent: {text:?}");
            assert!(text.contains("Ready"), "{text:?}");
            assert!(!text.contains("Enter send"), "{text:?}");
        }
    }
}

#[test]
fn footer_shows_compact_tokens_used_over_window_without_header_ctx() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.set_presentation("openai-api", "gpt-4.1", "session #1");
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        context_window: Some(8_192),
    }));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("15/8.2k"), "{text:?}");
    assert!(text.contains('%'), "{text:?}");
    assert!(!text.contains("ctx 15/8192"), "{text:?}");
    assert!(!text.contains("context 8192"), "{text:?}");
    assert!(!text.contains("unavailable"), "{text:?}");
}

#[test]
fn footer_keeps_five_fields_and_usage_across_submission_start() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_presentation(
        TuiPresentation::new("openai-api", "gpt-4.1", "session #1")
            .with_effort("high")
            .with_context_window(Some(200_000)),
    );
    tui.set_project("/home/iperez/dev/personal/agens");

    renderer.render(tui.view()).unwrap();
    let before_usage = rendered_text(&renderer);
    assert!(
        before_usage.contains("gpt-4.1 · high · 0/200k (0%) · agens · Ready"),
        "{before_usage:?}"
    );
    assert!(!before_usage.contains("model · default · ctx —"));

    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(70_000),
        output_tokens: Some(1_000),
        total_tokens: Some(71_000),
        context_window: None,
    }));
    tui.begin_submission("next turn");

    assert_eq!(
        tui.view().latest_usage.and_then(|usage| usage.total_tokens),
        Some(71_000)
    );
    renderer.render(tui.view()).unwrap();
    let next_turn = rendered_text(&renderer);
    assert!(
        next_turn.contains("gpt-4.1 · high · 71k/200k (36%) · agens"),
        "{next_turn:?}"
    );
}

#[test]
fn footer_uses_explicit_fallbacks_without_inventing_values() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 14)).unwrap());
    let tui = Tui::new(FakeEngine);

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("model — · effort — · ctx —"), "{text:?}");
    assert!(!text.contains("model · default · ctx —"), "{text:?}");
}

#[test]
fn active_status_glyph_advances_with_tick_and_idle_stays_static() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);

    tui.tick(Duration::from_millis(0));
    renderer.render(tui.view()).unwrap();
    let idle_early = rendered_text(&renderer);
    tui.tick(Duration::from_millis(80 * 5));
    renderer.render(tui.view()).unwrap();
    let idle_later = rendered_text(&renderer);

    assert!(idle_early.contains("Ready"), "{idle_early:?}");
    assert!(idle_later.contains("Ready"), "{idle_later:?}");
    assert!(!idle_early.contains("⠋"), "{idle_early:?}");
    assert!(!idle_later.contains("⠼"), "{idle_later:?}");
    assert_eq!(
        idle_early.matches('·').count(),
        idle_later.matches('·').count(),
        "idle chrome must not spin"
    );

    tui.begin_submission("glyph probe");
    tui.tick(Duration::from_millis(0));
    renderer.render(tui.view()).unwrap();
    let active_early = rendered_text(&renderer);
    tui.tick(Duration::from_millis(80));
    renderer.render(tui.view()).unwrap();
    let active_later = rendered_text(&renderer);

    assert!(active_early.contains("⠋"), "{active_early:?}");
    assert!(active_early.contains("Waiting"), "{active_early:?}");
    assert!(active_later.contains("⠙"), "{active_later:?}");
    assert!(active_later.contains("Waiting"), "{active_later:?}");
    assert_ne!(active_early, active_later);
}

#[test]
fn footer_shows_tokens_used_only_when_window_unknown_without_unavailable() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(4),
        output_tokens: Some(6),
        total_tokens: Some(10),
        context_window: None,
    }));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("10"), "{text:?}");
    assert!(!text.contains("10/"), "{text:?}");
    assert!(!text.contains("unavailable"), "{text:?}");
    assert!(!text.contains("context "), "{text:?}");
}

#[test]
fn typed_turn_blocks_group_tools_with_status_duration_and_preview() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 100)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect the workspace");

    for event in [
        ConversationEvent::ReasoningDelta("inspect typed events".into()),
        ConversationEvent::ToolCall {
            call_id: "read-1".into(),
            name: "native::read".into(),
            input: "src/lib.rs".into(),
            parsed: agens_core::ToolInput::Other {
                name: "native::read".into(),
                raw: "src/lib.rs".into(),
            },
        },
        ConversationEvent::ToolCall {
            call_id: "grep-2".into(),
            name: "native::grep".into(),
            input: "needle".into(),
            parsed: agens_core::ToolInput::Other {
                name: "native::grep".into(),
                raw: "needle".into(),
            },
        },
        ConversationEvent::ToolResult {
            call_id: "grep-2".into(),
            output: "grep preview".into(),
            is_error: true,
        },
        ConversationEvent::ToolResult {
            call_id: "read-1".into(),
            output: format!("read preview\n{}", "long-output ".repeat(400)),
            is_error: false,
        },
        ConversationEvent::Error {
            message: "safe failure".into(),
            action: "retry".into(),
        },
    ] {
        tui.apply_conversation_event(event).unwrap();
    }
    tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
        call_id: "read-1".into(),
        duration: Some(Duration::from_millis(12)),
        result: ToolResultState::Success,
    });
    tui.apply_runtime_event(TuiRuntimeEvent::ToolEnded {
        call_id: "grep-2".into(),
        duration: Some(Duration::from_secs(2)),
        result: ToolResultState::Failure,
    });
    tui.handle(Event::Key(Key::CtrlO));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert_eq!(text.matches("Tools · batch 1").count(), 1, "{text:?}");
    for expected in [
        "inspect the workspace",
        "inspect typed events",
        "src/lib.rs",
        "needle",
        "read preview",
        "grep preview",
        "Success · 12ms",
        "Failure · 2s",
        "visible output truncated",
        "safe failure",
        "Action: retry",
    ] {
        assert_eq!(text.matches(expected).count(), 1, "{expected}: {text:?}");
    }
    assert!(text.contains("read src/lib.rs"), "{text:?}");
    assert!(text.contains("grep needle"), "{text:?}");
    assert!(text.contains("└ read · Success"), "{text:?}");
    assert!(text.contains("└ grep · Failure"), "{text:?}");
    assert!(!text.contains("native::read · read-1"), "{text:?}");
    assert!(!text.contains("native::grep · grep-2"), "{text:?}");
}

#[test]
fn composer_dock_and_footer_degrade_without_detached_bands() {
    for (height, expects_footer, composer_y) in [(10, false, 7), (12, true, 8)] {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(72, height)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Key(Key::Char('d')));
        tui.add_info("compact-status-sentinel");

        renderer.render(tui.view()).unwrap();
        let text = rendered_text(&renderer);

        assert_eq!(
            text.matches("compact-status-sentinel").count(),
            1,
            "height {height}: {text:?}"
        );
        if expects_footer {
            assert!(
                text.contains("Ready") || text.contains("Responding"),
                "height {height}: expected operational footer: {text:?}"
            );
        }
        let _ = composer_y; // Compose title removed
        assert!(!text.contains("Compose"), "{text:?}");
        assert!(!text.contains("Enter send"), "{text:?}");
        assert!(!text.contains("F5"), "{text:?}");
        assert!(!text.contains("F6"), "{text:?}");
        assert!(!text.contains("TURN"), "{text:?}");
        assert!(!text.contains("USAGE"), "{text:?}");
    }
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

    assert!(text.contains('❯'), "{text:?}");
    assert!(!text.contains("USER"), "{text:?}");
    assert!(text.contains("deliberately long user"), "{text:?}");
    assert!(text.contains("Second source line."), "{text:?}");
    let user = cell_for_text(&renderer, "❯");
    assert_eq!(user.fg, Color::Rgb(0x73, 0xd0, 0xff));
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
    // Four columns of transcript margin plus the two-column shared row gutter.
    let content_width = usize::from(width - 6);
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
        // Finished restored history: thinking-first, then tools.
        tui.handle(Event::Key(Key::CtrlO));
        tui.handle(Event::Key(Key::CtrlO));
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
        // Live turn: Ctrl+O expands tools while thinking stays streaming-expanded.
        tui.handle(Event::Key(Key::CtrlO));
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains('❯'), "{text:?}");
    assert!(text.contains("USER_BODY"), "{text:?}");
    for content in [
        "ASSISTANT_FIRST",
        "ASSISTANT_WRAPPED",
        "THINKING_BODY",
        "TOOL_BODY",
        "• ASSISTANT_LIST",
        "read {}",
    ] {
        // Full-width transcript content shares a stable left margin.
        assert!(
            rendered_column(&renderer, content) <= 6,
            "{content} at col {}: {text:?}",
            rendered_column(&renderer, content)
        );
    }
    // A fenced body sits inside its own panel rail, which owns the content column.
    assert!(
        rendered_column(&renderer, "ASSISTANT_CODE") <= 8,
        "ASSISTANT_CODE at col {}: {text:?}",
        rendered_column(&renderer, "ASSISTANT_CODE")
    );
    assert!(!text.contains("Assistant"), "{text:?}");
}

#[test]
fn thinking_streams_expanded_auto_collapses_on_finish_and_ctrl_o_re_expands() {
    let terminal = Terminal::new(TestBackend::new(64, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "**THOUGHTTOKEN**\n\n- inspect\n- verify".into(),
    )));

    renderer.render(tui.view()).unwrap();
    let streaming = rendered_text(&renderer);
    assert_eq!(streaming.matches("Thinking").count(), 1, "{streaming:?}");
    assert!(!streaming.contains("Thought"), "{streaming:?}");
    assert!(streaming.contains("THOUGHTTOKEN"), "{streaming:?}");
    assert!(!streaming.contains("**"), "{streaming:?}");
    assert!(
        cell_for_text(&renderer, "THOUGHTTOKEN")
            .modifier
            .contains(Modifier::BOLD)
    );

    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("answer".into()));
    renderer.render(tui.view()).unwrap();
    let collapsed = rendered_text(&renderer);
    assert!(collapsed.contains("Thought"), "{collapsed:?}");
    assert!(!collapsed.contains("Thinking"), "{collapsed:?}");
    assert!(!collapsed.contains("THOUGHTTOKEN"), "{collapsed:?}");
    assert!(tui.view().collapse_thinking);

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let reexpanded = rendered_text(&renderer);
    assert!(reexpanded.contains("THOUGHTTOKEN"), "{reexpanded:?}");
    assert!(!reexpanded.contains("Thought"), "{reexpanded:?}");
    assert!(!tui.view().collapse_thinking);

    // Pin: a later finish path must not re-collapse user-expanded thinking.
    tui.set_running(true);
    tui.set_running(false);
    assert!(!tui.view().collapse_thinking);
    renderer.render(tui.view()).unwrap();
    let pinned = rendered_text(&renderer);
    assert!(pinned.contains("THOUGHTTOKEN"), "{pinned:?}");
}

#[test]
fn tool_rows_always_show_name_and_args_with_collapsed_finished_output() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "src/lib.rs".into(),
        parsed: agens_core::ToolInput::Other {
            name: "native::read".into(),
            raw: "src/lib.rs".into(),
        },
    })
    .unwrap();
    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "read-1".into(),
        output: "secret-tool-body-sentinel".into(),
        is_error: false,
    })
    .unwrap();

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(
        text.contains("┌ read") || text.contains(" read "),
        "{text:?}"
    );
    assert!(text.contains("src/lib.rs"), "{text:?}");
    assert!(text.contains("output collapsed"), "{text:?}");
    assert!(!text.contains("secret-tool-body-sentinel"), "{text:?}");
    assert!(
        !text.contains("native::read · read-1"),
        "call_id must stay off the scan path: {text:?}"
    );
    assert!(
        !text.contains("┌ read-1"),
        "call_id must not be the primary result label: {text:?}"
    );

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let expanded = rendered_text(&renderer);
    assert!(
        expanded.contains("secret-tool-body-sentinel"),
        "{expanded:?}"
    );
    assert!(expanded.contains("read"), "{expanded:?}");
    assert!(expanded.contains("src/lib.rs"), "{expanded:?}");
}

#[test]
fn typed_tool_headers_render_per_kind_and_keep_raw_arguments_behind_expand() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(110, 44)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    for (call_id, name, input, parsed) in [
        (
            "read-1",
            "native::read",
            "{\"path\":\"src/main.rs\"}",
            agens_core::ToolInput::Read {
                path: "src/main.rs".into(),
            },
        ),
        (
            "bash-1",
            "native::bash",
            "{\"command\":\"cargo test\"}",
            agens_core::ToolInput::Bash {
                command: "cargo test".into(),
            },
        ),
        (
            "grep-1",
            "native::grep",
            "{\"pattern\":\"needle\",\"path\":\"src\"}",
            agens_core::ToolInput::Grep {
                pattern: "needle".into(),
                path: Some("src".into()),
            },
        ),
        (
            "mcp-1",
            "mcp::foo__bar",
            "{\"path\":\"/etc/hosts-sentinel\",\"limit\":10}",
            agens_core::ToolInput::Other {
                name: "mcp::foo__bar".into(),
                raw: "{\"path\":\"/etc/hosts-sentinel\",\"limit\":10}".into(),
            },
        ),
    ] {
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            input: input.into(),
            parsed,
        })
        .unwrap();
        tui.apply_conversation_event(ConversationEvent::ToolResult {
            call_id: call_id.into(),
            output: "ok".into(),
            is_error: false,
        })
        .unwrap();
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    for header in [
        "Read src/main.rs",
        "$ cargo test",
        "Grep needle in src",
        "foo__bar {path, limit}",
    ] {
        assert!(text.contains(header), "missing {header:?}: {text:?}");
    }
    assert!(
        !text.contains("\"path\":"),
        "no raw JSON on the scan path: {text:?}"
    );
    assert!(!text.contains("/etc/hosts-sentinel"), "{text:?}");

    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let expanded = rendered_text(&renderer);
    assert!(
        expanded.contains("/etc/hosts-sentinel"),
        "full raw arguments stay reachable when expanded: {expanded:?}"
    );
}

#[test]
fn consecutive_reads_render_as_one_group_row_that_settles_into_past_tense() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: path.into(),
            name: "native::read".into(),
            input: "{}".into(),
            parsed: agens_core::ToolInput::Read { path: path.into() },
        })
        .unwrap();
    }

    tui.tick(Duration::from_millis(0));
    renderer.render(tui.view()).unwrap();
    let first_frame = rendered_text(&renderer);
    assert!(
        first_frame.contains("◈ Reading 3 files…"),
        "{first_frame:?}"
    );
    assert!(!first_frame.contains("Read src/a.rs"), "{first_frame:?}");
    assert_eq!(
        cell_for_text(&renderer, "◈").fg,
        Color::Rgb(0x73, 0xd0, 0xff),
        "a running group carries the active accent: {first_frame:?}"
    );

    let group_row = rendered_line(&renderer, rendered_row(&renderer, "◈ Reading 3 files…"));
    tui.tick(Duration::from_millis(200));
    renderer.render(tui.view()).unwrap();
    let second_frame = rendered_text(&renderer);
    assert_eq!(
        rendered_line(&renderer, rendered_row(&renderer, "◈ Reading 3 files…")),
        group_row,
        "a group row keeps one shape across ticks; state lives in its colour: {second_frame:?}"
    );

    for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        tui.apply_conversation_event(ConversationEvent::ToolResult {
            call_id: path.into(),
            output: "ok".into(),
            is_error: false,
        })
        .unwrap();
    }
    renderer.render(tui.view()).unwrap();
    let settled = rendered_text(&renderer);
    assert!(settled.contains("Read 3 files"), "{settled:?}");
    assert!(!settled.contains("Reading 3 files"), "{settled:?}");
    assert!(settled.contains("Ctrl+O to expand"), "{settled:?}");

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let expanded = rendered_text(&renderer);
    assert!(!expanded.contains("Read 3 files"), "{expanded:?}");
    for path in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        assert!(
            expanded.contains(&format!("Read {path}")),
            "expanding the group restores the individual rows: {expanded:?}"
        );
    }
}

/// Columns owned by the shared transcript gutter: the accent bar sits one column
/// left of the bullet at `CHROME_GUTTER`, its content two columns further right.
const BULLET_COLUMN: usize = CHROME_GUTTER as usize;
const CONTENT_COLUMN: usize = BULLET_COLUMN + 2;
const ACCENT_COLUMN: usize = BULLET_COLUMN - 1;

/// Transcript content rows, excluding the top border and the bottom scroll label.
fn transcript_rows(renderer: &RatatuiRenderer<TestBackend>) -> Vec<String> {
    let last = usize::from(composer_top_row(renderer)).saturating_sub(1);
    (1..last).map(|row| rendered_line(renderer, row)).collect()
}

fn first_glyph_column(row: &str) -> Option<usize> {
    row.char_indices()
        .find(|(_, glyph)| !glyph.is_whitespace())
        .map(|(index, _)| row[..index].chars().count())
}

#[test]
fn collapsed_thinking_occupies_exactly_one_row_and_names_the_finished_thought() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(72, 24)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "THOUGHTTOKEN body".into(),
    )));
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("answer".into()));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("Thought"), "{text:?}");
    assert!(
        !text.contains("collapsed"),
        "the collapsed state is the row itself, not a suffix: {text:?}"
    );
    assert!(!text.contains("THOUGHTTOKEN"), "{text:?}");

    let thought_row = rendered_row(&renderer, "Thought");
    assert_eq!(
        rendered_line(&renderer, thought_row).trim(),
        "Thought",
        "no duration is tracked for reasoning, so the bare form renders"
    );
    let rows = transcript_rows(&renderer);
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("Thought") || row.contains("Thinking"))
            .count(),
        1,
        "collapsed thinking is exactly one row: {rows:?}"
    );
}

#[test]
fn every_transcript_row_sits_on_the_shared_gutter() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "check the manifests".into(),
    )));
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "{}".into(),
        parsed: agens_core::ToolInput::Read {
            path: "Cargo.toml".into(),
        },
    })
    .unwrap();
    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "read-1".into(),
        output: "ok".into(),
        is_error: false,
    })
    .unwrap();
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "one focused workspace".into(),
    )));
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed(
        "one focused workspace".into(),
    ));

    renderer.render(tui.view()).unwrap();
    let rows = transcript_rows(&renderer);

    for row in &rows {
        let Some(column) = first_glyph_column(row) else {
            continue;
        };
        assert!(
            column == BULLET_COLUMN || column == CONTENT_COLUMN,
            "row {row:?} starts at column {column}, outside the shared gutter"
        );
    }
    assert_eq!(
        rendered_column(&renderer, "└"),
        BULLET_COLUMN,
        "the result footer rail shares the bullet column"
    );
    assert_eq!(
        rendered_column(&renderer, "Read Cargo.toml"),
        CONTENT_COLUMN
    );
    assert_eq!(rendered_column(&renderer, "◆"), BULLET_COLUMN);
}

#[test]
fn collapsed_tool_rows_pack_while_prose_keeps_exactly_one_blank_row() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    for (call_id, name, parsed) in [
        (
            "read-1",
            "native::read",
            agens_core::ToolInput::Read {
                path: "Cargo.toml".into(),
            },
        ),
        (
            "grep-1",
            "native::grep",
            agens_core::ToolInput::Grep {
                pattern: "needle".into(),
                path: None,
            },
        ),
    ] {
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            input: "{}".into(),
            parsed,
        })
        .unwrap();
        tui.apply_conversation_event(ConversationEvent::ToolResult {
            call_id: call_id.into(),
            output: "ok".into(),
            is_error: false,
        })
        .unwrap();
    }
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "PROSE_SENTINEL".into(),
    )));
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed(
        "PROSE_SENTINEL".into(),
    ));

    renderer.render(tui.view()).unwrap();
    let rows = transcript_rows(&renderer);

    let read_row = rendered_row(&renderer, "Read Cargo.toml");
    let grep_row = rendered_row(&renderer, "Grep needle");
    let prose_row = rendered_row(&renderer, "PROSE_SENTINEL");
    for row in read_row..grep_row {
        assert!(
            !rendered_line(&renderer, row).trim().is_empty(),
            "collapsed tool rows pack without a blank row: {rows:?}"
        );
    }
    assert!(
        rendered_line(&renderer, prose_row - 1).trim().is_empty(),
        "prose keeps one blank row above it: {rows:?}"
    );
    assert!(
        !rendered_line(&renderer, prose_row - 2).trim().is_empty(),
        "prose keeps exactly one blank row above it: {rows:?}"
    );
}

#[test]
fn transcript_bullets_carry_state_in_colour_and_group_headers_own_their_glyph() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 44)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "bash-1".into(),
        name: "native::bash".into(),
        input: "{}".into(),
        parsed: agens_core::ToolInput::Bash {
            command: "cargo check".into(),
        },
    })
    .unwrap();

    renderer.render(tui.view()).unwrap();
    assert_eq!(
        cell_for_text(&renderer, "◆").fg,
        Color::Rgb(0x73, 0xd0, 0xff),
        "a running activity row carries the active accent"
    );

    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "bash-1".into(),
        output: "boom".into(),
        is_error: true,
    })
    .unwrap();
    renderer.render(tui.view()).unwrap();
    assert_eq!(
        cell_for_text(&renderer, "◆").fg,
        Color::Rgb(0xf0, 0x71, 0x78),
        "a failed activity row carries the error colour"
    );

    for path in ["src/a.rs", "src/b.rs"] {
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: path.into(),
            name: "native::read".into(),
            input: "{}".into(),
            parsed: agens_core::ToolInput::Read { path: path.into() },
        })
        .unwrap();
    }
    for (path, is_error) in [("src/a.rs", false), ("src/b.rs", true)] {
        tui.apply_conversation_event(ConversationEvent::ToolResult {
            call_id: path.into(),
            output: "ok".into(),
            is_error,
        })
        .unwrap();
    }
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("Read 2 files"), "{text:?}");
    assert!(
        text.contains("· 1 failed"),
        "a folded group names its failures: {text:?}"
    );
    assert_eq!(rendered_column(&renderer, "◈"), BULLET_COLUMN);
    assert_eq!(
        cell_for_text(&renderer, "◈").fg,
        Color::Rgb(0xf0, 0x71, 0x78),
        "a group with a failure carries the error colour"
    );
}

#[test]
fn a_running_row_carries_an_accent_bar_left_of_its_bullet_without_shifting_content() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "bash-1".into(),
        name: "native::bash".into(),
        input: "{}".into(),
        parsed: agens_core::ToolInput::Bash {
            command: "cargo check".into(),
        },
    })
    .unwrap();

    tui.tick(Duration::from_millis(0));
    renderer.render(tui.view()).unwrap();
    let first = rendered_text(&renderer);
    assert_eq!(
        rendered_column(&renderer, "┃"),
        ACCENT_COLUMN,
        "the accent bar owns one column left of the bullet: {first:?}"
    );
    assert_eq!(rendered_column(&renderer, "◆"), BULLET_COLUMN);
    assert_eq!(rendered_column(&renderer, "$ cargo check"), CONTENT_COLUMN);
    assert_eq!(
        cell_for_text(&renderer, "┃").fg,
        Color::Rgb(0x73, 0xd0, 0xff),
        "a running bar opens on the full active accent: {first:?}"
    );

    let bar_row = rendered_row(&renderer, "┃");
    let shape = rendered_line(&renderer, bar_row);
    tui.tick(Duration::from_millis(240));
    renderer.render(tui.view()).unwrap();
    assert_eq!(
        rendered_line(&renderer, bar_row),
        shape,
        "the row keeps one shape across ticks; only the accent colour moves"
    );
    assert_ne!(
        cell_for_text(&renderer, "┃").fg,
        Color::Rgb(0x73, 0xd0, 0xff),
        "the running bar breathes with the shared tick clock"
    );
}

#[test]
fn finished_read_rows_drop_the_accent_bar_while_a_folded_group_dims_it() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    tui.apply_conversation_event(ConversationEvent::ToolCall {
        call_id: "read-1".into(),
        name: "native::read".into(),
        input: "{}".into(),
        parsed: agens_core::ToolInput::Read {
            path: "Cargo.toml".into(),
        },
    })
    .unwrap();
    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "read-1".into(),
        output: "ok".into(),
        is_error: false,
    })
    .unwrap();
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("done".into()));

    renderer.render(tui.view()).unwrap();
    let settled = rendered_text(&renderer);
    let read_row = rendered_row(&renderer, "Read Cargo.toml");
    let read_line = rendered_line(&renderer, read_row);
    assert!(
        !settled.contains('┃') && !settled.contains('❙'),
        "a plain finished read carries no accent bar: {settled:?}"
    );
    assert_eq!(
        read_line.chars().nth(ACCENT_COLUMN),
        Some(' '),
        "the accent column stays blank: {read_line:?}"
    );
    assert_eq!(
        rendered_column(&renderer, "Read Cargo.toml"),
        CONTENT_COLUMN
    );

    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("inspect");
    for path in ["src/a.rs", "src/b.rs"] {
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: path.into(),
            name: "native::read".into(),
            input: "{}".into(),
            parsed: agens_core::ToolInput::Read { path: path.into() },
        })
        .unwrap();
        tui.apply_conversation_event(ConversationEvent::ToolResult {
            call_id: path.into(),
            output: "ok".into(),
            is_error: false,
        })
        .unwrap();
    }
    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed("done".into()));
    renderer.render(tui.view()).unwrap();
    let folded = rendered_text(&renderer);

    assert!(folded.contains("Read 2 files"), "{folded:?}");
    assert_eq!(
        rendered_column(&renderer, "❙"),
        ACCENT_COLUMN,
        "a collapsed groupable run keeps the thin bar in the accent column: {folded:?}"
    );
    let Color::Rgb(red, green, blue) = cell_for_text(&renderer, "❙").fg else {
        panic!("the accent bar is painted in RGB: {folded:?}");
    };
    assert!(
        red < 0xaa && green < 0xd9 && blue < 0x4c,
        "the collapsed bar is dimmed against the group's own colour: {red:x} {green:x} {blue:x}"
    );
}

#[test]
fn accented_rows_render_without_panicking_on_small_terminals() {
    for (width, height) in [(1_u16, 1_u16), (2, 3), (6, 5), (10, 8), (24, 12), (40, 20)] {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Resize { width, height });
        tui.begin_submission("inspect");
        tui.apply_conversation_event(ConversationEvent::ToolCall {
            call_id: "bash-1".into(),
            name: "native::bash".into(),
            input: "{}".into(),
            parsed: agens_core::ToolInput::Bash {
                command: "cargo check".into(),
            },
        })
        .unwrap();
        tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
            "thinking body".into(),
        )));
        tui.tick(Duration::from_millis(240));

        renderer.render(tui.view()).unwrap();
        assert_eq!(
            rendered_text(&renderer).chars().count(),
            usize::from(width) * usize::from(height),
            "{width}x{height}"
        );
    }
}

#[test]
fn no_header_row_and_the_working_indicator_lives_at_the_end_of_the_chat() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 30)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.begin_submission("do work");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "assistant body".into(),
    )));
    tui.apply_runtime_event(TuiRuntimeEvent::Usage(Usage {
        input_tokens: Some(3_000),
        output_tokens: Some(400),
        total_tokens: Some(3_400),
        context_window: Some(200_000),
    }));
    tui.tick(Duration::from_secs(12));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    assert!(
        rendered_line(&renderer, 0)
            .chars()
            .all(|glyph| glyph == '─'),
        "the first row is transcript content chrome, not a header strip: {:?}",
        rendered_line(&renderer, 0)
    );
    assert!(text.contains("12s"), "{text:?}");
    assert!(text.contains("3400 tok"), "{text:?}");

    let body_row = rendered_row(&renderer, "assistant body");
    let indicator_row = rendered_row(&renderer, "3400 tok");
    let footer_row = rendered_row(&renderer, "model —");
    assert!(
        body_row < indicator_row && indicator_row < footer_row,
        "the working indicator sits inside the chat: {body_row} {indicator_row} {footer_row}"
    );

    tui.set_running(false);
    renderer.render(tui.view()).unwrap();
    let idle = rendered_text(&renderer);
    assert!(
        !idle.contains("3400 tok"),
        "the working indicator disappears when idle: {idle:?}"
    );
}

#[test]
fn subagent_tree_renders_below_the_composer_and_owns_the_navigation_hints() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 50)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 100,
        height: 50,
    });
    tui.begin_submission("delegate");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "assistant body".into(),
    )));
    for (id, agent) in [(9_u64, "explore"), (10, "plan")] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(id, agent, "task", TuiExecutionState::ForegroundRunning),
        );
    }
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::tool_call(9, "read-1", "native::read", "secret-child-input"),
    );
    tui.tick(Duration::from_secs(13));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    assert!(text.contains("├─"), "{text:?}");
    assert!(text.contains("└─"), "{text:?}");
    assert!(text.contains("Explore #9"), "{text:?}");
    assert!(text.contains("Plan #10"), "{text:?}");
    assert!(
        text.contains("└─ ● Read files"),
        "the focused branch expands into its live child activity: {text:?}"
    );
    assert!(!text.contains("secret-child-input"), "{text:?}");

    assert_eq!(
        text.matches("Tab focus · Enter inspect · Ctrl+B background")
            .count(),
        1,
        "the navigation hints live with the tree only: {text:?}"
    );
    let body_row = rendered_row(&renderer, "assistant body");
    let tree_row = rendered_row(&renderer, "Tab focus");
    let footer_row = rendered_row(&renderer, "model —");
    assert!(
        body_row < footer_row && footer_row < tree_row,
        "the tree sits below the composer border that carries the metadata: {body_row} {footer_row} {tree_row}"
    );

    let branch_row = rendered_row(&renderer, "Explore #9");
    tui.handle(Event::MouseDown {
        column: 4,
        row: u16::try_from(branch_row).unwrap(),
    });
    assert_eq!(
        tui.view().active_transcript,
        TranscriptId::Subagent(9),
        "clicking a tree branch navigates to that subagent"
    );
}

/// Long single-line summaries are the transcript's only unwrappable rows, so a
/// narrow terminal must see them elided rather than sliced mid-word.
#[test]
fn narrow_terminals_elide_long_summaries_on_a_word_boundary_instead_of_slicing_them() {
    const LONG_TASK: &str = "Investiga este proyecto sin modificar archivos. Revisa la \
         estructura, tecnologias usadas, puntos de entrada, scripts disponibles.";

    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(40, 30)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 40,
        height: 30,
    });
    tui.begin_submission("delegate");
    for (id, agent, task) in [
        (9_u64, "explore", LONG_TASK),
        (10, "deep-research-explorer", "task"),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(id, agent, task, TuiExecutionState::ForegroundRunning),
        );
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    assert!(
        text.contains("Explore · Investiga este proyecto…"),
        "the card keeps a first-sentence title elided on a word boundary: {text:?}"
    );
    assert!(
        !text.contains("Revisa"),
        "the card never dumps the rest of the prompt: {text:?}"
    );
    assert!(
        text.contains("Deep-research-explorer #10…"),
        "a tree branch label is elided instead of sliced: {text:?}"
    );
    assert!(
        text.contains("Tab focus · Enter inspect…"),
        "the tree affordance row is elided instead of sliced: {text:?}"
    );
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
fn fenced_code_block_chrome_does_not_nest_body_gutter_on_header() {
    let terminal = Terminal::new(TestBackend::new(56, 16)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "Use:\n\n```bash\nagens chat \"hello\"\n```\n".into(),
    )));
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("╭─ bash"), "{text:?}");
    assert!(text.contains("agens chat"), "{text:?}");
    assert!(text.contains('╰'), "{text:?}");
    // Header must not nest the body gutter into chrome.
    assert!(!text.contains("│ ╭"), "{text:?}");
    assert!(!text.contains("│╭"), "{text:?}");
    // Body lines keep a single gutter cell; rails join ╭─ / │ / ╰─.
    assert!(text.contains("│ agens chat"), "{text:?}");
    // Full-width rule continues past the language label.
    assert!(
        text.contains("╭─ bash ─") || text.matches('─').count() >= 8,
        "expected continuous header rule: {text:?}"
    );
}

#[test]
fn fenced_code_block_is_compact_and_has_no_trailing_empty_body_row() {
    let width = 48_u16;
    let terminal = Terminal::new(TestBackend::new(width, 14)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "```bash\nshort\n```\n".into(),
    )));
    renderer.render(tui.view()).unwrap();

    let header_row = rendered_row(&renderer, "╭─ bash");
    let body_row = rendered_row(&renderer, "short");
    let footer_row = rendered_row(&renderer, "╰");
    assert_eq!(body_row, header_row + 1);
    assert_eq!(footer_row, body_row + 1);

    let header = rendered_line(&renderer, header_row);
    let body = rendered_line(&renderer, body_row);
    let footer = rendered_line(&renderer, footer_row);
    assert!(header.contains("╭─ bash ╮"), "{header:?}");
    assert!(body.contains("│ short │"), "{body:?}");
    assert!(footer.contains("╰───────╯"), "{footer:?}");

    let borders =
        [(&header, '╭', '╮'), (&body, '│', '│'), (&footer, '╰', '╯')].map(|(line, left, right)| {
            let left = line
                .chars()
                .position(|character| character == left)
                .expect("left code border");
            let right = line
                .chars()
                .enumerate()
                .filter_map(|(index, character)| (character == right).then_some(index))
                .last()
                .expect("right code border");
            (left, right, right - left + 1)
        });
    assert!(
        borders.iter().all(|border| *border == borders[0]),
        "borders={borders:?} header={header:?} body={body:?} footer={footer:?}"
    );
    assert!(borders[0].1 < usize::from(width - 1));

    let buffer = renderer.terminal().backend().buffer();
    let panel = Color::Rgb(0x1a, 0x1f, 0x29);
    let panel_cells = (0..buffer.area.width)
        .filter(|x| buffer[(*x, body_row as u16)].bg == panel)
        .count();
    assert_eq!(panel_cells, 9);
}

#[test]
fn fenced_javascript_uses_token_specific_foregrounds() {
    let terminal = Terminal::new(TestBackend::new(72, 18)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "```js\nconst answer = \"value\"; // note\nlet count = 42;\n```\n".into(),
    )));
    tui.apply_progress(TurnEvent::StateChanged(agens_core::TurnState::Completed));
    renderer.render(tui.view()).unwrap();

    let colors =
        ["const", "\"value\"", "// note", "42"].map(|token| cell_for_text(&renderer, token).fg);
    let unique = colors.iter().fold(Vec::new(), |mut unique, color| {
        if !unique.contains(color) {
            unique.push(*color);
        }
        unique
    });

    assert!(unique.len() >= 3, "foregrounds: {colors:?}");
    assert!(
        colors
            .iter()
            .any(|color| *color != Color::Rgb(0xaa, 0xd9, 0x4c)),
        "code block stayed uniformly success-green: {colors:?}"
    );
}

#[test]
fn unknown_fence_language_uses_neutral_panel_style() {
    let terminal = Terminal::new(TestBackend::new(48, 14)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "```not-a-language\nmystery_token 42\n```\n".into(),
    )));
    renderer.render(tui.view()).unwrap();

    let cell = cell_for_text(&renderer, "mystery_token");
    assert_eq!(cell.fg, Color::Rgb(0xbf, 0xbd, 0xb6));
    assert_eq!(cell.bg, Color::Rgb(0x1a, 0x1f, 0x29));
}

#[test]
fn paragraph_to_fence_has_one_blank_transition_row() {
    let terminal = Terminal::new(TestBackend::new(48, 20)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "Lead paragraph.\n\n```bash\nshort\n```\n".into(),
    )));
    renderer.render(tui.view()).unwrap();

    let paragraph_row = rendered_row(&renderer, "Lead paragraph.");
    let fence_row = rendered_row(&renderer, "╭─ bash");
    assert_eq!(fence_row, paragraph_row + 2);
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
    // Emphasis is weight, slant and underline: prose never acquires a hue.
    let body = Color::Rgb(0xbf, 0xbd, 0xb6);
    let inline_code = cell_for_text(&renderer, "INLINE_TOKEN");
    assert_eq!(inline_code.fg, body);
    assert_eq!(
        inline_code.bg,
        Color::Rgb(0x1a, 0x1f, 0x29),
        "inline code is set apart by its panel, not by a colour"
    );
    let link = cell_for_text(&renderer, "LINKTOKEN");
    assert_eq!(link.fg, body);
    assert!(link.modifier.contains(Modifier::UNDERLINED));
    assert_eq!(cell_for_text(&renderer, "STRONGTOKEN").fg, body);
    assert_eq!(cell_for_text(&renderer, "Result").fg, body);
    assert!(
        cell_for_text(&renderer, "Result")
            .modifier
            .contains(Modifier::UNDERLINED),
        "H1 should underline for hierarchy"
    );
    assert_eq!(cell_for_text(&renderer, "EMPHASISTOKEN").fg, body);
    assert_eq!(
        cell_for_text(&renderer, "•").fg,
        Color::Rgb(0x5c, 0x67, 0x73),
        "list markers are chrome"
    );
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
    // Idle header collapses after the turn ends (reclaims one row); markdown path stays stable.
    let final_row = rendered_row(&renderer, "stable-answer-token");
    assert!(
        final_row == live_row || final_row + 1 == live_row,
        "live_row={live_row} final_row={final_row}"
    );
    assert!(!live.contains("##"), "{live:?}");
    assert!(!final_text.contains("**"), "{final_text:?}");
}

#[test]
fn completed_turn_collapses_duplicated_live_stream_into_one_assistant_body() {
    let terminal = Terminal::new(TestBackend::new(72, 16)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    let once = "hola-unique-token";

    tui.begin_submission("request");
    // Simulate dual progress emission of the same completed body.
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(once.into())));
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(once.into())));
    renderer.render(tui.view()).unwrap();
    assert_eq!(
        rendered_text(&renderer).matches(once).count(),
        2,
        "precondition: live path still shows both deltas"
    );

    tui.finish_provider_turn(agens_tui::TuiProviderOutcome::Completed(once.into()));
    renderer.render(tui.view()).unwrap();
    let final_text = rendered_text(&renderer);
    assert_eq!(
        final_text.matches(once).count(),
        1,
        "Completed must heal exact dual-progress duplication: {final_text:?}"
    );
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
            parsed: agens_core::ToolInput::Other {
                name: "native::read".into(),
                raw: "src/render.rs".into(),
            },
        },
        ConversationEvent::ToolCall {
            call_id: "write-2".into(),
            name: "native::write".into(),
            input: "src/render.rs".into(),
            parsed: agens_core::ToolInput::Other {
                name: "native::write".into(),
                raw: "src/render.rs".into(),
            },
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

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    for expected in [
        "final markdown",
        "inspect every changed line",
        "read",
        "src/render.rs",
        "read result",
        "write",
        "write result",
        "12ms",
        "new line",
        "8/128",
        "Request failed safely",
        "Action: Check credentials and retry.",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }
    assert!(!text.contains("unavailable"), "{text:?}");
    assert!(!text.contains("context 128"), "{text:?}");
    assert!(!text.contains("stale live markdown"), "{text:?}");
    assert!(!text.contains("**"), "{text:?}");
    assert!(!text.contains("```"), "{text:?}");
    assert!(!text.contains("native::read · read-1"), "{text:?}");
    assert!(!text.contains("native::write · write-2"), "{text:?}");
    assert_eq!(text.matches("Tools").count(), 1, "{text:?}");
    assert_eq!(text.matches("Error").count(), 1, "{text:?}");
    assert!(text.find("read src/render.rs").unwrap() < text.find("write src/render.rs").unwrap());
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
    assert!(text.contains("15/8.2k"), "{text:?}");
    assert!(!text.contains("context 8192"), "{text:?}");
    assert!(!text.contains("unavailable"), "{text:?}");
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
        parsed: agens_core::ToolInput::Other {
            name: "native::read".into(),
            raw: "large.log".into(),
        },
    })
    .unwrap();
    tui.apply_conversation_event(ConversationEvent::ToolResult {
        call_id: "read-1".into(),
        output: format!("short preview\n{}", "full-output-sentinel ".repeat(12)),
        is_error: false,
    })
    .unwrap();
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
        parsed: agens_core::ToolInput::Other {
            name: "native::read".into(),
            raw: "large.log".into(),
        },
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

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("output-end-sentinel"));

    let mut traversal = String::new();
    for _ in 0..100 {
        tui.handle(Event::Key(Key::ScrollUp));
        renderer.render(tui.view()).unwrap();
        traversal.push_str(&rendered_text(&renderer));
    }
    for _ in 0..100 {
        tui.handle(Event::Key(Key::ScrollDown));
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
        "read",
        "first-input",
        "first-result-sentinel",
        "first-answer-sentinel",
    ] {
        assert!(history.contains(expected), "missing {expected:?}");
    }
    assert!(!history.contains("first-call"), "{history:?}");

    // While the next turn is streaming, Ctrl+O targets tools (thinking is live).
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
    // Thinking-first then tools for restored finished history.
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    let order = "first user|first reasoning|read|first answer|first result|persisted reminder|second user|second answer";
    let mut offset = 0;
    for expected in order.split('|') {
        let position = text[offset..].find(expected).expect(expected);
        offset += position + expected.len();
    }
    assert!(!text.contains("read · c1"), "{text:?}");
    assert_eq!(text.matches('❯').count(), 2, "{text:?}");
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

    assert!(
        text.contains("─ Details"),
        "title sits in the border: {text:?}"
    );
    assert!(text.contains("bounded dialog body"), "{text:?}");
    assert!(text.contains("close"), "derived footer: {text:?}");
}

#[test]
fn diagnostics_dialog_wraps_a_long_line_instead_of_clipping_later_diagnostics() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(60, 12)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize {
        width: 60,
        height: 12,
    });

    tui.add_diagnostic(
        "6 skills share a command name; /name runs the command, the skill tool still loads them: sdd-apply, sdd-verify",
    );
    tui.add_diagnostic("Command /shared has multiple definitions; applied source precedence.");
    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(
        text.contains("sdd-verify"),
        "tail of a wrapped line: {text:?}"
    );
    assert!(
        text.contains("precedence"),
        "a later diagnostic keeps its own rows: {text:?}"
    );
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

    assert!(text.contains("─ Choose a model"), "{text:?}");
    assert!(text.contains("▌ gpt-4.1 (current)"), "{text:?}");
    // 28 columns leave 11 label cells once the badge is placed.
    assert!(text.contains("disabled future-mod"), "{text:?}");
    assert!(text.contains("navigate"), "derived footer: {text:?}");
    assert!(text.contains("select"), "derived footer: {text:?}");
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
    assert!(arrows.contains("▌ Option 08"), "{arrows:?}");
    assert!(!arrows.contains("Option 00"), "{arrows:?}");
    assert!(arrows.contains("█") || arrows.contains("│"), "{arrows:?}");

    tui.handle(Event::Key(Key::PageDown));
    renderer.render(tui.view()).unwrap();
    let page = rendered_text(&renderer);
    assert!(page.contains("▌ Option 11"), "{page:?}");

    tui.handle(Event::Key(Key::ScrollUp));
    renderer.render(tui.view()).unwrap();
    let wheel = rendered_text(&renderer);
    assert!(wheel.contains("▌ Option 10"), "{wheel:?}");

    tui.handle(Event::Resize {
        width: 24,
        height: 5,
    });
    renderer.render(tui.view()).unwrap();
    let resized = rendered_text(&renderer);
    assert!(resized.contains("▌ Option 10"), "{resized:?}");

    tui.handle(Event::Key(Key::Char('/')));
    tui.handle(Event::Key(Key::Char('1')));
    for _ in 0..10 {
        tui.handle(Event::Key(Key::PageDown));
    }
    renderer.render(tui.view()).unwrap();
    let filtered = rendered_text(&renderer);
    assert!(filtered.contains("/ 1"), "{filtered:?}");
    assert!(filtered.contains("▌ Option 19"), "{filtered:?}");
    assert!(!filtered.contains("Option 08"), "{filtered:?}");
}

#[test]
fn session_dialog_renders_scope_hints_rows_details_and_distinct_empty_states() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(78, 16)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    let current = DialogEntry::action_with_metadata(
        "#7 Alpha",
        "2 turns · 5m ago",
        "7 Alpha /work/alpha primary",
        "ID: 7 · Alpha\nTurns: 2 · Agent: primary\nUpdated: 100 (5m ago)",
        "session:7",
    );
    let other = DialogEntry::action_with_metadata(
        "#9 Beta",
        "4 turns · 1h ago",
        "9 Beta /work/beta reviewer",
        "ID: 9 · Beta\nTurns: 4 · Agent: reviewer\nUpdated: 90 (1h ago) · Root: /work/beta",
        "session:9",
    );
    tui.show_selection_dialog(DialogView::sessions_page(
        vec![current.clone()],
        SessionDialogRequest::initial(),
        Some(SessionDialogCursor::new(100, 7)),
    ));

    renderer.render(tui.view()).unwrap();
    let project = rendered_text(&renderer);
    assert!(
        project.contains("Resume session · Current project"),
        "{project:?}"
    );
    assert!(project.contains("ctrl+a all projects"), "{project:?}");
    assert!(project.contains("▌ #7 Alpha"), "{project:?}");
    assert!(project.contains("2 turns · 5m ago"), "{project:?}");
    assert!(project.contains("page 1 · more"), "{project:?}");
    assert!(
        !project.contains("Type to search |"),
        "session help prose lives in the footer now: {project:?}"
    );
    assert!(!project.contains("Agent: primary"), "{project:?}");
    assert!(!project.contains("Updated: 100"), "{project:?}");
    assert!(!project.contains("#9 Beta"), "{project:?}");

    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let details = rendered_text(&renderer);
    assert!(details.contains("Agent: primary"), "{details:?}");
    assert!(details.contains("Updated: 100 (5m ago)"), "{details:?}");

    let Action::LoadSessionPage(global_request) = tui.handle(Event::Key(Key::LineStart)) else {
        panic!("scope toggle should request a page");
    };
    tui.apply_submission_outcome(TuiSubmissionOutcome::Dialog(DialogView::sessions_page(
        vec![current, other],
        global_request,
        None,
    )));
    renderer.render(tui.view()).unwrap();
    let global = rendered_text(&renderer);
    assert!(
        global.contains("Resume session · All projects"),
        "{global:?}"
    );
    assert!(global.contains("ctrl+a current project"), "{global:?}");
    assert!(global.contains("4 turns · 1h ago"), "{global:?}");
    assert!(!global.contains("root=/work/beta"), "{global:?}");
    assert!(!global.contains("Agent: primary"), "{global:?}");

    let mut search_request = None;
    tui.handle(Event::Key(Key::Char('/')));
    for character in "missing".chars() {
        let Action::LoadSessionPage(request) = tui.handle(Event::Key(Key::Char(character))) else {
            panic!("search should request a page");
        };
        search_request = Some(request);
    }
    tui.apply_submission_outcome(TuiSubmissionOutcome::Dialog(DialogView::sessions_page(
        Vec::new(),
        search_request.unwrap(),
        None,
    )));
    renderer.render(tui.view()).unwrap();
    let search = rendered_text(&renderer);
    assert!(search.contains("No sessions match search."), "{search:?}");

    tui.show_selection_dialog(DialogView::sessions_page(
        Vec::new(),
        SessionDialogRequest::initial(),
        None,
    ));
    renderer.render(tui.view()).unwrap();
    let empty_project = rendered_text(&renderer);
    assert!(empty_project.contains("No resumable sessions in current project."));
    let Action::LoadSessionPage(empty_global_request) = tui.handle(Event::Key(Key::LineStart))
    else {
        panic!("scope toggle should request a page");
    };
    tui.apply_submission_outcome(TuiSubmissionOutcome::Dialog(DialogView::sessions_page(
        Vec::new(),
        empty_global_request,
        None,
    )));
    renderer.render(tui.view()).unwrap();
    let empty_global = rendered_text(&renderer);
    assert!(empty_global.contains("No resumable sessions in any project."));

    tui.show_selection_dialog(DialogView::sessions_loading(SessionDialogRequest::initial()));
    renderer.render(tui.view()).unwrap();
    let loading = rendered_text(&renderer);
    assert!(loading.contains("Loading sessions…"), "{loading:?}");
    assert!(!loading.contains("No resumable sessions"), "{loading:?}");

    tui.show_selection_dialog(DialogView::sessions_error(
        SessionDialogRequest::initial(),
        "Saved sessions could not be loaded.",
    ));
    renderer.render(tui.view()).unwrap();
    let error = rendered_text(&renderer);
    assert!(
        error.contains("Saved sessions could not be loaded."),
        "{error:?}"
    );
    assert!(!error.contains("Loading sessions…"), "{error:?}");
}

#[test]
fn short_session_dialog_keeps_search_and_selected_row_visible_without_default_details() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(34, 7)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    let entries = (0..12)
        .map(|id| {
            DialogEntry::action_with_metadata(
                format!("#{id} Session {id}"),
                "2 turns · now",
                format!("{id} Session {id} /work/alpha primary"),
                format!("Turns: 2 · Agent: primary\nID: {id} · Session {id}"),
                format!("session:{id}"),
            )
        })
        .collect::<Vec<_>>();
    tui.show_selection_dialog(DialogView::sessions_page(
        entries,
        SessionDialogRequest::initial(),
        None,
    ));
    tui.handle(Event::Resize {
        width: 34,
        height: 7,
    });
    tui.handle(Event::Key(Key::Char('/')));
    for _ in 0..8 {
        tui.handle(Event::Key(Key::Down));
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);
    assert!(text.contains("/ "), "{text:?}");
    // 34 columns keep the right column and truncate the label instead.
    assert!(text.contains("▌ #8 Sessio"), "{text:?}");
    assert!(text.contains("2 turns · now"), "{text:?}");
    assert!(!text.contains("Agent: primary"), "{text:?}");
    assert!(!text.contains("#0 Session 0"), "{text:?}");
}

#[test]
fn subagent_inspect_dialog_renders_through_the_overlay_shell() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(70, 18)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    for (id, agent) in [(9, "explore"), (10, "reviewer")] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(
                id,
                agent,
                "inspect the overlay shell",
                TuiExecutionState::ForegroundRunning,
            ),
        );
    }

    tui.handle(Event::Key(Key::Escape));
    tui.handle(Event::Key(Key::Char('g')));
    renderer.render(tui.view()).unwrap();
    let inspect = rendered_text(&renderer);

    assert!(inspect.contains("─ Subagents"), "{inspect:?}");
    assert!(inspect.contains("  Main"), "{inspect:?}");
    assert!(inspect.contains("▌ Explore #9"), "{inspect:?}");
    assert!(inspect.contains("  Reviewer #10"), "{inspect:?}");
    assert!(inspect.contains("navigate"), "derived footer: {inspect:?}");
    assert!(inspect.contains("select"), "derived footer: {inspect:?}");
    assert_eq!(
        cell_for_text(&renderer, "Explore #9").bg,
        Color::Rgb(0x1b, 0x33, 0x30),
        "the selection band spans the row"
    );
}

#[test]
fn read_only_dialog_renders_explicit_empty_and_clipped_selected_details() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(32, 7)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.show_selection_dialog(
        DialogView::read_only(
            "MCP servers",
            Some("Type to search | Enter details"),
            vec![DialogEntry::read_only(
                "remote-server-with-a-long-name  http  enabled/ready  32 tools",
                "remote http ready search fetch",
                "Source: global\nEndpoint: https://example.test/a/safe/path\nTools: search, fetch",
            )],
            "mcp",
        )
        .with_empty_message("No MCP servers configured."),
    );

    tui.handle(Event::Key(Key::Enter));
    renderer.render(tui.view()).unwrap();
    let details = rendered_text(&renderer);
    assert!(details.contains("Source: global"), "{details:?}");
    assert!(!details.contains("safe/path"), "{details:?}");
    assert!(details.contains("─ MCP servers"), "{details:?}");

    tui.show_selection_dialog(
        DialogView::read_only("MCP servers", Some("Search"), Vec::new(), "mcp")
            .with_empty_message("No MCP servers configured."),
    );
    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("No MCP servers configured."));
}

#[test]
fn refreshable_dialog_footer_carries_the_refresh_shortcut() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 20)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.show_selection_dialog(DialogView::read_only(
        "MCP servers",
        None::<&str>,
        vec![DialogEntry::read_only(
            "remote  http  enabled/ready",
            "remote",
            "Source: global",
        )],
        "mcp",
    ));

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("refresh"), "derived footer: {text:?}");

    tui.show_selection_dialog(DialogView::selection(
        "Choose a model",
        None::<&str>,
        vec![DialogEntry::action("gpt-4.1", "model:gpt-4.1")],
    ));
    renderer.render(tui.view()).unwrap();

    assert!(
        !rendered_text(&renderer).contains("refresh"),
        "only refreshable dialogs advertise the shortcut: {:?}",
        rendered_text(&renderer)
    );
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
    let before = rendered_text(&renderer);

    tui.handle(Event::Key(Key::Char('/')));
    tui.handle(Event::Key(Key::Char('r')));
    renderer.render(tui.view()).unwrap();
    let palette = rendered_text(&renderer);

    assert!(palette.contains("commands"), "{palette:?}");
    assert!(palette.contains("/review"), "{palette:?}");
    assert!(palette.contains("/resume"), "{palette:?}");
    assert!(palette.contains("/review [scope]"), "{palette:?}");
    assert!(
        !palette.contains("Review the patch"),
        "34 columns degrade to a single column: {palette:?}"
    );
    assert!(
        !palette.contains("Resume a session"),
        "34 columns degrade to a single column: {palette:?}"
    );
    assert!(!palette.contains("[command]"), "{palette:?}");
    assert!(!palette.contains("[built-in]"), "{palette:?}");
    assert_eq!(
        cell_for_text(&renderer, "commands").fg,
        Color::Rgb(0x95, 0xe6, 0xcb)
    );
    assert_eq!(
        cell_for_text(&renderer, "─ commands").fg,
        Color::Rgb(0x6c, 0x73, 0x80)
    );
    assert!(palette.contains("▌ /review"), "{palette:?}");
    assert!(palette.contains("navigate"), "{palette:?}");
    assert!(palette.contains("close"), "{palette:?}");
    assert_eq!(
        cell_for_text(&renderer, "/review").bg,
        Color::Rgb(0x1b, 0x33, 0x30)
    );
    assert!(!palette.contains("/connect"), "{palette:?}");
    assert_ne!(before, palette);

    tui.handle(Event::Key(Key::Escape));
    renderer.render(tui.view()).unwrap();
    assert!(tui.transcript().is_empty());
    assert!(tui.view().status.is_none());
}

#[test]
fn renderer_draws_the_palette_description_column_right_aligned_when_wide() {
    let backend = TestBackend::new(100, 20);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.set_palette_entries(vec![
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

    tui.handle(Event::Key(Key::Char('/')));
    tui.handle(Event::Key(Key::Char('r')));
    renderer.render(tui.view()).unwrap();
    let palette = rendered_text(&renderer);

    assert!(palette.contains("▌ /review [scope]"), "{palette:?}");
    assert!(palette.contains("Review the patch"), "{palette:?}");
    assert!(palette.contains("Resume a session"), "{palette:?}");
    assert_eq!(
        rendered_column(&renderer, "Review the patch"),
        rendered_column(&renderer, "Resume a session"),
        "equal-width descriptions share the right-aligned column"
    );
    assert_eq!(
        cell_for_text(&renderer, "Review the patch").fg,
        Color::Rgb(0xbf, 0xbd, 0xb6),
        "the selected row lifts its metadata out of the muted grey"
    );
    assert_eq!(
        cell_for_text(&renderer, "Resume a session").fg,
        Color::Rgb(0x5c, 0x67, 0x73),
        "unselected rows keep the muted metadata column"
    );
    assert_eq!(
        cell_for_text(&renderer, "Review the patch").bg,
        Color::Rgb(0x1b, 0x33, 0x30),
        "the selection band spans the full row"
    );
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
        parsed: agens_core::ToolInput::Read {
            path: "src/render.rs".into(),
        },
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

    tui.handle(Event::Key(Key::CtrlO));
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
        "read",
        "first line",
        "second line",
        "12ms",
        "old line",
        "new line",
        "8/128",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in {text:?}");
    }
    assert!(!text.contains("context 128"), "{text:?}");
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
    // TOP-only composer border: text starts at x=0 of the composer band.
    assert!(
        cursor.x < 30 && cursor.y < 10,
        "cursor must remain inside the terminal: {cursor:?}"
    );
    assert!(
        rendered_text(&renderer).contains("é🙂"),
        "{:?}",
        rendered_text(&renderer)
    );

    let terminal = Terminal::new(TestBackend::new(5, 8)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    for character in "ab🙂".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    renderer.render(tui.view()).unwrap();
    let cursor = renderer.terminal().backend().cursor_position();
    assert!(
        cursor.x < 5,
        "cursor must remain inside the composer: {cursor:?}"
    );
}

#[test]
fn physical_cursor_follows_main_composer_focus_and_overlay_ownership() {
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Key(Key::Char('x')));

    renderer.render(tui.view()).unwrap();
    assert!(renderer.terminal().backend().cursor_visible());

    tui.handle(Event::Key(Key::PageUp));
    for _ in 0..3 {
        renderer.render(tui.view()).unwrap();
        assert!(!renderer.terminal().backend().cursor_visible());
    }
    tui.handle(Event::Key(Key::ScrollUp));
    renderer.render(tui.view()).unwrap();
    assert!(!renderer.terminal().backend().cursor_visible());

    tui.handle(Event::Key(Key::Char('i')));
    renderer.render(tui.view()).unwrap();
    assert!(renderer.terminal().backend().cursor_visible());

    tui.show_selection_dialog(DialogView::selection(
        "Permission required",
        None::<String>,
        vec![DialogEntry::action("Allow once", "permission:1:allow-once")],
    ));
    renderer.render(tui.view()).unwrap();
    assert!(!renderer.terminal().backend().cursor_visible());
    tui.handle(Event::Key(Key::Escape));

    tui.set_palette_entries(vec![PaletteEntry::new(
        "help",
        "Help",
        "",
        PaletteEntryKind::BuiltIn,
    )]);
    tui.handle(Event::Key(Key::LineStart));
    tui.handle(Event::Key(Key::DeleteToLineEnd));
    tui.handle(Event::Key(Key::Char('/')));
    renderer.render(tui.view()).unwrap();
    assert!(!renderer.terminal().backend().cursor_visible());
    tui.handle(Event::Key(Key::Escape));

    tui.set_running(true);
    renderer.render(tui.view()).unwrap();
    assert!(!renderer.terminal().backend().cursor_visible());
    tui.set_running(false);

    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "explore".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 7 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::started(
            7,
            "explore",
            "inspect",
            agens_tui::TuiExecutionState::ForegroundRunning,
        ),
    );
    tui.select_transcript(TranscriptId::Subagent(7));
    renderer.render(tui.view()).unwrap();
    assert!(!renderer.terminal().backend().cursor_visible());
}

#[test]
fn armed_quit_warning_is_visible_with_exact_copy() {
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);

    assert_eq!(
        tui.handle(Event::Key(Key::CtrlC)),
        agens_tui::Action::Render
    );
    renderer.render(tui.view()).unwrap();

    assert!(rendered_text(&renderer).contains("Press Ctrl+C again to exit"));
}

#[test]
fn session_loading_uses_exact_local_state_without_running_the_composer() {
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine);
    assert!(tui.begin_session_load());

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);
    assert!(rendered.contains("Loading session…"), "{rendered:?}");
    assert!(!rendered.contains("Working"), "{rendered:?}");
    assert!(!rendered.contains(" running "), "{rendered:?}");
    assert!(!tui.view().running);
}

#[test]
fn u15_c1b_renderer_shows_selected_and_all_active_recent_executions() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 50)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.set_agent_catalog(["reviewer", "writer", "tester", "triage"]);
    tui.select_agent("reviewer");
    for (agent, event) in [
        ("reviewer", TuiExecutionEvent::ForegroundStarted { id: 1 }),
        ("writer", TuiExecutionEvent::BackgroundStarted { id: 2 }),
        ("tester", TuiExecutionEvent::ForegroundStarted { id: 3 }),
        ("triage", TuiExecutionEvent::ForegroundStarted { id: 4 }),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event,
        });
    }
    for (agent, event) in [
        ("tester", TuiExecutionEvent::Completed { id: 3 }),
        ("triage", TuiExecutionEvent::Failed { id: 4 }),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event,
        });
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(text.contains("Main"), "{text:?}");
    assert!(text.contains("Reviewer #1"), "{text:?}");
    assert!(text.contains("Writer #2"), "{text:?}");
    assert!(text.contains("Triage #4"), "{text:?}");
    assert!(!text.contains("Tester #3"), "{text:?}");
    assert!(!text.contains("fg 1"), "old aggregate leaked: {text:?}");
}

#[test]
fn p1a1_renderer_collapses_live_tool_uses_and_expands_ordered_details() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 9 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::started(
            9,
            "reviewer",
            "review the rendering",
            TuiExecutionState::ForegroundRunning,
        ),
    );
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::tool_call(9, "read", "native::read", "bounded input"),
    );
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::tool_result(9, "read", "read result", false),
    );
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::tool_call(9, "grep", "native::grep", "bounded pattern"),
    );

    renderer.render(tui.view()).unwrap();
    let main = rendered_text(&renderer);
    assert!(
        main.contains("● Reviewer · review the rendering")
            && main.contains("Running · foreground")
            && main.contains("Read files")
            && main.contains("Search code"),
        "{main:?}"
    );
    assert!(!main.contains("native::read"), "{main:?}");
    assert!(!main.contains("read result"), "{main:?}");
}

#[test]
fn p1a2_renderer_renders_terminal_status_final_result_and_ordered_expanded_tools() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 10 },
    });
    for event in [
        TuiSubagentEvent::started(
            10,
            "reviewer",
            "review terminal output",
            TuiExecutionState::ForegroundRunning,
        ),
        TuiSubagentEvent::tool_call(10, "first", "native::read", "first input"),
        TuiSubagentEvent::tool_result(10, "first", "first result", false),
        TuiSubagentEvent::tool_call(10, "second", "native::grep", "second input"),
    ] {
        apply_subagent(&mut tui, event);
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Completed { id: 10 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::terminal(10, SubagentStatus::Success, "terminal result"),
    );

    renderer.render(tui.view()).unwrap();
    let main = rendered_text(&renderer);
    assert!(
        main.contains("● Reviewer · review terminal output") && main.contains("Success · recent"),
        "{main:?}"
    );
    for child_row in ["terminal result", "read", "native::grep", "first result"] {
        assert!(
            !main.contains(child_row),
            "duplicated {child_row:?}: {main:?}"
        );
    }
}

#[test]
fn subagent_cards_are_compact_statusful_and_never_render_raw_task_payloads() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 30)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.tick(Duration::from_secs(5));
    tui.begin_submission("delegate");
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "parent-task".into(),
        name: "task".into(),
        input: "raw-parent-task-input-secret".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "parent-task".into(),
        content: "raw-parent-task-output-secret".into(),
        is_error: false,
    }));
    for (id, name, input, output) in [
        (
            "task-control",
            "task_control",
            "raw-task-control-input-secret",
            "raw-task-control-output-secret",
        ),
        (
            "task-message",
            "native::task_message",
            "raw-task-message-input-secret",
            "raw-task-message-output-secret",
        ),
    ] {
        tui.apply_progress(TurnEvent::ToolCallRequested {
            id: id.into(),
            name: name.into(),
            input: input.into(),
        });
        tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: id.into(),
            content: output.into(),
            is_error: false,
        }));
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "explore".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 9 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::started(
            9,
            "explore",
            "inspect the navigation",
            TuiExecutionState::ForegroundRunning,
        ),
    );

    renderer.render(tui.view()).unwrap();
    let initializing = rendered_text(&renderer);
    assert!(
        initializing.contains("● Explore · inspect the navigation"),
        "{initializing:?}"
    );
    assert!(initializing.contains("Initializing"), "{initializing:?}");
    assert!(!initializing.contains("raw-parent-task-input-secret"));
    assert!(!initializing.contains("raw-parent-task-output-secret"));
    assert!(!initializing.contains("raw-task-control-input-secret"));
    assert!(!initializing.contains("raw-task-control-output-secret"));
    assert!(!initializing.contains("raw-task-message-input-secret"));
    assert!(!initializing.contains("raw-task-message-output-secret"));

    tui.tick(Duration::from_secs(8));
    for (call_id, name, input) in [
        ("read", "native::read", "api_key=not-rendered"),
        ("grep", "native::grep", "token=not-rendered"),
        ("list", "native::list", "password=not-rendered"),
        ("unknown", "plugin::opaque", "secret=not-rendered"),
    ] {
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::tool_call(9, call_id, name, input),
        );
    }
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::tool_result(9, "read", "child-output-secret", false),
    );

    renderer.render(tui.view()).unwrap();
    let running = rendered_text(&renderer);
    assert!(running.contains("Running"), "{running:?}");
    assert!(running.contains("3s"), "{running:?}");
    assert!(running.contains("Read files"), "{running:?}");
    assert!(running.contains("Search code"), "{running:?}");
    assert!(running.contains("List files"), "{running:?}");
    assert!(running.contains("+1 more activity"), "{running:?}");
    let title_row = rendered_row(&renderer, "● Explore · inspect");
    let last_card_row = rendered_row(&renderer, "+1 more activity");
    assert!(last_card_row.saturating_sub(title_row) < 7, "{running:?}");
    // Navigation affordances belong to the tree under the composer, not the card.
    let affordance_row = rendered_row(&renderer, "Ctrl+B background");
    assert!(affordance_row > last_card_row + 7, "{running:?}");
    for secret in [
        "api_key=not-rendered",
        "token=not-rendered",
        "password=not-rendered",
        "secret=not-rendered",
        "child-output-secret",
        "plugin::opaque",
    ] {
        assert!(!running.contains(secret), "leaked {secret:?}: {running:?}");
    }
}

#[test]
fn resumed_task_tool_rows_remain_stored_but_are_not_rendered_in_main() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 24)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.replace_history(&[
        Message {
            role: Role::User,
            parts: vec![MessagePart::Text("delegate restored".into())],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::ToolCall {
                id: "restored-task".into(),
                name: "native::task".into(),
                input: "restored-raw-input-sentinel".into(),
            }],
        },
        Message {
            role: Role::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_call_id: "restored-task".into(),
                content: "restored-raw-result-sentinel".into(),
                is_error: false,
            }],
        },
        Message {
            role: Role::Assistant,
            parts: vec![MessagePart::Text("restored parent summary".into())],
        },
    ])
    .unwrap();

    let stored = &tui.view().completed_conversations[0].tool_batches[0].calls[0];
    assert_eq!(stored.input, "restored-raw-input-sentinel");
    assert_eq!(
        stored.result.as_ref().unwrap().output,
        "restored-raw-result-sentinel"
    );
    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);
    assert!(rendered.contains("restored parent summary"), "{rendered:?}");
    assert!(!rendered.contains("restored-raw-input-sentinel"));
    assert!(!rendered.contains("restored-raw-result-sentinel"));
    assert!(!rendered.contains("native::task"));
}

#[test]
fn subagent_terminal_status_and_elapsed_are_frozen_and_low_dimensions_are_safe() {
    let mut tui = Tui::new(FakeEngine);
    tui.tick(Duration::from_secs(5));
    for (id, agent) in [(9, "explore"), (10, "reviewer")] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(
                id,
                agent,
                "bounded summary",
                TuiExecutionState::ForegroundRunning,
            ),
        );
    }
    tui.tick(Duration::from_secs(10));
    for (id, agent, event, status) in [
        (
            9,
            "explore",
            TuiExecutionEvent::Completed { id: 9 },
            SubagentStatus::Success,
        ),
        (
            10,
            "reviewer",
            TuiExecutionEvent::Failed { id: 10 },
            SubagentStatus::Failure,
        ),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event,
        });
        apply_subagent(&mut tui, TuiSubagentEvent::terminal(id, status, "done"));
    }
    tui.tick(Duration::from_secs(20));
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::text(9, "post-terminal-sentinel"),
    );

    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 24)).unwrap());
    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);
    assert!(rendered.contains("Success · recent · 5s"), "{rendered:?}");
    assert!(rendered.contains("Failure · recent · 5s"), "{rendered:?}");
    assert!(!rendered.contains("post-terminal-sentinel"));

    for height in 1..=8 {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(20, height)).unwrap());
        renderer.render(tui.view()).unwrap();
        assert_eq!(
            rendered_text(&renderer).chars().count(),
            20 * usize::from(height)
        );
    }

    tui.set_palette_entries(vec![
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
    tui.handle(Event::Key(Key::Char('/')));
    let low_dimensions = |tui: &Tui<FakeEngine>, label: &str| {
        for (width, height) in [(1, 1), (2, 3), (8, 4), (34, 10)] {
            let mut renderer =
                RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
            renderer.render(tui.view()).unwrap();
            assert_eq!(
                rendered_text(&renderer).chars().count(),
                usize::from(width) * usize::from(height),
                "{label} at {width}x{height}"
            );
        }
    };
    low_dimensions(&tui, "palette");

    tui.handle(Event::Key(Key::Escape));
    tui.show_selection_dialog(DialogView::selection(
        "Choose a model",
        Some("Up/Down navigate, Enter selects, Esc cancels"),
        (0..12)
            .map(|index| DialogEntry::action(format!("Option {index:02}"), format!("pick:{index}")))
            .collect(),
    ));
    low_dimensions(&tui, "selection dialog");

    tui.handle(Event::Key(Key::CtrlO));
    low_dimensions(&tui, "selection dialog with details");

    tui.show_selection_dialog(DialogView::sessions_page(
        vec![DialogEntry::action_with_metadata(
            "#7 Alpha",
            "2 turns · 5m ago",
            "7 alpha",
            "ID: 7 · Alpha\nTurns: 2",
            "session:7",
        )],
        SessionDialogRequest::initial(),
        Some(SessionDialogCursor::new(100, 7)),
    ));
    low_dimensions(&tui, "session dialog");

    tui.show_selection_dialog(DialogView::sessions_loading(SessionDialogRequest::initial()));
    low_dimensions(&tui, "loading session dialog");

    tui.show_selection_dialog(
        DialogView::selection(
            "Permission required",
            Some("native::read\n/work/alpha"),
            vec![
                DialogEntry::action("Allow once", "permission:1:allow-once"),
                DialogEntry::action("Deny once", "permission:1:deny-once"),
            ],
        )
        .as_confirm(),
    );
    low_dimensions(&tui, "confirm dialog");
}

#[test]
fn execution_strip_shows_main_and_at_most_three_prioritized_children() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 50)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    for (id, agent, event) in [
        (1, "old", TuiExecutionEvent::ForegroundStarted { id: 1 }),
        (2, "recent", TuiExecutionEvent::ForegroundStarted { id: 2 }),
        (
            3,
            "background",
            TuiExecutionEvent::BackgroundStarted { id: 3 },
        ),
        (4, "active", TuiExecutionEvent::ForegroundStarted { id: 4 }),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event,
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(
                id,
                agent,
                "task",
                match event {
                    TuiExecutionEvent::BackgroundStarted { .. } => {
                        TuiExecutionState::BackgroundRunning
                    }
                    _ => TuiExecutionState::ForegroundRunning,
                },
            ),
        );
        tui.tick(Duration::from_secs(id));
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "old".into(),
        event: TuiExecutionEvent::Completed { id: 1 },
    });

    renderer.render(tui.view()).unwrap();
    let rendered = rendered_text(&renderer);
    assert!(rendered.contains("Main"), "{rendered:?}");
    assert!(rendered.contains("Active #4"), "{rendered:?}");
    assert!(rendered.contains("Background #3"), "{rendered:?}");
    assert!(rendered.contains("Recent #2"), "{rendered:?}");
    assert!(!rendered.contains("Old #1"), "{rendered:?}");
}

#[test]
fn p1c2_renderer_shows_restored_tool_count_without_fabricating_tool_details() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::RestoredCompletedSubagent {
        id: 42,
        agent: "reviewer".into(),
        task_summary: "review the durable result".into(),
        final_result: "approved".into(),
        tool_uses: 3,
    });

    renderer.render(tui.view()).unwrap();
    let main = rendered_text(&renderer);
    assert!(main.contains("● Reviewer · review the durable result"));
    assert!(main.contains("Success · recent"));
    assert!(main.contains("+3 more activities"));
    assert!(!main.contains("Final result: approved"));
}

struct FixedPtyHarness {
    width: u16,
    height: u16,
    renderer: RatatuiRenderer<TestBackend>,
}

impl FixedPtyHarness {
    fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            renderer: Self::renderer(width, height),
        }
    }

    fn resize(&mut self, tui: &mut Tui<FakeEngine>, width: u16, height: u16) {
        tui.handle(Event::Resize { width, height });
        self.width = width;
        self.height = height;
        self.renderer = Self::renderer(width, height);
    }

    fn render(&mut self, tui: &Tui<FakeEngine>) -> String {
        self.renderer.render(tui.view()).unwrap();
        rendered_text(&self.renderer)
    }

    fn cursor(&self) -> ratatui::layout::Position {
        self.renderer.terminal().backend().cursor_position()
    }

    fn renderer(width: u16, height: u16) -> RatatuiRenderer<TestBackend> {
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap())
    }
}

#[test]
fn structural_pty_resize_scroll_stream_and_dialog_contract() {
    let mut harness = FixedPtyHarness::new(52, 14);
    let mut tui = Tui::new(FakeEngine);

    tui.begin_submission("streaming request");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        (0..24)
            .map(|line| format!("before-resize-{line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )));
    harness.render(&tui);

    tui.handle(Event::Key(Key::ScrollUp));
    let scrolled = harness.render(&tui);
    assert!(
        scrolled.contains("before-resize-"),
        "scroll must keep prior stream rows visible: {scrolled:?}"
    );
    assert!(!tui.following_bottom());

    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "streamed-after-scroll-sentinel".into(),
    )));
    let streamed = harness.render(&tui);
    assert!(
        streamed.contains("before-resize-"),
        "stream while scrolled must preserve anchor rows: {streamed:?}"
    );
    assert!(!streamed.contains("streamed-after-scroll-sentinel") || !tui.following_bottom());
    assert!(!tui.following_bottom());

    tui.handle(Event::Key(Key::End));
    let refollowed = harness.render(&tui);
    assert!(refollowed.contains("streamed-after-scroll-sentinel"));
    assert!(tui.following_bottom());

    tui.show_selection_dialog(DialogView::selection(
        "Recover interrupted attempt",
        Some("This may invalidate an attempt still running in another process."),
        vec![DialogEntry::action("Recover", "recover:7")],
    ));
    let dialog = harness.render(&tui);
    assert!(dialog.contains("Recover interrupted attempt"), "{dialog:?}");
    assert!(dialog.contains("still running"), "{dialog:?}");
    assert!(!dialog.contains("streaming request"), "{dialog:?}");

    tui.handle(Event::Key(Key::Escape));
    for character in "composer\né🙂".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }

    harness.resize(&mut tui, 24, 8);
    let resized = harness.render(&tui);
    assert!(
        resized.contains("composer") || resized.contains("é🙂"),
        "{resized:?}"
    );
    let cursor = harness.cursor();
    assert!(cursor.x < 24 && cursor.y < 8, "{cursor:?}");

    for height in 1..=12 {
        harness.resize(&mut tui, 40, height);
        let surface = harness.render(&tui);
        assert_eq!(surface.chars().count(), 40 * usize::from(height));
        assert!(!surface.contains("agens safe"), "height {height}");
        if height >= 12 {
            assert!(
                surface.contains("Ready") || surface.contains("Responding"),
                "height {height}"
            );
        }
    }
}

#[test]
fn active_transcript_render_keeps_child_rows_out_of_main_and_renders_owner_navigation() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::ForegroundStarted { id: 7 },
    });
    for event in [
        TuiSubagentEvent::started(
            7,
            "reviewer",
            "review the active transcript",
            TuiExecutionState::ForegroundRunning,
        ),
        TuiSubagentEvent::reasoning(7, "child-reasoning-sentinel"),
        TuiSubagentEvent::text(7, "child-text-sentinel"),
        TuiSubagentEvent::tool_call(7, "child-call", "native::read", "child-input-sentinel"),
        TuiSubagentEvent::tool_result(7, "child-call", "child-result-sentinel", false),
        TuiSubagentEvent::error(7, SubagentErrorKind::Tool),
    ] {
        apply_subagent(&mut tui, event);
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Failed { id: 7 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::terminal(7, SubagentStatus::Failure, "child-final-sentinel"),
    );

    renderer.render(tui.view()).unwrap();
    let main = rendered_text(&renderer);
    assert!(
        !main.contains("Main · primary conversation"),
        "main provenance should be quiet: {main:?}"
    );
    assert!(
        main.contains("● Reviewer · review the active transcript"),
        "{main:?}"
    );
    for child_row in [
        "child-reasoning-sentinel",
        "child-text-sentinel",
        "child-input-sentinel",
        "child-result-sentinel",
        "Subagent tool execution failed.",
        "child-final-sentinel",
    ] {
        assert!(
            !main.contains(child_row),
            "duplicated {child_row:?}: {main:?}"
        );
    }

    tui.select_transcript(TranscriptId::Subagent(7));
    // Ctrl+O is thinking-first, then tools.
    tui.handle(Event::Key(Key::CtrlO));
    tui.handle(Event::Key(Key::CtrlO));
    renderer.render(tui.view()).unwrap();
    let child = rendered_text(&renderer);
    assert!(child.contains("Subagent 7 · reviewer"), "{child:?}");
    assert!(
        child.contains("g select · m Main · h/l sibling"),
        "{child:?}"
    );
    assert!(
        child.contains("Subagent transcript · i to message · x to cancel"),
        "{child:?}"
    );
    assert!(!child.contains("Compose"), "{child:?}");
    for child_row in [
        "child-reasoning-sentinel",
        "child-text-sentinel",
        "child-input-sentinel",
        "child-result-sentinel",
        "Subagent tool execution failed.",
        "child-final-sentinel",
    ] {
        assert!(
            child.contains(child_row),
            "missing {child_row:?}: {child:?}"
        );
    }
}

#[test]
fn conversation_owns_the_first_row_under_every_notice_condition() {
    type ArmNotice = fn(&mut Tui<FakeEngine>);

    let notices: [(&str, ArmNotice); 5] = [
        ("Press Ctrl+C again to exit", |tui| {
            tui.handle(Event::Key(Key::CtrlC));
        }),
        ("Recovered failed prompt", |tui| {
            tui.apply_submission_outcome(TuiSubmissionOutcome::SessionResumed {
                message: "Recovered failed prompt.".into(),
                presentation: TuiPresentation::new("provider", "model", "session #1"),
                history: Vec::new(),
                draft: Some("failed prompt".into()),
                resume_error: None,
                file_candidates: Vec::new(),
                palette_entries: Vec::new(),
            });
        }),
        ("danger", |tui| tui.set_dangerous_mode(true)),
        ("BYPASS", |tui| tui.set_bypass(true)),
        ("status-sentinel", |tui| tui.add_info("status-sentinel")),
    ];

    for (needle, arm) in notices {
        let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 30)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 30,
        });
        arm(&mut tui);

        renderer.render(tui.view()).unwrap();

        assert!(
            !rendered_line(&renderer, 0).contains(needle),
            "{needle:?} must not band the first row: {:?}",
            rendered_line(&renderer, 0)
        );
        let bottom = composer_bottom_row(&renderer);
        assert!(
            (bottom + 1..30).any(|row| rendered_line(&renderer, usize::from(row)).contains(needle)),
            "{needle:?} belongs to the bottom chrome, below composer bottom {bottom}: {:?}",
            rendered_text(&renderer)
        );
    }
}

#[test]
fn reserved_bottom_chrome_parks_the_composer_and_keeps_it_stable() {
    let height = 30_u16;
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width: 80, height });

    renderer.render(tui.view()).unwrap();
    let idle_top = composer_top_row(&renderer);
    let idle_bottom = composer_bottom_row(&renderer);
    assert!(
        idle_bottom + 3 < height,
        "the composer is parked above the reserved bottom chrome: bottom {idle_bottom}"
    );
    assert!(
        !rendered_line(&renderer, 0).contains('┌'),
        "the conversation owns the first row: {:?}",
        rendered_line(&renderer, 0)
    );

    tui.handle(Event::Key(Key::CtrlC));
    start_execution(&mut tui, 9, "explore");
    start_execution(&mut tui, 10, "plan");
    renderer.render(tui.view()).unwrap();

    assert_eq!(
        (composer_top_row(&renderer), composer_bottom_row(&renderer)),
        (idle_top, idle_bottom),
        "notices and the subagent tree must not move the composer"
    );
    let notice_row = rendered_row(&renderer, "Press Ctrl+C again to exit") as u16;
    let tree_row = rendered_row(&renderer, "Tab focus") as u16;
    let footer_row = rendered_row(&renderer, "model —") as u16;
    assert_eq!(
        footer_row, idle_bottom,
        "the metadata stays glued to the composer border"
    );
    assert!(
        idle_bottom < notice_row && notice_row < tree_row,
        "bottom chrome order below the composer is notice then tree: {notice_row} {tree_row}"
    );

    tui.handle(Event::Key(Key::CtrlC));
    tui.handle(Event::Key(Key::Escape));
    renderer.render(tui.view()).unwrap();
    assert_eq!(
        (composer_top_row(&renderer), composer_bottom_row(&renderer)),
        (idle_top, idle_bottom),
        "clearing a notice must not move the composer either"
    );
}

#[test]
fn tree_affordance_advertises_background_only_while_a_branch_runs_in_foreground() {
    let (width, height) = (100_u16, 30_u16);
    let mut renderer =
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width, height });
    start_execution(&mut tui, 9, "explore");

    renderer.render(tui.view()).unwrap();
    assert!(
        rendered_text(&renderer).contains("Tab focus · Enter inspect · Ctrl+B background"),
        "a foreground branch can still be backgrounded: {:?}",
        rendered_text(&renderer)
    );

    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "explore".into(),
        event: TuiExecutionEvent::Backgrounded { id: 9 },
    });
    renderer.render(tui.view()).unwrap();
    let backgrounded = rendered_text(&renderer);
    assert!(
        backgrounded.contains("Tab focus · Enter inspect"),
        "focus and inspect always apply: {backgrounded:?}"
    );
    assert!(
        !backgrounded.contains("Ctrl+B"),
        "an already backgrounded branch must not advertise Ctrl+B: {backgrounded:?}"
    );

    start_execution(&mut tui, 10, "plan");
    renderer.render(tui.view()).unwrap();
    assert!(
        rendered_text(&renderer).contains("Tab focus · Enter inspect · Ctrl+B background"),
        "a new foreground branch brings the hint back: {:?}",
        rendered_text(&renderer)
    );
}

#[test]
fn bottom_chrome_flushes_the_subagent_tree_under_the_composer() {
    let (width, height) = (80_u16, 30_u16);
    let mut renderer =
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width, height });
    start_execution(&mut tui, 9, "explore");

    renderer.render(tui.view()).unwrap();
    let bottom = composer_bottom_row(&renderer);
    assert_eq!(
        rendered_row(&renderer, "Main") as u16,
        bottom + 1,
        "an inactive notice must not leave a dead row under the composer: {:?}",
        rendered_line(&renderer, usize::from(bottom + 1))
    );
    assert_eq!(
        rendered_row(&renderer, "model —") as u16,
        bottom,
        "the metadata keeps the composer's bottom border"
    );

    tui.handle(Event::Key(Key::CtrlC));
    renderer.render(tui.view()).unwrap();

    assert_eq!(
        composer_bottom_row(&renderer),
        bottom,
        "showing a notice must not move the composer"
    );
    assert_eq!(
        rendered_row(&renderer, "Press Ctrl+C again to exit") as u16,
        bottom + 1,
        "an active notice owns the row under the composer"
    );
    assert_eq!(
        rendered_row(&renderer, "Main") as u16,
        bottom + 2,
        "the tree follows the notice without a gap"
    );
    assert_eq!(
        rendered_row(&renderer, "model —") as u16,
        bottom,
        "the metadata keeps the composer's bottom border with a notice too"
    );
}

/// Composer border rows for a terminal whose gutter is not the wide default.
fn composer_border_rows(renderer: &RatatuiRenderer<TestBackend>, gutter: u16) -> (u16, u16) {
    let buffer = renderer.terminal().backend().buffer();
    let top = (0..buffer.area.height)
        .find(|row| buffer[(gutter, *row)].symbol() == "┌")
        .expect("composer top border should be rendered");
    let bottom = (top + 1..buffer.area.height)
        .find(|row| buffer[(gutter, *row)].symbol() == "└")
        .expect("composer bottom border should be rendered");
    (top, bottom)
}

fn docked_renderer(width: u16, height: u16) -> (RatatuiRenderer<TestBackend>, Tui<FakeEngine>) {
    let renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width, height });
    (renderer, tui)
}

#[test]
fn composer_bottom_border_hosts_the_metadata_right_aligned() {
    let (width, height) = (100_u16, 24_u16);
    let (mut renderer, mut tui) = docked_renderer(width, height);

    renderer.render(tui.view()).unwrap();
    let (top, bottom) = composer_border_rows(&renderer, CHROME_GUTTER);

    assert_eq!(
        rendered_line(&renderer, usize::from(bottom)),
        format!(
            "    └{} model — · effort — · ctx — · agens · Ready ┘    ",
            "─".repeat(46)
        ),
        "the metadata is spliced into the composer border, one gap off the corner"
    );
    assert_eq!(
        rendered_text(&renderer).matches("model —").count(),
        1,
        "the metadata renders once, in the border only"
    );
    for row in bottom + 1..height {
        assert!(
            !rendered_line(&renderer, usize::from(row)).contains("Ready"),
            "no detached status row survives below the composer: row {row}"
        );
    }

    tui.begin_submission("prompt");
    renderer.render(tui.view()).unwrap();
    let (running_top, running_bottom) = composer_border_rows(&renderer, CHROME_GUTTER);
    assert_eq!((running_top, running_bottom), (top, bottom));
    assert!(
        rendered_line(&renderer, usize::from(top)).contains(" running "),
        "the top border keeps its state label: {:?}",
        rendered_line(&renderer, usize::from(top))
    );
    assert!(
        rendered_line(&renderer, usize::from(bottom)).contains("model —"),
        "the bottom border keeps the metadata while running: {:?}",
        rendered_line(&renderer, usize::from(bottom))
    );
}

#[test]
fn border_metadata_drops_segments_as_the_composer_narrows() {
    for (width, present, absent) in [
        (100_u16, "model — · effort — · ctx — · agens · Ready", ""),
        (52, "model — · effort — · ctx — · Ready", "agens"),
        (42, "model — · effort — · Ready", "ctx —"),
        (36, "model — · Ready", "effort —"),
    ] {
        let (mut renderer, tui) = docked_renderer(width, 20);
        renderer.render(tui.view()).unwrap();
        let (_, bottom) = composer_border_rows(&renderer, CHROME_GUTTER);
        let line = rendered_line(&renderer, usize::from(bottom));

        assert!(
            line.contains(present),
            "width {width} keeps {present:?}: {line:?}"
        );
        assert!(
            absent.is_empty() || !line.contains(absent),
            "width {width} drops {absent:?}: {line:?}"
        );
        assert_eq!(
            line.chars().nth(usize::from(width - 2 - CHROME_GUTTER)),
            Some(' '),
            "width {width} keeps the metadata off the closing corner: {line:?}"
        );
        assert_eq!(
            line.chars().nth(usize::from(width - 1 - CHROME_GUTTER)),
            Some('┘'),
            "width {width} keeps the closing corner: {line:?}"
        );
    }
}

#[test]
fn a_border_too_narrow_for_the_metadata_falls_back_to_its_own_row() {
    let (width, height) = (24_u16, 20_u16);
    let (mut renderer, tui) = docked_renderer(width, height);

    renderer.render(tui.view()).unwrap();
    let (_, bottom) = composer_border_rows(&renderer, 0);
    let border = rendered_line(&renderer, usize::from(bottom));

    assert_eq!(
        border,
        format!("└{}┘", "─".repeat(usize::from(width - 2))),
        "a border too narrow for the shortest form stays undecorated"
    );
    assert_eq!(
        rendered_row(&renderer, "model —") as u16,
        height - 1,
        "the metadata falls back to its own row below the composer"
    );
}

#[test]
fn idle_bottom_chrome_leaves_at_most_two_rows_below_the_composer() {
    let height = 24_u16;
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(72, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width: 72, height });

    renderer.render(tui.view()).unwrap();
    let bottom = composer_bottom_row(&renderer);
    let gap = height
        .saturating_sub(1)
        .saturating_sub(bottom)
        .saturating_sub(1);

    assert!(
        gap <= 2,
        "the idle screen keeps at most two blank chrome rows: composer bottom {bottom}, gap {gap}"
    );
}

#[test]
fn elided_subagent_tree_keeps_a_running_branch_and_the_affordance_as_its_last_row() {
    let height = 14_u16;
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(72, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width: 72, height });
    for (id, agent) in [(9_u64, "explore"), (10, "plan"), (11, "build")] {
        start_execution(&mut tui, id, agent);
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "build".into(),
        event: TuiExecutionEvent::Completed { id: 11 },
    });

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(
        text.contains("Plan #10"),
        "the elided tree keeps a running branch: {text:?}"
    );
    assert!(
        !text.contains("Build #11"),
        "a finished branch is elided before a running one: {text:?}"
    );
    assert_eq!(
        rendered_row(&renderer, "Tab to focus"),
        rendered_row(&renderer, "Plan #10") + 1,
        "the affordance survives elision as the last tree row: {text:?}"
    );
    assert!(
        rendered_row(&renderer, "Tab to focus") < usize::from(height - 1),
        "the elided tree stays inside its reserved band: {text:?}"
    );
    assert!(
        !text.contains("Main"),
        "the root is elided before a running branch: {text:?}"
    );
}

#[test]
fn elided_subagent_tree_reports_the_hidden_branch_count() {
    let height = 24_u16;
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(72, height)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width: 72, height });
    for (id, agent) in [(9_u64, "explore"), (10, "plan"), (11, "build")] {
        start_execution(&mut tui, id, agent);
    }

    renderer.render(tui.view()).unwrap();
    let text = rendered_text(&renderer);

    assert!(
        text.contains("+2 more · Tab to focus"),
        "the elision row states the hidden branch count and keeps the affordance: {text:?}"
    );
}

#[test]
fn bottom_chrome_degrades_without_panicking_on_small_terminals() {
    for (width, height, gutter) in [
        (1_u16, 1_u16, 0_u16),
        (1, 2, 0),
        (2, 1, 0),
        (4, 3, 0),
        (10, 7, 0),
        (20, 12, 0),
        (24, 14, 0),
        (40, 20, CHROME_GUTTER),
    ] {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Resize { width, height });
        tui.handle(Event::Key(Key::CtrlC));
        start_execution(&mut tui, 9, "explore");

        renderer.render(tui.view()).unwrap();
        let text = rendered_text(&renderer);

        assert_eq!(
            text.chars().count(),
            usize::from(width) * usize::from(height),
            "{width}x{height}"
        );
        if height >= 2 {
            let buffer = renderer.terminal().backend().buffer();
            let top = (0..height)
                .find(|row| buffer[(gutter, *row)].symbol() == "┌")
                .unwrap_or_else(|| panic!("no composer top at {width}x{height}: {text:?}"));
            assert!(
                (top + 1..height).any(|row| buffer[(gutter, row)].symbol() == "└"),
                "the composer keeps priority over decorative chrome at {width}x{height}: {text:?}"
            );
            if width >= 2 {
                assert_eq!(
                    buffer[(width - 1 - gutter, top)].symbol(),
                    "┐",
                    "the gutter stays symmetric at {width}x{height}: {text:?}"
                );
            }
        }
    }
}

#[test]
fn active_transcript_render_keeps_terminal_child_renderable_after_expiry_and_switches_siblings() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    for (id, agent, text) in [
        (7, "reviewer", "expired-child-sentinel"),
        (8, "writer", "sibling-child-sentinel"),
    ] {
        tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
            agent: agent.into(),
            event: TuiExecutionEvent::ForegroundStarted { id },
        });
        apply_subagent(
            &mut tui,
            TuiSubagentEvent::started(id, agent, "task", TuiExecutionState::ForegroundRunning),
        );
        apply_subagent(&mut tui, TuiSubagentEvent::text(id, text));
    }
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::Completed { id: 7 },
    });
    apply_subagent(
        &mut tui,
        TuiSubagentEvent::terminal(7, SubagentStatus::Success, "expired-final-sentinel"),
    );

    tui.select_transcript(TranscriptId::Subagent(7));
    tui.tick(Duration::from_secs(60));
    renderer.render(tui.view()).unwrap();
    let expired = rendered_text(&renderer);
    assert!(expired.contains("Subagent 7 · reviewer"), "{expired:?}");
    assert!(expired.contains("expired-child-sentinel"), "{expired:?}");
    assert!(expired.contains("expired-final-sentinel"), "{expired:?}");

    tui.handle(Event::Key(Key::Char('l')));
    renderer.render(tui.view()).unwrap();
    let sibling = rendered_text(&renderer);
    assert!(sibling.contains("Subagent 8 · writer"), "{sibling:?}");
    assert!(sibling.contains("sibling-child-sentinel"), "{sibling:?}");
    assert!(!sibling.contains("expired-child-sentinel"), "{sibling:?}");
}

#[test]
fn renderer_draws_the_file_picker_with_the_name_and_its_directory_column() {
    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 14)).unwrap());
    let mut tui = Tui::new(FakeEngine);
    tui.add_info("conversation sentinel");
    tui.set_file_candidates(vec![
        "AGENTS.md".to_owned(),
        "crates/agens-cli/src/lib.rs".to_owned(),
        "crates/agens-tui/src/render.rs".to_owned(),
    ]);

    renderer.render(tui.view()).unwrap();
    assert!(rendered_text(&renderer).contains("conversation sentinel"));

    tui.handle(Event::Key(Key::Char('@')));
    renderer.render(tui.view()).unwrap();
    let picker = rendered_text(&renderer);

    assert!(picker.contains("files"), "{picker:?}");
    assert!(picker.contains("▌ AGENTS.md"), "{picker:?}");
    assert!(picker.contains("render.rs"), "{picker:?}");
    assert!(picker.contains("crates/agens-tui/src"), "{picker:?}");
    assert!(picker.contains("insert"), "{picker:?}");
    assert_eq!(
        rendered_column(&renderer, "crates/agens-cli/src"),
        rendered_column(&renderer, "crates/agens-tui/src"),
        "equal-width directories share the right-aligned column"
    );
    assert_eq!(
        cell_for_text(&renderer, "crates/agens-tui/src").fg,
        Color::Rgb(0x5c, 0x67, 0x73)
    );

    for character in "render".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    renderer.render(tui.view()).unwrap();
    let filtered = rendered_text(&renderer);
    assert!(filtered.contains("▌ render.rs"), "{filtered:?}");
    assert!(!filtered.contains("AGENTS.md"), "{filtered:?}");

    for character in "-missing".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    renderer.render(tui.view()).unwrap();
    assert!(
        rendered_text(&renderer).contains("No matching files"),
        "{:?}",
        rendered_text(&renderer)
    );
}

#[test]
fn the_file_picker_stays_panic_free_and_exact_on_narrow_terminals() {
    let mut tui = Tui::new(FakeEngine);
    tui.set_file_candidates(
        (0..40)
            .map(|index| format!("crates/agens-tui/src/module-{index:02}.rs"))
            .collect(),
    );
    tui.handle(Event::Key(Key::Char('@')));
    assert!(tui.view().file_picker.is_some());

    for (width, height) in [(1, 1), (2, 3), (8, 4), (12, 6), (34, 10)] {
        let mut renderer =
            RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
        renderer.render(tui.view()).unwrap();
        assert_eq!(
            rendered_text(&renderer).chars().count(),
            usize::from(width) * usize::from(height),
            "file picker at {width}x{height}"
        );
    }
}

#[test]
fn a_context_changed_outcome_shows_the_toggled_safety_state_without_a_further_keystroke() {
    for (needle, presentation) in [
        (
            "danger",
            TuiPresentation::new("provider", "model", "session #1").with_dangerous_mode(true),
        ),
        (
            "BYPASS",
            TuiPresentation::new("provider", "model", "session #1").with_bypass(true),
        ),
    ] {
        let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(80, 30)).unwrap());
        let mut tui = Tui::new(FakeEngine);
        tui.handle(Event::Resize {
            width: 80,
            height: 30,
        });

        assert!(
            tui.apply_submission_outcome(TuiSubmissionOutcome::ContextChanged {
                message: format!("{needle} toggled."),
                presentation,
            })
            .is_none()
        );

        renderer.render(tui.view()).unwrap();

        assert!(
            rendered_text(&renderer).contains(needle),
            "{needle:?} must be visible right after the toggle: {:?}",
            rendered_text(&renderer)
        );
    }
}

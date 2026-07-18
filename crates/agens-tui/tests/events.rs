use agens_core::{MessagePart, TurnEvent, TurnState};
use agens_tui::{Action, Engine, Event, Key, RatatuiRenderer, Renderer, TranscriptEntry, Tui};
use ratatui::{Terminal, backend::TestBackend};

#[derive(Default)]
struct FakeEngine {
    cancellations: usize,
}

impl Engine for FakeEngine {
    fn cancel(&mut self) {
        self.cancellations += 1;
    }
}

#[test]
fn normal_input_submits_the_composed_prompt() {
    let mut tui = Tui::new(FakeEngine::default());

    assert_eq!(tui.handle(Event::Key(Key::Char('h'))), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Char('i'))), Action::Render);
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("hi".into())
    );
    assert_eq!(tui.input(), "");
}

#[test]
fn second_submission_is_rejected_while_a_turn_owns_cancellation() {
    let mut tui = Tui::new(FakeEngine::default());

    tui.begin_submission("first prompt");
    assert_eq!(tui.handle(Event::Key(Key::Char('s'))), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert_eq!(tui.input(), "s");
    assert_eq!(
        tui.transcript(),
        [
            agens_tui::TranscriptEntry::User("first prompt".into()),
            agens_tui::TranscriptEntry::Info("A response is already in progress.".into()),
        ]
    );
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Cancel);
    assert_eq!(tui.engine().cancellations, 1);
}

#[test]
fn resize_updates_the_render_state() {
    let mut tui = Tui::new(FakeEngine::default());

    assert_eq!(
        tui.handle(Event::Resize {
            width: 120,
            height: 40
        }),
        Action::Render
    );
    assert_eq!(tui.size(), (120, 40));
}

#[test]
fn control_c_cancels_a_running_turn_before_quitting() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_running(true);

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Cancel);
    assert_eq!(tui.engine().cancellations, 1);
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Cancel);
    assert_eq!(tui.engine().cancellations, 2);
}

#[test]
fn repeated_control_c_quits_only_when_idle_and_input_is_empty() {
    let mut tui = Tui::new(FakeEngine::default());

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Quit);
}

#[test]
fn submitted_prompt_and_provider_output_are_retained_in_order() {
    let mut tui = Tui::new(FakeEngine::default());

    tui.begin_submission("explain the project");
    tui.finish_submission(Ok("Agens is a coding agent.".into()));

    assert_eq!(
        tui.transcript(),
        [
            agens_tui::TranscriptEntry::User("explain the project".into()),
            agens_tui::TranscriptEntry::Assistant("Agens is a coding agent.".into()),
        ]
    );
    assert!(!tui.view().running);
}

#[test]
fn provider_failures_are_shown_without_leaving_the_turn_running() {
    let mut tui = Tui::new(FakeEngine::default());

    tui.begin_submission("use the provider");
    tui.finish_submission(Err("provider: provider request failed".into()));

    assert_eq!(
        tui.transcript(),
        [
            agens_tui::TranscriptEntry::User("use the provider".into()),
            agens_tui::TranscriptEntry::Error("provider: provider request failed".into()),
        ]
    );
    assert!(!tui.view().running);
}

#[test]
fn streaming_events_update_stable_entries_and_preserve_tool_order() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("inspect the project");

    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("First ".into())));
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("answer".into())));
    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "call-1".into(),
        name: "native::read".into(),
        input: "secret path omitted".into(),
    });
    tui.apply_progress(TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "call-1".into(),
        content: "file contents".into(),
        is_error: false,
    }));
    tui.apply_progress(TurnEvent::StateChanged(TurnState::Completed));

    assert_eq!(
        tui.transcript(),
        [
            TranscriptEntry::User("inspect the project".into()),
            TranscriptEntry::Assistant("First answer".into()),
            TranscriptEntry::Tool("native::read started".into()),
            TranscriptEntry::Tool("native::read completed: file contents".into()),
        ]
    );
    assert!(!tui.view().running);
}

#[test]
fn multiline_editing_and_scroll_follow_are_deterministic() {
    let mut tui = Tui::new(FakeEngine::default());
    tui.handle(Event::Key(Key::Char('a')));
    tui.handle(Event::Key(Key::ShiftEnter));
    tui.handle(Event::Key(Key::Char('b')));
    tui.handle(Event::Key(Key::Left));
    tui.handle(Event::Key(Key::Backspace));
    tui.handle(Event::Key(Key::PageUp));

    assert_eq!(tui.input(), "ab");
    assert!(!tui.following_bottom());
    assert_eq!(tui.handle(Event::Key(Key::End)), Action::Render);
    assert!(tui.following_bottom());
    assert_eq!(
        tui.handle(Event::Key(Key::Enter)),
        Action::Submit("ab".into())
    );
}

#[test]
fn ratatui_layout_degrades_without_overlapping_at_standard_narrow_and_short_sizes() {
    for (width, height) in [(80, 24), (35, 24), (80, 10)] {
        let backend = TestBackend::new(width, height);
        let terminal = Terminal::new(backend).unwrap();
        let mut renderer = RatatuiRenderer::new(terminal);
        let tui = Tui::new(FakeEngine::default());

        renderer.render(tui.view()).unwrap();
        let buffer = renderer.terminal().backend().buffer();

        assert_eq!(buffer.area.width, width);
        assert_eq!(buffer.area.height, height);
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "a"));
    }
}

#[test]
fn ratatui_surface_presents_context_roles_activity_and_responsive_shortcuts() {
    let backend = TestBackend::new(96, 24);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine::default());
    tui.set_presentation("openai-api", "gpt-4.1", "session #42");
    tui.begin_submission("Inspect the project structure.");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Reasoning(
        "Checking the workspace.".into(),
    )));
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "The workspace contains focused Rust crates.".into(),
    )));
    tui.handle(Event::Key(Key::Char('n')));
    tui.handle(Event::Key(Key::ShiftEnter));
    tui.handle(Event::Key(Key::Char('o')));

    renderer.render(tui.view()).unwrap();
    let buffer = renderer.terminal().backend().buffer();
    let text = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(text.contains("agens"));
    assert!(text.contains("openai-api / gpt-4.1"));
    assert!(text.contains("session #42"));
    assert!(text.contains("USER"));
    assert!(text.contains("THINKING"));
    assert!(text.contains("ASSISTANT"));
    assert!(text.contains("Compose"));
    assert!(text.contains("2 lines"));
    assert!(text.contains("Shift+Enter"));
    assert!(text.contains("LIVE"));

    let user_cell = buffer
        .content
        .iter()
        .find(|cell| cell.symbol() == "U")
        .expect("user role label is rendered");
    assert_eq!(user_cell.fg, ratatui::style::Color::Green);

    tui.apply_progress(TurnEvent::ToolCallRequested {
        id: "call-1".into(),
        name: "native::read".into(),
        input: "omitted".into(),
    });
    renderer.render(tui.view()).unwrap();
    let tool_text = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(tool_text.contains("TOOL"));
    assert!(tool_text.contains("Tool: native::read"));

    let backend = TestBackend::new(50, 14);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    renderer.render(tui.view()).unwrap();
    let narrow_text = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(narrow_text.contains("agens"));
    assert!(narrow_text.contains("Enter"));
    assert!(!narrow_text.contains("Shift+Enter"));
    assert!(narrow_text.contains("Compose"));
}

#[test]
fn ratatui_active_turn_row_distinguishes_waiting_responding_cancelling_and_failure() {
    let backend = TestBackend::new(80, 24);
    let terminal = Terminal::new(backend).unwrap();
    let mut renderer = RatatuiRenderer::new(terminal);
    let mut tui = Tui::new(FakeEngine::default());
    tui.begin_submission("status test");

    renderer.render(tui.view()).unwrap();
    let waiting = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(waiting.contains("Waiting"));

    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text(
        "response".into(),
    )));
    renderer.render(tui.view()).unwrap();
    let responding = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(responding.contains("Responding"));

    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Cancel);
    renderer.render(tui.view()).unwrap();
    let cancelling = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(cancelling.contains("Cancelling"));

    tui.apply_progress(TurnEvent::StateChanged(TurnState::Failed));
    renderer.render(tui.view()).unwrap();
    let failed = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(failed.contains("Failed"));
}

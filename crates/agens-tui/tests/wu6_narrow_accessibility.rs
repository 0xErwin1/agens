use std::time::Instant;

use agens_tui::{
    AppEvent, AppState, Engine, Event, Key, PromptTransition, RatatuiRenderer, Renderer,
    SurfaceFocus, Tui, TuiExecutionEvent, TuiRuntimeEvent,
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

fn enter(tui: &mut Tui<FakeEngine>, text: &str) {
    for character in text.chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
}

#[test]
fn narrow_surface_keeps_composer_queue_and_activity_controls_reachable() {
    let mut tui = Tui::with_queue_capacity(FakeEngine, 1);
    tui.handle(Event::Resize {
        width: 30,
        height: 12,
    });
    tui.begin_submission("active");
    tui.apply_runtime_event(TuiRuntimeEvent::TaskExecution {
        agent: "reviewer".into(),
        event: TuiExecutionEvent::BackgroundStarted { id: 7 },
    });
    enter(&mut tui, "queued");
    tui.handle(Event::Key(Key::Enter));
    enter(&mut tui, "kept draft");
    tui.handle(Event::Key(Key::Enter));

    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(30, 12)).unwrap());
    renderer.render(tui.view()).unwrap();
    let composer = rendered_text(&renderer);
    assert!(composer.contains("queue is full"), "{composer:?}");
    assert!(composer.contains("Ctrl+C"), "{composer:?}");

    tui.handle(Event::Key(Key::Tab));
    assert_eq!(tui.view().surface_focus, SurfaceFocus::Queue);
    renderer.render(tui.view()).unwrap();
    let queue = rendered_text(&renderer);
    assert!(queue.contains("queued"), "queue row missing: {queue:?}");
    assert!(queue.contains("QUEUE"), "{queue:?}");
    assert!(queue.contains("edit"), "{queue:?}");
    assert!(
        queue.contains("Del") || queue.contains("remove"),
        "{queue:?}"
    );

    tui.handle(Event::Key(Key::Tab));
    assert_eq!(tui.view().surface_focus, SurfaceFocus::Activity);
    renderer.render(tui.view()).unwrap();
    let activity = rendered_text(&renderer);
    assert!(activity.contains("ACTIVITY"), "{activity:?}");
    assert!(activity.contains("cancel"), "{activity:?}");
    assert!(activity.contains("all"), "{activity:?}");
}

#[test]
fn narrow_running_composer_keeps_a_grapheme_cursor_inside_terminal_bounds() {
    let (width, height) = (24, 10);
    let mut tui = Tui::new(FakeEngine);
    tui.handle(Event::Resize { width, height });
    tui.begin_submission("active");
    enter(&mut tui, "e\u{301} 🙂 abc");
    tui.handle(Event::Key(Key::PreviousWord));
    tui.handle(Event::Key(Key::NextWord));

    let mut renderer =
        RatatuiRenderer::new(Terminal::new(TestBackend::new(width, height)).unwrap());
    renderer.render(tui.view()).unwrap();

    let cursor = renderer.terminal().backend().cursor_position();
    assert!(renderer.terminal().backend().cursor_visible());
    assert!(cursor.x < width, "cursor x outside terminal: {cursor:?}");
    assert!(cursor.y < height, "cursor y outside terminal: {cursor:?}");
    assert_eq!(tui.input(), "e\u{301} 🙂 abc");
}

#[test]
fn scheduler_observability_is_exhaustive_and_cannot_carry_prompt_content() {
    let mut scheduler = AppState::new(2);
    let secret_prompt = "never-observe-this-prompt";
    let active = scheduler.reduce(AppEvent::SubmitPrompt("active".into()));
    assert_eq!(active.len(), 1);
    let generation = scheduler.lifecycle().active().unwrap().generation();

    scheduler.reduce(AppEvent::SubmitPrompt(secret_prompt.into()));
    let queued = scheduler.queued_entries()[0].id();
    scheduler.remove_queue_entry(queued);
    scheduler.reduce(AppEvent::SubmitPrompt("next".into()));
    scheduler.reduce(AppEvent::Key(Key::CtrlC, Instant::now()));
    scheduler.reduce(AppEvent::TurnCancelledFor {
        generation: generation + 1,
    });
    scheduler.reduce(AppEvent::TurnCancelledFor { generation });
    scheduler.reduce(AppEvent::DeferAutoTurn);
    scheduler.reduce(AppEvent::DeferAutoTurn);

    let observability = scheduler.observability();
    assert_eq!(observability.queued(), 2);
    assert_eq!(observability.dequeued(), 1);
    assert_eq!(observability.removed(), 1);
    assert_eq!(observability.cancellation_requested(), 1);
    assert_eq!(observability.cancellation_confirmed(), 1);
    assert_eq!(observability.stale_event_dropped(), 1);
    assert_eq!(observability.auto_turn_coalesced(), 1);
    assert_eq!(
        observability.transitions(),
        [
            PromptTransition::Queued,
            PromptTransition::Removed,
            PromptTransition::Queued,
            PromptTransition::CancellationRequested,
            PromptTransition::StaleEventDropped,
            PromptTransition::CancellationConfirmed,
            PromptTransition::Dequeued,
            PromptTransition::AutoTurnCoalesced,
        ]
    );
    assert!(
        !format!("{observability:?}").contains(secret_prompt),
        "observability payload must not include prompt content"
    );
}

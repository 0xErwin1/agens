use agens_tui::{
    Action, AppEvent, AppState, Effect, Engine, Event, Key, SurfaceFocus, TranscriptEntry, Tui,
};
use agens_tui::{RatatuiRenderer, Renderer};
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
fn running_composer_keeps_editing_cursor_and_tab_focus_without_escape_mode_switch() {
    let mut tui = Tui::with_queue_capacity(FakeEngine::default(), 2);
    tui.begin_submission("active");

    tui.handle(Event::Key(Key::Char('a')));
    tui.handle(Event::Key(Key::Char(' ')));
    tui.handle(Event::Key(Key::Char('é')));
    tui.handle(Event::Key(Key::PreviousWord));
    tui.handle(Event::Key(Key::DeleteNextWord));

    assert_eq!(tui.input(), "a ");
    assert_eq!(tui.view().surface_focus, SurfaceFocus::Composer);
    assert!(tui.view().composer_cursor_visible);

    assert_eq!(tui.handle(Event::Key(Key::Tab)), Action::Render);
    assert_eq!(tui.view().surface_focus, SurfaceFocus::Queue);
    assert_eq!(tui.handle(Event::Key(Key::Tab)), Action::Render);
    assert_eq!(tui.view().surface_focus, SurfaceFocus::Activity);
    assert_eq!(tui.handle(Event::Key(Key::Tab)), Action::Render);
    assert_eq!(tui.view().surface_focus, SurfaceFocus::Composer);

    assert_eq!(tui.handle(Event::Key(Key::Escape)), Action::Render);
    assert_eq!(tui.view().surface_focus, SurfaceFocus::Composer);
    assert_eq!(tui.input(), "a ");
    assert!(tui.view().running);
    assert_eq!(tui.engine().cancellations, 0);
}

#[test]
fn queue_is_fifo_editable_and_lossless_before_dispatch() {
    let mut tui = Tui::with_queue_capacity(FakeEngine::default(), 2);
    tui.begin_submission("active");

    for prompt in ["first", "second"] {
        for character in prompt.chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    }

    let entries = tui.queue_entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].prompt(), "first");
    assert_eq!(entries[1].prompt(), "second");
    assert_ne!(entries[0].id(), entries[1].id());
    assert!(tui.input().is_empty());
    assert_eq!(
        tui.transcript(),
        &[TranscriptEntry::User("active".into())],
        "queued drafts must not enter transcript history"
    );

    for character in "third".chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
    assert_eq!(tui.input(), "third");
    assert!(
        tui.status()
            .is_some_and(|status| status.contains("queue is full")),
        "status: {:?}",
        tui.status()
    );

    tui.handle(Event::Key(Key::Tab));
    assert_eq!(tui.view().surface_focus, SurfaceFocus::Queue);
    tui.handle(Event::Key(Key::Down));
    tui.handle(Event::Key(Key::AltUp));
    assert_eq!(
        tui.queue_entries()
            .iter()
            .map(|entry| entry.prompt())
            .collect::<Vec<_>>(),
        ["second", "first"]
    );
    tui.handle(Event::Key(Key::Delete));
    assert_eq!(tui.queue_entries().len(), 1);
    tui.handle(Event::Key(Key::Enter));
    assert_eq!(tui.input(), "first");
    assert!(tui.queue_entries().is_empty());
}

#[test]
fn scheduler_queue_operations_preserve_identity_and_refuse_at_capacity() {
    let mut scheduler = AppState::new(2);
    scheduler.reduce(AppEvent::SubmitPrompt("active".into()));
    scheduler.reduce(AppEvent::SubmitPrompt("first".into()));
    scheduler.reduce(AppEvent::SubmitPrompt("second".into()));

    let first_id = scheduler.queued_entries()[0].id();
    let second_id = scheduler.queued_entries()[1].id();
    assert!(scheduler.move_queue_entry(second_id, -1));
    assert_eq!(scheduler.queued_entries()[0].id(), second_id);
    assert_eq!(
        scheduler
            .remove_queue_entry(first_id)
            .map(|entry| entry.prompt().to_owned()),
        Some("first".to_owned())
    );
    assert_eq!(scheduler.queued_entries()[0].id(), second_id);

    scheduler.reduce(AppEvent::SubmitPrompt("replacement".into()));
    assert_eq!(
        scheduler.reduce(AppEvent::SubmitPrompt("overflow".into())),
        vec![Effect::RefusePrompt(
            "Prompt queue is full; draft was kept unchanged.".into()
        )]
    );
}

#[test]
fn queue_rows_render_in_fifo_order_without_transcript_admission() {
    let mut tui = Tui::with_queue_capacity(FakeEngine::default(), 3);
    tui.begin_submission("active");
    for prompt in ["first queued", "second queued"] {
        for character in prompt.chars() {
            tui.handle(Event::Key(Key::Char(character)));
        }
        tui.handle(Event::Key(Key::Enter));
    }

    let mut renderer = RatatuiRenderer::new(Terminal::new(TestBackend::new(100, 30)).unwrap());
    renderer.render(tui.view()).unwrap();
    let rendered = renderer
        .terminal()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();

    assert!(rendered.contains("1. first queued"));
    assert!(rendered.contains("2. second queued"));
    assert_eq!(tui.transcript().len(), 1);
}

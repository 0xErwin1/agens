use std::time::Instant;

use agens_core::{MessagePart, TurnEvent};
use agens_tui::{
    Action, AppEvent, AppState, Effect, Engine, Event, Key, TranscriptEntry, Tui,
    TuiProviderOutcome, TurnLifecycle,
};

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
fn cancellation_waits_for_matching_terminal_before_fifo_dispatch() {
    let mut app = AppState::new(2);
    let now = Instant::now();
    assert_eq!(
        app.reduce(AppEvent::SubmitPrompt("active".into())),
        vec![Effect::StartPrompt("active".into())]
    );
    app.reduce(AppEvent::SubmitPrompt("queued".into()));
    assert_eq!(
        app.reduce(AppEvent::Key(Key::CtrlC, now)),
        vec![Effect::CancelTurn]
    );
    assert!(matches!(app.lifecycle(), TurnLifecycle::Cancelling(_)));
    assert!(
        app.reduce(AppEvent::TurnCancelledFor { generation: 0 })
            .is_empty()
    );
    assert_eq!(
        app.reduce(AppEvent::TurnCancelledFor { generation: 1 }),
        vec![Effect::StartPrompt("queued".into())]
    );
}

#[test]
fn cancelling_control_c_is_not_a_quit_gesture() {
    let mut tui = Tui::with_queue_capacity(FakeEngine::default(), 2);
    tui.begin_submission("active");
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.engine().cancellations, 1);
    assert_eq!(tui.handle(Event::Key(Key::CtrlC)), Action::Render);
    assert_eq!(tui.engine().cancellations, 1);
}

#[test]
fn terminal_provider_outcomes_dispatch_the_next_fifo_prompt() {
    for outcome in [
        TuiProviderOutcome::Completed("answer".into()),
        TuiProviderOutcome::Failed {
            message: "failed".into(),
            action: "retry".into(),
        },
        TuiProviderOutcome::Cancelled {
            message: "cancelled".into(),
            action: "retry".into(),
        },
    ] {
        let mut tui = Tui::with_queue_capacity(FakeEngine::default(), 2);
        tui.begin_submission("active");
        type_and_queue(&mut tui, "queued");

        assert_eq!(tui.finish_provider_turn(outcome), Some("queued".into()));
        assert!(tui.queue_entries().is_empty());
    }
}

#[test]
fn cancelled_partial_output_stays_with_cancelled_turn_until_next_dispatch() {
    let mut tui = Tui::with_queue_capacity(FakeEngine::default(), 2);
    tui.begin_submission("active");
    tui.apply_progress(TurnEvent::ProviderPart(MessagePart::Text("partial".into())));
    type_and_queue(&mut tui, "queued");

    assert_eq!(
        tui.finish_provider_turn(TuiProviderOutcome::Cancelled {
            message: "cancelled".into(),
            action: "retry".into(),
        }),
        Some("queued".into())
    );
    assert_eq!(
        tui.transcript(),
        &[
            TranscriptEntry::User("active".into()),
            TranscriptEntry::Assistant("partial".into()),
            TranscriptEntry::Error("cancelled".into()),
        ]
    );
}

fn type_and_queue(tui: &mut Tui<FakeEngine>, prompt: &str) {
    for character in prompt.chars() {
        assert_eq!(tui.handle(Event::Key(Key::Char(character))), Action::Render);
    }

    assert_eq!(tui.handle(Event::Key(Key::Enter)), Action::Render);
}

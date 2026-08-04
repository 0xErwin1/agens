use std::time::Instant;

use agens_tui::{AppEvent, AppState, Effect, TurnLifecycle};

#[test]
fn explicit_fifo_dispatches_before_a_deferred_auto_turn() {
    let mut scheduler = AppState::new(2);
    assert_eq!(
        scheduler.reduce(AppEvent::SubmitPrompt("active".into())),
        vec![Effect::StartPrompt("active".into())]
    );
    scheduler.reduce(AppEvent::SubmitPrompt("explicit".into()));
    scheduler.reduce(AppEvent::DeferAutoTurn);
    assert_eq!(
        scheduler.reduce(AppEvent::TurnCompletedFor {
            generation: 1,
            output: "done".into(),
        }),
        vec![
            Effect::PersistCompleted {
                prompt: "active".into(),
                output: "done".into(),
            },
            Effect::StartPrompt("explicit".into()),
        ]
    );
    assert!(scheduler.take_ready_auto_turn().is_none());

    assert!(
        scheduler
            .reduce(AppEvent::TurnCancelledFor { generation: 2 })
            .is_empty()
    );
    assert_eq!(scheduler.take_ready_auto_turn(), Some(1));
    assert!(matches!(scheduler.lifecycle(), TurnLifecycle::Running(_)));
}

#[test]
fn draft_blocks_and_preserves_coalesced_auto_turn_until_the_safe_idle_point() {
    let mut scheduler = AppState::new(1);
    scheduler.set_composer("still typing");
    scheduler.reduce(AppEvent::DeferAutoTurn);
    scheduler.reduce(AppEvent::DeferAutoTurn);
    scheduler.reduce(AppEvent::DeferAutoTurn);

    assert!(scheduler.take_ready_auto_turn().is_none());

    scheduler.set_composer("");
    assert_eq!(scheduler.take_ready_auto_turn(), Some(3));
    assert!(scheduler.take_ready_auto_turn().is_none());
}

#[test]
fn auto_turn_is_cancellable_through_the_authoritative_lifecycle() {
    let mut scheduler = AppState::new(1);
    let now = Instant::now();
    scheduler.reduce(AppEvent::DeferAutoTurn);
    scheduler
        .take_ready_auto_turn()
        .expect("deferred turn starts when idle");

    assert_eq!(
        scheduler.reduce(AppEvent::Key(agens_tui::Key::CtrlC, now)),
        vec![Effect::CancelTurn]
    );
    assert!(matches!(
        scheduler.lifecycle(),
        TurnLifecycle::Cancelling(_)
    ));
}

//! Mid-turn steering: a prompt submitted while a turn runs is handed to the
//! steering channel instead of waiting for the turn boundary, and the boundary
//! queue stays the fallback for anything the turn never collected.

use agens_tui::{AppEvent, AppState, Effect};
use agens_core::{Message, MessagePart, Role};

fn running_app(capacity: usize) -> AppState {
    let mut app = AppState::new(capacity);
    app.enable_steering();
    app.reduce(AppEvent::SubmitPrompt("first".into()));
    app
}

#[test]
fn a_prompt_submitted_mid_turn_is_handed_to_steering_and_stays_queued() {
    let mut app = running_app(4);

    let effects = app.reduce(AppEvent::SubmitPrompt("steer me".into()));

    assert_eq!(
        effects,
        vec![Effect::SteerPrompt {
            id: 1,
            text: "steer me".into(),
        }]
    );
    let entries = app.queued_entries();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].steered());
    assert_eq!(entries[0].prompt(), "steer me");
}

#[test]
fn steering_stays_off_unless_the_runtime_installed_a_channel() {
    let mut app = AppState::new(4);
    app.reduce(AppEvent::SubmitPrompt("first".into()));

    assert!(app.reduce(AppEvent::SubmitPrompt("queued".into())).is_empty());
    assert!(!app.queued_entries()[0].steered());
}

#[test]
fn a_delivered_steer_leaves_the_boundary_queue() {
    let mut app = running_app(4);
    app.reduce(AppEvent::SubmitPrompt("steer me".into()));

    assert!(
        app.reduce(AppEvent::SteeringDelivered {
            text: "steer me".into(),
        })
        .is_empty()
    );

    assert!(app.queued_entries().is_empty());
    let effects = app.reduce(AppEvent::TurnCompletedFor {
        generation: 1,
        output: "done".into(),
    });
    assert_eq!(
        effects,
        vec![Effect::PersistCompleted {
            prompt: "first".into(),
            output: "done".into(),
        }],
        "a consumed steer must not fire again at the boundary"
    );
}

#[test]
fn delivery_removes_only_the_oldest_matching_entry() {
    let mut app = running_app(4);
    app.reduce(AppEvent::SubmitPrompt("same".into()));
    app.reduce(AppEvent::SubmitPrompt("same".into()));

    app.reduce(AppEvent::SteeringDelivered {
        text: "same".into(),
    });

    let entries = app.queued_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id(), 2);
}

#[test]
fn a_delivery_with_no_matching_entry_changes_nothing() {
    let mut app = running_app(4);
    app.reduce(AppEvent::SubmitPrompt("steer me".into()));
    let before = app.clone();

    assert!(
        app.reduce(AppEvent::SteeringDelivered {
            text: "something else".into(),
        })
        .is_empty()
    );
    assert_eq!(app, before);
}

#[test]
fn an_undelivered_steer_still_drains_at_the_turn_boundary() {
    let mut app = running_app(4);
    app.reduce(AppEvent::SubmitPrompt("steer me".into()));

    let effects = app.reduce(AppEvent::TurnCompletedFor {
        generation: 1,
        output: "done".into(),
    });

    assert_eq!(
        effects,
        vec![
            Effect::PersistCompleted {
                prompt: "first".into(),
                output: "done".into(),
            },
            Effect::StartPrompt("steer me".into()),
        ]
    );
}

#[test]
fn an_undelivered_steer_still_drains_after_cancellation() {
    let mut app = running_app(4);
    app.reduce(AppEvent::SubmitPrompt("steer me".into()));

    let effects = app.reduce(AppEvent::TurnCancelledFor { generation: 1 });

    assert_eq!(effects, vec![Effect::StartPrompt("steer me".into())]);
}

#[test]
fn a_message_with_media_queues_without_steering() {
    let mut app = running_app(4);

    let message = Message {
        role: Role::User,
        parts: vec![
            MessagePart::Text("look at this".into()),
            MessagePart::Media {
                media_id: 3,
                mime: "image/png".into(),
            },
        ],
    };
    let effects = app.reduce(AppEvent::QueueMessage {
        display: "look at this[Image #1]".into(),
        message,
    });

    assert!(effects.is_empty());
    assert!(!app.queued_entries()[0].steered());
}

#[test]
fn a_resolved_text_message_mid_turn_is_steered() {
    let mut app = running_app(4);

    let effects = app.reduce(AppEvent::QueueMessage {
        display: "/command".into(),
        message: Message {
            role: Role::User,
            parts: vec![MessagePart::Text("expanded command".into())],
        },
    });

    assert_eq!(
        effects,
        vec![Effect::SteerPrompt {
            id: 1,
            text: "expanded command".into(),
        }]
    );
    assert!(app.queued_entries()[0].steered());
}

#[test]
fn a_full_queue_still_refuses_before_steering() {
    let mut app = running_app(1);
    app.reduce(AppEvent::SubmitPrompt("occupies the queue".into()));

    let effects = app.reduce(AppEvent::SubmitPrompt("overflow".into()));

    assert!(matches!(effects.first(), Some(Effect::RefusePrompt(_))));
}

//! Mid-turn steering: a prompt submitted while a turn runs is handed to the
//! steering channel instead of waiting for the turn boundary, and the boundary
//! queue stays the fallback for anything the turn never collected.

use agens_core::{
    HeadlessIntraTurnInbox, IntraTurnInputSource, IntraTurnSteeringQueue, Message, MessagePart,
    Role, TurnEvent,
};
use agens_tui::{
    AppEvent, AppState, Effect, Engine, Event, Key, TranscriptEntry, Tui, TuiProviderOutcome,
};

#[derive(Default)]
struct SteerEngine;

impl Engine for SteerEngine {
    fn cancel(&mut self) {}
}

fn block_on_ready<T>(future: impl std::future::Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

    match future.as_mut().poll(context) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("steering drains complete immediately"),
    }
}

fn drained_texts(steering: &IntraTurnSteeringQueue) -> Vec<String> {
    let mut inbox = steering.clone();
    block_on_ready(inbox.drain())
        .expect("the steering queue drains")
        .into_iter()
        .map(|input| input.text)
        .collect()
}

fn steering_tui() -> (Tui<SteerEngine>, IntraTurnSteeringQueue) {
    let mut tui = Tui::new(SteerEngine);
    let steering = IntraTurnSteeringQueue::default();
    tui.set_steering(steering.clone());
    tui.begin_submission("first");
    (tui, steering)
}

fn submit_text(tui: &mut Tui<SteerEngine>, text: &str) {
    for character in text.chars() {
        tui.handle(Event::Key(Key::Char(character)));
    }
    tui.handle(Event::Key(Key::Enter));
}

#[test]
fn a_mid_turn_submission_reaches_the_steering_channel() {
    let (mut tui, steering) = steering_tui();

    submit_text(&mut tui, "steer me");

    assert_eq!(drained_texts(&steering), ["steer me"]);
    let entries = tui.queue_entries();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].steered());
}

#[test]
fn a_collected_steer_moves_from_the_queue_into_the_transcript() {
    let (mut tui, steering) = steering_tui();
    submit_text(&mut tui, "steer me");

    tui.apply_progress(TurnEvent::IntraTurnInput {
        source: IntraTurnInputSource::Human,
        text: "steer me".into(),
    });

    assert!(tui.queue_entries().is_empty());
    assert!(
        tui.transcript()
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::User(text) if text == "steer me")),
        "a delivered steer renders as user input"
    );
    let _ = steering;
}

#[test]
fn a_finished_turn_clears_steers_it_never_collected() {
    let (mut tui, steering) = steering_tui();
    submit_text(&mut tui, "steer me");

    let next = tui.finish_provider_turn(TuiProviderOutcome::Completed("done".into()));

    assert_eq!(next.as_deref(), Some("steer me"));
    assert!(
        drained_texts(&steering).is_empty(),
        "an uncollected steer must not leak into the next turn"
    );
}

#[test]
fn removing_a_steered_queue_entry_withdraws_it_from_the_channel() {
    let (mut tui, steering) = steering_tui();
    submit_text(&mut tui, "steer me");
    let id = tui.queue_entries()[0].id();

    assert!(tui.withdraw_queue_entry(id).is_some());

    assert!(drained_texts(&steering).is_empty());
    assert!(tui.queue_entries().is_empty());
}

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

    assert!(
        app.reduce(AppEvent::SubmitPrompt("queued".into()))
            .is_empty()
    );
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

#[test]
fn a_supervisor_delivery_never_retires_a_queued_user_steer() {
    let (mut tui, _steering) = steering_tui();
    submit_text(&mut tui, "steer me");

    tui.apply_progress(TurnEvent::IntraTurnInput {
        source: IntraTurnInputSource::Supervisor,
        text: "steer me".into(),
    });

    assert_eq!(tui.queue_entries().len(), 1);
}

//! A chat hosted by the daemon: prompts in, a turn's events out, and a session
//! whose life is nobody's terminal.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agens_core::ask_user::{
    AskUserAnswer, AskUserMode, AskUserOption, AskUserQuestion, AskUserReply, AskUserRequest,
};
use agens_core::{HeadlessTurnCancellation, TurnEvent, TurnState};
use agens_server::{
    ChatAsks, ChatError, ChatEvent, ChatPermissionAnswer, ChatPermissionRequest, ChatSession,
    ChatSessionRequest, ChatSessions, ChatSubscription, ChatTurnOutcome, ChatTurns,
    SessionAdmission, SessionBudget, SessionId, SessionProvider, SessionRuntime, SessionState,
    SessionSupervisor,
};
use tokio::runtime::Runtime;

/// How long a test waits for something the daemon does on another thread.
const PATIENCE: Duration = Duration::from_secs(5);

struct StubProvider;

impl SessionProvider for StubProvider {
    fn model(&self) -> &str {
        "stub/model"
    }
}

/// A turn the test drives: it reports one provider part, then waits to be
/// released, so a test can observe a chat while a turn is still running.
struct ScriptedTurns {
    started: Sender<String>,
    release: Arc<Mutex<Receiver<ChatTurnOutcome>>>,
    /// A question this turn asks before doing anything else, when the test gave
    /// it one. The answer becomes the turn's own outcome, so a test can read it.
    question: Option<ChatPermissionRequest>,
    ask_user: Option<AskUserRequest>,
}

impl ChatTurns for ScriptedTurns {
    fn command(&mut self, command: &str) -> Result<String, ChatError> {
        match command {
            "/effort high" => Ok("Reasoning effort: high.".to_owned()),
            _ => Err(ChatError::Unavailable("unsupported command".to_owned())),
        }
    }

    fn run(
        &mut self,
        message: &agens_core::SessionMessage,
        _runtime: &SessionRuntime,
        cancellation: &HeadlessTurnCancellation,
        asks: &Arc<dyn ChatAsks>,
        progress: &agens_core::TurnProgressSink,
    ) -> ChatTurnOutcome {
        let prompt = message
            .as_message()
            .parts
            .iter()
            .filter_map(|part| match part {
                agens_core::MessagePart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let _ = self.started.send(prompt);
        progress(TurnEvent::StateChanged(TurnState::Streaming));

        // A turn the test gave a question to asks it and reports the answer as
        // its own outcome, so what came back is readable from the stream.
        if let Some(question) = self.question.take() {
            let answer = asks.permission(&question);

            return ChatTurnOutcome::Completed(answer.as_str().to_owned());
        }
        if let Some(question) = self.ask_user.take() {
            let answer = asks.ask_user(&question);
            let selected = match answer {
                AskUserReply::Answered(answers) => answers
                    .first()
                    .and_then(|answer| answer.selected.first())
                    .cloned()
                    .unwrap_or_else(|| "unavailable".to_owned()),
                AskUserReply::Cancelled => "cancelled".to_owned(),
                _ => "unavailable".to_owned(),
            };

            return ChatTurnOutcome::Completed(format!("continued:{selected}"));
        }

        let release = self
            .release
            .lock()
            .expect("the release channel is readable");
        let deadline = Instant::now() + PATIENCE;

        // Polled rather than blocked outright, because what this turn is
        // standing in for is a provider call that looks at its cancellation
        // between chunks.
        loop {
            if cancellation.is_cancelled() {
                return ChatTurnOutcome::Failed("the turn was cancelled".to_owned());
            }

            match release.recv_timeout(Duration::from_millis(10)) {
                Ok(outcome) => return outcome,
                Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(_) => {
                    return ChatTurnOutcome::Failed("the turn was never released".to_owned());
                }
            }
        }
    }
}

/// What a test holds to drive the chats it opened.
///
/// The chats are declared before the runtime, and the order matters: dropping
/// the runtime first would shut its blocking pool down while the hosted
/// sessions are still parked on inboxes nobody has closed, and the pool waits
/// for them. Dropping the chats first closes those inboxes, which is the signal
/// a hosted session ends on.
struct Harness {
    chats: ChatSessions,
    supervisor: SessionSupervisor,
    _runtime: Runtime,
    started: Receiver<String>,
    release: Sender<ChatTurnOutcome>,
}

fn harness() -> Harness {
    harness_asking(None)
}

fn harness_asking(question: Option<ChatPermissionRequest>) -> Harness {
    harness_with_questions(question, None)
}

fn harness_asking_user(question: AskUserRequest) -> Harness {
    harness_with_questions(None, Some(question))
}

fn harness_with_questions(
    question: Option<ChatPermissionRequest>,
    ask_user: Option<AskUserRequest>,
) -> Harness {
    let asked = Arc::new(Mutex::new(question));
    let ask_user = Arc::new(Mutex::new(ask_user));
    let runtime = Runtime::new().expect("a runtime is available");
    let supervisor = SessionSupervisor::new(runtime.handle().clone());

    let (started, started_rx) = channel();
    let (release, release_rx) = channel();
    let release_rx = Arc::new(Mutex::new(release_rx));

    let chats = ChatSessions::new(
        supervisor.clone(),
        Arc::new(move |request: &ChatSessionRequest| {
            let session = SessionId::new(request.resume.unwrap_or(1));

            Ok(ChatSession {
                admission: SessionAdmission::new(
                    session,
                    Box::new(StubProvider),
                    SessionBudget::unlimited(),
                ),
                turns: Box::new(ScriptedTurns {
                    started: started.clone(),
                    release: Arc::clone(&release_rx),
                    question: asked.lock().expect("the script is readable").clone(),
                    ask_user: ask_user.lock().expect("the script is readable").clone(),
                }),
            })
        }),
        Arc::new(|_| {
            Ok(vec![agens_core::Message {
                role: agens_core::Role::User,
                parts: vec![agens_core::MessagePart::Text("what we said".to_owned())],
            }])
        }),
    );

    Harness {
        chats,
        supervisor,
        _runtime: runtime,
        started: started_rx,
        release,
    }
}

fn user_message(text: &str) -> agens_core::SessionMessage {
    agens_core::SessionMessage::try_from(agens_core::Message {
        role: agens_core::Role::User,
        parts: vec![agens_core::MessagePart::Text(text.to_owned())],
    })
    .unwrap()
}

fn request(session: i64) -> ChatSessionRequest {
    ChatSessionRequest {
        checkout: PathBuf::from("/projects/agens"),
        resume: Some(session),
    }
}

/// Drains a subscription until it carries the event the predicate accepts.
fn wait_for(events: &ChatSubscription, accepts: impl Fn(&ChatEvent) -> bool) -> ChatEvent {
    let deadline = Instant::now() + PATIENCE;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(event) if accepts(&event) => return event,
            Ok(_) => continue,
            Err(error) => panic!("the chat never published the event: {error:?}"),
        }
    }
}

#[test]
fn a_command_executes_on_the_daemon_owned_chat_state() {
    let harness = harness();
    let session = harness.chats.open(&request(1)).expect("the chat opens");

    assert_eq!(
        harness.chats.command(session, "/effort high".to_owned()),
        Ok("Reasoning effort: high.".to_owned()),
    );
}

#[test]
fn a_prompt_reaches_the_turn_and_its_progress_reaches_a_subscriber() {
    let harness = harness();
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness
        .chats
        .prompt(session, user_message("what does this repository do"))
        .expect("the prompt is accepted");

    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("what does this repository do".to_owned()),
    );

    assert_eq!(
        wait_for(&events, |event| matches!(event, ChatEvent::Progress(_))),
        ChatEvent::Progress(TurnEvent::StateChanged(TurnState::Streaming)),
    );

    harness
        .release
        .send(ChatTurnOutcome::Completed("it hosts chats".to_owned()))
        .expect("the turn is waiting");

    assert_eq!(
        wait_for(&events, |event| matches!(
            event,
            ChatEvent::TurnCompleted { .. }
        )),
        ChatEvent::TurnCompleted {
            text: "it hosts chats".to_owned(),
        },
    );
}

/// A chat is a conversation: the reply to a prompt sent behind another one
/// depends on what that one did, so the daemon refuses it rather than queueing
/// work whose meaning changes underneath the client.
#[test]
fn a_prompt_sent_while_a_turn_is_running_and_one_already_waits_is_refused() {
    let harness = harness();
    let session = harness.chats.open(&request(1)).expect("the chat opens");

    harness
        .chats
        .prompt(session, user_message("first"))
        .expect("the prompt is accepted");
    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("first".to_owned())
    );

    harness
        .chats
        .prompt(session, user_message("second"))
        .expect("one prompt may wait behind a running turn");

    assert_eq!(
        harness.chats.prompt(session, user_message("third")),
        Err(ChatError::Busy),
    );
}

#[test]
fn a_turn_that_failed_is_published_as_a_failure_and_the_chat_stays_open() {
    let harness = harness();
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness
        .chats
        .prompt(session, user_message("first"))
        .expect("the prompt is accepted");
    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("first".to_owned())
    );

    harness
        .release
        .send(ChatTurnOutcome::Failed("the provider refused".to_owned()))
        .expect("the turn is waiting");

    assert_eq!(
        wait_for(&events, |event| matches!(
            event,
            ChatEvent::TurnFailed { .. }
        )),
        ChatEvent::TurnFailed {
            detail: "the provider refused".to_owned(),
        },
    );

    harness
        .chats
        .prompt(session, user_message("second"))
        .expect("a failed turn does not close the chat");
    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("second".to_owned()),
    );
}

#[test]
fn closing_a_chat_ends_its_session_and_says_so_on_the_stream() {
    let harness = harness();
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness.chats.close(session).expect("the chat is open");

    assert_eq!(
        wait_for(&events, |event| matches!(event, ChatEvent::Closed)),
        ChatEvent::Closed,
    );
    assert_eq!(harness.chats.open_chats(), 0);
    assert_eq!(harness.chats.close(session), Err(ChatError::Unknown));
}

#[test]
fn a_prompt_for_a_chat_nobody_opened_is_refused_rather_than_started() {
    let harness = harness();

    assert_eq!(
        harness
            .chats
            .prompt(SessionId::new(7), user_message("hello")),
        Err(ChatError::Unknown),
    );
    assert_eq!(
        harness.chats.subscribe(SessionId::new(7)).err(),
        Some(ChatError::Unknown),
    );
}

/// The client stops hearing a chat it closed. The subscription ends rather than
/// staying open on a session that will never publish again, so a client parked
/// on it is released instead of waiting for a turn nobody can start.
#[test]
fn a_subscription_ends_with_the_chat_it_was_following() {
    let harness = harness();
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness.chats.close(session).expect("the chat is open");
    wait_for(&events, |event| matches!(event, ChatEvent::Closed));

    let deadline = Instant::now() + PATIENCE;
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("the subscription outlived its chat"),
            Ok(_) => continue,
        }
    }
}

/// Chats come and go for the life of the daemon, and the ones whose session has
/// already ended are dropped where the next one is opened rather than held
/// until shutdown.
#[test]
fn opening_a_chat_drops_the_records_of_the_ones_that_already_ended() {
    let harness = harness();

    let first = harness.chats.open(&request(1)).expect("the chat opens");
    // The session's own cancellation rather than the chat's: what ends a
    // session out from under its record is the daemon stopping it, and that is
    // the state pruning exists to clear.
    harness
        .supervisor
        .cancel(first)
        .expect("the session is live");

    let deadline = Instant::now() + PATIENCE;
    while !harness
        .supervisor
        .status(first)
        .is_some_and(|status| status.state.terminal().is_some())
    {
        assert!(Instant::now() < deadline, "the cancelled chat never ended");
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(harness.chats.open_chats(), 1);

    harness.chats.open(&request(2)).expect("the chat opens");

    assert_eq!(harness.chats.open_chats(), 1);
}

/// Stopping the answer you are reading is stopping that answer. A cancellation
/// that ended the chat would leave the next prompt with nowhere to arrive.
#[test]
fn cancelling_stops_the_running_turn_and_leaves_the_chat_open() {
    let harness = harness();
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness
        .chats
        .prompt(session, user_message("first"))
        .expect("the prompt is accepted");
    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("first".to_owned())
    );

    harness.chats.cancel(session).expect("the chat is open");

    assert_eq!(
        wait_for(&events, |event| matches!(
            event,
            ChatEvent::TurnFailed { .. }
        )),
        ChatEvent::TurnFailed {
            detail: "the turn was cancelled".to_owned(),
        },
    );

    assert_eq!(
        harness
            .supervisor
            .status(session)
            .map(|status| status.state),
        Some(SessionState::Running),
        "the session survives the turn it was running"
    );

    harness
        .chats
        .prompt(session, user_message("second"))
        .expect("a cancelled turn does not close the chat");
    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("second".to_owned()),
    );
}

/// A cancellation belongs to the turn that was running when it arrived. Reusing
/// one would let a person who stopped an answer stop the next one they asked
/// for without meaning to.
#[test]
fn a_cancellation_does_not_carry_into_the_next_turn() {
    let harness = harness();
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness
        .chats
        .prompt(session, user_message("first"))
        .expect("the prompt is accepted");
    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("first".to_owned())
    );
    harness.chats.cancel(session).expect("the chat is open");
    wait_for(&events, |event| {
        matches!(event, ChatEvent::TurnFailed { .. })
    });

    harness
        .chats
        .prompt(session, user_message("second"))
        .expect("the prompt is accepted");
    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("second".to_owned()),
    );

    harness
        .release
        .send(ChatTurnOutcome::Completed("an answer".to_owned()))
        .expect("the second turn is waiting");

    assert_eq!(
        wait_for(&events, |event| matches!(
            event,
            ChatEvent::TurnCompleted { .. }
        )),
        ChatEvent::TurnCompleted {
            text: "an answer".to_owned(),
        },
    );
}

/// Read back from where the session is stored rather than from the running
/// chat's memory, so a client asking mid-answer is not blocked for as long as
/// the answer takes.
#[test]
fn a_chats_conversation_is_readable_while_a_turn_is_running() {
    let harness = harness();
    let session = harness.chats.open(&request(1)).expect("the chat opens");

    harness
        .chats
        .prompt(session, user_message("first"))
        .expect("the prompt is accepted");
    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("first".to_owned())
    );

    let history = harness.chats.history(session).expect("the chat is open");

    assert_eq!(history.len(), 1);
    assert_eq!(history[0].role, agens_core::Role::User);
}

#[test]
fn the_conversation_of_a_chat_nobody_opened_is_refused() {
    let harness = harness();

    assert_eq!(
        harness.chats.history(SessionId::new(7)).err(),
        Some(ChatError::Unknown),
    );
}

fn a_question() -> ChatPermissionRequest {
    ChatPermissionRequest {
        tool: "bash".to_owned(),
        target: "cargo test".to_owned(),
        access: "execute".to_owned(),
        reason: "permission policy requires confirmation".to_owned(),
    }
}

/// The turn stops on the question, the person answers it, and the answer is
/// what the turn acts on.
#[test]
fn a_question_the_turn_asks_reaches_a_subscriber_and_the_answer_reaches_the_turn() {
    let harness = harness_asking(Some(a_question()));
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness
        .chats
        .prompt(session, user_message("run the tests"))
        .expect("the prompt is accepted");
    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("run the tests".to_owned()),
    );

    let asked = wait_for(&events, |event| {
        matches!(event, ChatEvent::PermissionAsked { .. })
    });
    let ChatEvent::PermissionAsked { prompt_id, request } = asked else {
        panic!("the chat published something else");
    };
    assert_eq!(request, a_question());

    harness
        .chats
        .answer(session, prompt_id, ChatPermissionAnswer::AllowOnce)
        .expect("the chat is waiting on it");

    assert_eq!(
        wait_for(&events, |event| matches!(
            event,
            ChatEvent::TurnCompleted { .. }
        )),
        ChatEvent::TurnCompleted {
            text: "allow_once".to_owned(),
        },
    );
}

fn an_ask_user_question() -> AskUserRequest {
    let question = AskUserQuestion::new(
        "approval",
        "Choose an outcome",
        None,
        AskUserMode::Single,
        vec![
            AskUserOption::new("approve", "Approve", None, None),
            AskUserOption::new("decline", "Decline", None, None),
        ],
        false,
        false,
        false,
    );

    AskUserRequest::new(None, vec![question]).expect("the question is valid")
}

#[test]
fn an_external_ask_user_answer_is_validated_then_continues_the_same_turn() {
    let harness = harness_asking_user(an_ask_user_question());
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness
        .chats
        .prompt(session, user_message("ask before continuing"))
        .expect("the prompt is accepted");
    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("ask before continuing".to_owned()),
    );

    let asked = wait_for(&events, |event| {
        matches!(event, ChatEvent::AskUserAsked { .. })
    });
    let ChatEvent::AskUserAsked { prompt_id, request } = asked else {
        panic!("the chat published something else");
    };
    assert_eq!(request, an_ask_user_question());

    let invalid = AskUserReply::Answered(Vec::new());
    assert_eq!(
        harness.chats.answer_ask_user(session, prompt_id, invalid),
        Err(ChatError::NotAsked),
    );

    let answer = AskUserReply::Answered(vec![AskUserAnswer {
        question_id: "approval".to_owned(),
        selected: vec!["approve".to_owned()],
        other: None,
        note: None,
    }]);
    harness
        .chats
        .answer_ask_user(session, prompt_id, answer)
        .expect("a valid structured answer is accepted");

    assert_eq!(
        wait_for(&events, |event| matches!(
            event,
            ChatEvent::TurnCompleted { .. }
        )),
        ChatEvent::TurnCompleted {
            text: "continued:approve".to_owned(),
        },
    );
}

/// Detaching is not declining. The daemon keeps the question open so the
/// person can come back to it, and a subscriber that joins while it is open is
/// greeted with it rather than attaching to a silence.
#[test]
fn an_ask_user_question_survives_every_listener_detaching_and_greets_a_new_one() {
    let harness = harness_asking_user(an_ask_user_question());
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness
        .chats
        .prompt(session, user_message("ask before continuing"))
        .expect("the prompt is accepted");
    let asked = wait_for(&events, |event| {
        matches!(event, ChatEvent::AskUserAsked { .. })
    });
    let ChatEvent::AskUserAsked { prompt_id, .. } = asked else {
        panic!("the chat published something else");
    };

    // The one client watching detaches, and the daemon notices before anybody
    // comes back.
    drop(events);
    std::thread::sleep(Duration::from_millis(200));

    let events = harness.chats.subscribe(session).expect("the chat is open");
    let replayed = wait_for(&events, |event| {
        matches!(event, ChatEvent::AskUserAsked { .. })
    });
    assert_eq!(
        replayed,
        ChatEvent::AskUserAsked {
            prompt_id,
            request: an_ask_user_question(),
        },
        "the pending question greets the subscriber that came back"
    );

    let answer = AskUserReply::Answered(vec![AskUserAnswer {
        question_id: "approval".to_owned(),
        selected: vec!["approve".to_owned()],
        other: None,
        note: None,
    }]);
    harness
        .chats
        .answer_ask_user(session, prompt_id, answer)
        .expect("the held question still takes its answer");

    assert_eq!(
        wait_for(&events, |event| matches!(
            event,
            ChatEvent::TurnCompleted { .. }
        )),
        ChatEvent::TurnCompleted {
            text: "continued:approve".to_owned(),
        },
    );
}

/// A held question must not outlive the turn that asked it: cancelling the
/// turn resolves the question as cancelled rather than leaving the chat stuck
/// on something nobody will answer.
#[test]
fn cancelling_the_turn_releases_an_unanswered_ask_user_question() {
    let harness = harness_asking_user(an_ask_user_question());
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness
        .chats
        .prompt(session, user_message("ask before continuing"))
        .expect("the prompt is accepted");
    wait_for(&events, |event| {
        matches!(event, ChatEvent::AskUserAsked { .. })
    });

    harness.chats.cancel(session).expect("the chat is open");

    assert_eq!(
        wait_for(&events, |event| matches!(
            event,
            ChatEvent::TurnCompleted { .. }
        )),
        ChatEvent::TurnCompleted {
            text: "continued:cancelled".to_owned(),
        },
    );
}

/// The bounded-answer wire the fleet console already speaks can resolve a
/// single-question ask by naming an option, so `team answer` needs no
/// structured payload for the common case.
#[test]
fn a_bare_option_id_answers_a_single_question_ask_over_the_value_wire() {
    let harness = harness_asking_user(an_ask_user_question());
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness
        .chats
        .prompt(session, user_message("ask before continuing"))
        .expect("the prompt is accepted");
    let asked = wait_for(&events, |event| {
        matches!(event, ChatEvent::AskUserAsked { .. })
    });
    let ChatEvent::AskUserAsked { prompt_id, .. } = asked else {
        panic!("the chat published something else");
    };

    assert_eq!(
        harness
            .chats
            .answer_value(session, prompt_id, "outside-domain"),
        Err(ChatError::NotAsked),
    );
    harness
        .chats
        .answer_value(session, prompt_id, "approve")
        .expect("an option id is a whole answer to a single question");

    assert_eq!(
        wait_for(&events, |event| matches!(
            event,
            ChatEvent::TurnCompleted { .. }
        )),
        ChatEvent::TurnCompleted {
            text: "continued:approve".to_owned(),
        },
    );
}

/// A question nobody can hear is refused rather than left holding the turn
/// forever. Without this a chat everybody detached from would stop for good on
/// the first tool call that needed a decision.
///
/// Observed through the next prompt rather than through the stream: a client
/// subscribed in order to watch would be a client that could answer, which is
/// the situation this is about the absence of. The second prompt only reaches
/// the turn machinery once the first turn has returned, so it arriving is what
/// says the stopped turn ended.
#[test]
fn a_question_nobody_is_listening_to_is_refused_rather_than_held() {
    let harness = harness_asking(Some(a_question()));
    let session = harness.chats.open(&request(1)).expect("the chat opens");
    let events = harness.chats.subscribe(session).expect("the chat is open");

    harness
        .chats
        .prompt(session, user_message("run the tests"))
        .expect("the prompt is accepted");
    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("run the tests".to_owned()),
    );

    wait_for(&events, |event| {
        matches!(event, ChatEvent::PermissionAsked { .. })
    });

    // The one client watching goes away without answering.
    drop(events);

    harness
        .chats
        .prompt(session, user_message("again"))
        .expect("the prompt is accepted");

    assert_eq!(
        harness.started.recv_timeout(PATIENCE),
        Ok("again".to_owned()),
        "the turn stopped on an unanswerable question ended instead of holding the chat"
    );
}

/// An answer to a question that already resolved should not be applied to
/// whatever the person is looking at now.
#[test]
fn an_answer_to_a_question_this_chat_is_not_waiting_on_is_refused() {
    let harness = harness();
    let session = harness.chats.open(&request(1)).expect("the chat opens");

    assert_eq!(
        harness
            .chats
            .answer(session, 99, ChatPermissionAnswer::AllowOnce),
        Err(ChatError::NotAsked),
    );
}

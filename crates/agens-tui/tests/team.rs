//! What the team supervision surface actually paints.

use std::time::Duration;

use agens_tui::team::{
    TeamAnswer, TeamAttempt, TeamCommand, TeamEvent, TeamEventClass, TeamInboxItem, TeamNode,
    TeamNodeDetail, TeamQuestion, TeamRepo, TeamScreen, TeamSnapshot, TeamState, TeamSurface,
};
use agens_tui::{ColorLevel, Key, UnicodeLevel};
use ratatui::{Terminal, backend::TestBackend};

fn screen(width: u16, height: u16) -> TeamScreen<TestBackend> {
    screen_at(width, height, UnicodeLevel::Extended)
}

fn screen_at(width: u16, height: u16, level: UnicodeLevel) -> TeamScreen<TestBackend> {
    let terminal = Terminal::new(TestBackend::new(width, height)).expect("the test backend opens");

    TeamScreen::new(terminal, ColorLevel::TrueColor, level)
}

fn rendered(screen: &TeamScreen<TestBackend>) -> String {
    let buffer = screen.terminal().backend().buffer();
    let width = usize::from(buffer.area.width);

    buffer
        .content
        .chunks(width)
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fleet() -> TeamSnapshot {
    TeamSnapshot {
        inbox: Vec::new(),
        repos: vec![
            TeamRepo {
                id: "agens".to_owned(),
                label: "agens".to_owned(),
                nodes: vec![
                    TeamNode {
                        attempt: Some(2),
                        model: Some("gpt-5.5".to_owned()),
                        cost_micros: Some(420_000),
                        duration: Some(Duration::from_secs(95)),
                        ..TeamNode::run(11, "ship the api", TeamState::Running)
                    },
                    TeamNode {
                        parent: Some(11),
                        waiting: Some("merge authorization".to_owned()),
                        parked_for: Some(Duration::from_secs(240)),
                        ..TeamNode::run(12, "review it", TeamState::AwaitingInput)
                    },
                    TeamNode::run(13, "port the store", TeamState::AwaitingQuota),
                    TeamNode::run(14, "the next one", TeamState::Draft),
                ],
            },
            TeamRepo {
                id: "harness".to_owned(),
                label: "harness".to_owned(),
                nodes: vec![TeamNode {
                    is_self: true,
                    ..TeamNode::chat(90, "~/dev/harness", TeamState::Answering)
                }],
            },
        ],
    }
}

#[test]
fn the_board_names_every_repository_and_every_node_under_it() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("agens"), "{text}");
    assert!(text.contains("harness"), "{text}");
    assert!(text.contains("ship the api"), "{text}");
    assert!(text.contains("review it"), "{text}");
    assert!(text.contains("~/dev/harness"), "{text}");
}

#[test]
fn each_state_carries_its_own_glyph_so_parked_never_reads_as_running() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("● 11"), "{text}");
    assert!(text.contains("◐ 12"), "{text}");
    assert!(text.contains("⏱ 13"), "{text}");
    assert!(text.contains("◇ 14"), "{text}");
    assert!(text.contains("◆ 90"), "{text}");
}

#[test]
fn a_parked_node_says_what_holds_it_and_for_how_long() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(
        text.contains("merge authorization · parked 4m 00s"),
        "{text}"
    );
    assert!(text.contains("quota"), "{text}");
}

#[test]
fn a_node_carries_its_attempt_model_and_cost() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(
        text.contains("attempt 2 · gpt-5.5 · $0.42 · 1m 35s"),
        "{text}"
    );
}

#[test]
fn the_readers_own_session_is_a_node_like_any_other() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("chat 90") || text.contains("◆ 90"), "{text}");
    assert!(text.contains("(this terminal)"), "{text}");
}

#[test]
fn the_header_counts_the_fleet_and_says_how_much_of_it_is_parked() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("4 runs · 1 chats"), "{text}");
    assert!(text.contains("2 parked"), "{text}");
}

#[test]
fn the_conversation_is_tab_zero_and_the_tree_is_tab_one() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("[0 chat] [1 tree]"), "{text}");
}

#[test]
fn the_footer_names_the_keys_the_board_answers_to() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("↑↓ move"), "{text}");
    assert!(text.contains("⏎ detail"), "{text}");
    assert!(text.contains("0 chat"), "{text}");
}

#[test]
fn an_ascii_terminal_gets_the_same_board_at_the_same_columns() {
    let mut screen = screen_at(100, 30, UnicodeLevel::Ascii);
    let surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("o 11"), "{text}");
    assert!(text.contains("? 12"), "{text}");
    assert!(text.contains("% 13"), "{text}");
    assert!(text.contains("up/down move"), "{text}");
    assert!(!text.contains('⏱'), "{text}");
}

#[test]
fn the_context_panel_describes_whatever_the_reader_is_standing_on() {
    let mut screen = screen(100, 30);
    let mut surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");
    assert!(
        rendered(&screen).contains("run 11"),
        "{}",
        rendered(&screen)
    );

    surface.handle_key(Key::Down);
    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("run 12"), "{text}");
    assert!(text.contains("awaiting_input"), "{text}");
}

#[test]
fn a_node_whose_detail_has_not_arrived_says_so_instead_of_looking_empty() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");

    assert!(
        rendered(&screen).contains("reading the run"),
        "{}",
        rendered(&screen)
    );
}

fn detail() -> TeamNodeDetail {
    TeamNodeDetail {
        node_id: 11,
        task: "ship the api".to_owned(),
        scope: "crates/agens-api".to_owned(),
        definition_of_done: "the gate is green".to_owned(),
        worktree: Some("/w/agn-11".to_owned()),
        attempts: vec![
            TeamAttempt {
                n: 1,
                outcome: Some("failed".to_owned()),
                tokens: Some(12_000),
                cost_micros: Some(180_000),
                duration: Some(Duration::from_secs(600)),
            },
            TeamAttempt {
                n: 2,
                outcome: None,
                tokens: None,
                cost_micros: Some(240_000),
                duration: None,
            },
        ],
        questions: vec![TeamQuestion {
            question_id: 5,
            run_id: 11,
            kind: "approval".to_owned(),
            blocked_decision: "merge the branch".to_owned(),
            options: vec!["merge".to_owned()],
            recommendation: None,
        }],
        events: vec![
            TeamEvent {
                class: TeamEventClass::Agent,
                kind: "tool_call".to_owned(),
                payload: "{\"name\":\"shell\"}".to_owned(),
                ts: 10,
            },
            TeamEvent {
                class: TeamEventClass::Infra,
                kind: "quota_reached".to_owned(),
                payload: "{\"provider\":\"openai\"}".to_owned(),
                ts: 11,
            },
        ],
    }
}

#[test]
fn the_context_panel_shows_the_attempts_and_the_open_questions() {
    let mut screen = screen(100, 30);
    let mut surface = TeamSurface::new(fleet());
    surface.set_detail(detail());

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("attempts"), "{text}");
    assert!(text.contains("1 failed · $0.18 · 10m 00s"), "{text}");
    assert!(text.contains("2 running · $0.24"), "{text}");
    assert!(text.contains("5 merge authorization"), "{text}");
}

#[test]
fn enter_opens_the_full_detail_with_the_journal_split_by_class() {
    let mut screen = screen(100, 30);
    let mut surface = TeamSurface::new(fleet());
    surface.set_detail(detail());

    screen.draw(&surface).expect("the board draws");
    assert!(!rendered(&screen).contains("quota_reached"));

    surface.handle_key(Key::Enter);
    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("infra"), "{text}");
    assert!(text.contains("quota_reached"), "{text}");
    assert!(text.contains("agent"), "{text}");
    assert!(text.contains("tool_call"), "{text}");
    assert!(text.contains("/w/agn-11"), "{text}");
}

#[test]
fn an_empty_fleet_says_the_daemon_holds_nothing_rather_than_drawing_a_blank() {
    let mut screen = screen(80, 20);
    let surface = TeamSurface::new(TeamSnapshot::default());

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("the daemon holds no runs or chats"), "{text}");
    assert!(text.contains("nothing selected"), "{text}");
}

fn inbox() -> Vec<TeamInboxItem> {
    vec![
        TeamInboxItem {
            repo_id: "agens".to_owned(),
            run_id: 12,
            question_id: 5,
            kind: "approval".to_owned(),
            blocked_decision: "merge the branch".to_owned(),
            options: vec!["merge".to_owned(), "reject".to_owned()],
            recommendation: Some("merge".to_owned()),
            age: Some(Duration::from_secs(240)),
        },
        TeamInboxItem {
            repo_id: "agens".to_owned(),
            run_id: 13,
            question_id: 6,
            kind: "question".to_owned(),
            blocked_decision: "which store to port first".to_owned(),
            options: vec!["sessions".to_owned(), "questions".to_owned()],
            recommendation: None,
            age: Some(Duration::from_secs(30)),
        },
    ]
}

fn fleet_with_inbox() -> TeamSnapshot {
    TeamSnapshot {
        inbox: inbox(),
        ..fleet()
    }
}

#[test]
fn the_header_badges_how_many_answers_the_fleet_is_waiting_for() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(fleet_with_inbox());

    screen.draw(&surface).expect("the board draws");
    let header = rendered(&screen)
        .lines()
        .next()
        .expect("the header is drawn")
        .to_owned();

    assert!(header.contains("inbox 2"), "{header}");
}

#[test]
fn an_empty_inbox_badges_nothing_rather_than_a_zero() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(fleet());

    screen.draw(&surface).expect("the board draws");
    let header = rendered(&screen)
        .lines()
        .next()
        .expect("the header is drawn")
        .to_owned();

    assert!(!header.contains("inbox"), "{header}");
}

#[test]
fn the_inbox_tab_lists_every_question_with_the_decision_it_blocks() {
    let mut screen = screen(100, 30);
    let mut surface = TeamSurface::new(fleet_with_inbox());
    surface.handle_key(Key::Char('2'));

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("[2 inbox]"), "{text}");
    assert!(text.contains("merge authorization"), "{text}");
    assert!(text.contains("merge the branch"), "{text}");
    assert!(text.contains("which store to port first"), "{text}");
    assert!(text.contains("run 12"), "{text}");
    assert!(text.contains("merge | reject"), "{text}");
}

#[test]
fn an_empty_inbox_says_so_instead_of_drawing_a_blank_pane() {
    let mut screen = screen(100, 30);
    let mut surface = TeamSurface::new(fleet());
    surface.handle_key(Key::Char('2'));

    screen.draw(&surface).expect("the board draws");

    assert!(
        rendered(&screen).contains("nothing is waiting on you"),
        "{}",
        rendered(&screen)
    );
}

#[test]
fn answering_opens_from_the_tree_on_the_question_the_selected_node_is_parked_on() {
    let mut screen = screen(100, 30);
    let mut surface = TeamSurface::new(fleet_with_inbox());
    surface.handle_key(Key::Down);

    assert_eq!(surface.handle_key(Key::Char('a')), TeamCommand::Handled);
    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("answer question 5"), "{text}");
    assert!(text.contains("merge the branch"), "{text}");
    assert!(text.contains("merge"), "{text}");
}

#[test]
fn answering_a_node_with_no_open_question_is_refused_rather_than_guessed_at() {
    let mut surface = TeamSurface::new(fleet_with_inbox());

    assert_eq!(surface.handle_key(Key::Char('a')), TeamCommand::Ignored);
}

#[test]
fn the_chosen_option_is_handed_back_with_the_question_it_answers() {
    let mut surface = TeamSurface::new(fleet_with_inbox());
    surface.handle_key(Key::Char('2'));
    surface.handle_key(Key::Char('a'));
    surface.handle_key(Key::Down);

    let command = surface.handle_key(Key::Enter);

    assert_eq!(
        command,
        TeamCommand::Answer(TeamAnswer {
            question_id: 5,
            run_id: 12,
            kind: "approval".to_owned(),
            answer: "reject".to_owned(),
        })
    );
}

#[test]
fn escape_abandons_an_answer_without_sending_one() {
    let mut surface = TeamSurface::new(fleet_with_inbox());
    surface.handle_key(Key::Char('2'));
    surface.handle_key(Key::Char('a'));

    assert_eq!(surface.handle_key(Key::Escape), TeamCommand::Handled);
    assert_eq!(surface.handle_key(Key::Escape), TeamCommand::LeaveToChat);
}

#[test]
fn an_approval_is_recognised_as_a_merge_authorization_and_a_question_is_not() {
    let merge = TeamAnswer {
        question_id: 5,
        run_id: 12,
        kind: "approval".to_owned(),
        answer: "merge".to_owned(),
    };
    let asked = TeamAnswer {
        kind: "question".to_owned(),
        ..merge.clone()
    };

    assert!(merge.is_approval());
    assert!(!asked.is_approval());
}

#[test]
fn a_repository_is_headed_by_its_label_rather_than_its_fingerprint() {
    let mut screen = screen(100, 30);
    let surface = TeamSurface::new(TeamSnapshot {
        inbox: Vec::new(),
        repos: vec![TeamRepo {
            id: "a1b2c3d4e5f60718".to_owned(),
            label: "agens".to_owned(),
            nodes: vec![TeamNode::run(11, "ship the api", TeamState::Running)],
        }],
    });

    screen.draw(&surface).expect("the board draws");
    let text = rendered(&screen);

    assert!(text.contains("agens"), "{text}");
    assert!(!text.contains("a1b2c3d4e5f60718"), "{text}");
}

#[test]
fn a_reading_that_failed_is_said_out_loud_rather_than_swallowed() {
    let mut screen = screen(100, 30);
    let mut surface = TeamSurface::new(fleet());
    surface.set_notice(Some("the daemon stopped answering".to_owned()));

    screen.draw(&surface).expect("the board draws");

    assert!(
        rendered(&screen).contains("the daemon stopped answering"),
        "{}",
        rendered(&screen)
    );
}

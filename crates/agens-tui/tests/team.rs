//! What the team supervision surface actually paints.

use std::time::Duration;

use agens_tui::team::{
    TeamAttempt, TeamEvent, TeamEventClass, TeamNode, TeamNodeDetail, TeamQuestion, TeamRepo,
    TeamScreen, TeamSnapshot, TeamState, TeamSurface,
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
        repos: vec![
            TeamRepo {
                id: "agens".to_owned(),
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

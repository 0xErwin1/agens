//! The team supervision surface: the fleet as a tree, read-only by default.
//!
//! The surface owns no connection. Whatever holds one hands it a
//! [`TeamSnapshot`] and, for the selected node, a [`TeamNodeDetail`]; the
//! surface decides what is selected, which tab is showing, and what the reader
//! just asked for. That keeps every rule about how the board reads testable
//! without a daemon.
//!
//! Inspection is the default and the only thing wired here. Nothing on this
//! surface writes to a run.

mod model;
mod render;

pub use model::{
    TeamAttempt, TeamEvent, TeamEventClass, TeamInboxItem, TeamLens, TeamLogLine, TeamNode,
    TeamNodeDetail, TeamNodeKind, TeamQuestion, TeamRepo, TeamSnapshot, TeamState, format_cost,
    format_duration, waiting_label_for_kind,
};
pub use render::TeamScreen;

use std::collections::VecDeque;

use crate::Key;

/// How many journal lines the wall holds before the oldest scroll off.
///
/// The fleet's journal is unbounded and the board is long-lived, so the wall is
/// a window on it rather than a transcript of it.
pub(crate) const LOG_CAPACITY: usize = 500;

/// Which tab the surface is showing.
///
/// Zero is the conversation, and it is not drawn here: the chat is the main
/// entry and this surface is what a reader steps out to. Selecting it is a
/// request to leave, which is why it is a command rather than a view.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TeamTab {
    #[default]
    Tree,
    Inbox,
    Logs,
}

impl TeamTab {
    /// The tabs in the order the header shows them, the chat included.
    pub(crate) const HEADER: [(u8, &'static str); 4] =
        [(0, "chat"), (1, "tree"), (2, "inbox"), (3, "logs")];

    pub(crate) const fn digit(self) -> u8 {
        match self {
            Self::Tree => 1,
            Self::Inbox => 2,
            Self::Logs => 3,
        }
    }
}

/// An answer the reader composed, for the host to deliver.
///
/// The surface never writes to a run. It says which question was answered and
/// with what, and whether that answer authorizes a merge or answers a question,
/// because those are different calls on the daemon.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamAnswer {
    pub question_id: i64,
    pub run_id: i64,
    pub kind: String,
    pub answer: String,
}

impl TeamAnswer {
    #[must_use]
    pub fn is_approval(&self) -> bool {
        self.kind == "approval"
    }
}

/// The open answer prompt: one question, and which of its options is picked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AnswerPrompt {
    pub(crate) question: TeamInboxItem,
    pub(crate) option: usize,
}

impl AnswerPrompt {
    fn new(question: TeamInboxItem) -> Self {
        Self {
            question,
            option: 0,
        }
    }

    fn step(&mut self, delta: isize) {
        let last = self.question.options.len().saturating_sub(1);
        self.option = self.option.saturating_add_signed(delta).min(last);
    }

    fn answer(&self) -> Option<TeamAnswer> {
        let answer = self.question.options.get(self.option)?;

        Some(TeamAnswer {
            question_id: self.question.question_id,
            run_id: self.question.run_id,
            kind: self.question.kind.clone(),
            answer: answer.clone(),
        })
    }
}

/// What the reader asked the host to do, once the surface has done everything
/// it can do on its own.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TeamCommand {
    /// The key moved something inside the surface and needs nothing else.
    Handled,
    /// The key means nothing here.
    Ignored,
    /// A different node is selected; its detail is worth fetching.
    Selected(i64),
    /// Back to the conversation.
    LeaveToChat,
    /// The reader answered a question the fleet was stopped on.
    Answer(TeamAnswer),
}

/// One row of the tree as it is drawn: a repository header, or a node under it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TeamRow<'a> {
    Repo(&'a str),
    Node { node: &'a TeamNode, depth: usize },
}

/// The supervision surface's own state.
#[derive(Clone, Debug, Default)]
pub struct TeamSurface {
    snapshot: TeamSnapshot,
    tab: TeamTab,
    /// Which selectable row the reader is standing on, by node id.
    selected: Option<i64>,
    /// The selected node's detail, once the host has fetched it.
    detail: Option<TeamNodeDetail>,
    /// Whether the detail owns the whole frame instead of the context panel.
    expanded: bool,
    /// Which inbox row the reader is standing on.
    inbox_selected: usize,
    /// The open answer prompt, which any view can raise.
    answering: Option<AnswerPrompt>,
    /// What went wrong last, when something did.
    notice: Option<String>,
    /// The fleet's journal as it arrives, oldest first, newest last.
    logs: VecDeque<TeamLogLine>,
    /// How much of that journal the wall is showing.
    lens: TeamLens,
}

impl TeamSurface {
    #[must_use]
    pub fn new(snapshot: TeamSnapshot) -> Self {
        let selected = first_node_id(&snapshot);

        Self {
            snapshot,
            tab: TeamTab::default(),
            selected,
            detail: None,
            expanded: false,
            inbox_selected: 0,
            answering: None,
            notice: None,
            logs: VecDeque::new(),
            lens: TeamLens::default(),
        }
    }

    /// Replaces the fleet with a newer reading, keeping the reader where they
    /// were whenever that node still exists.
    pub fn refresh(&mut self, snapshot: TeamSnapshot) {
        let kept = self
            .selected
            .filter(|id| snapshot.node(*id).is_some())
            .or_else(|| first_node_id(&snapshot));

        if kept != self.selected {
            self.detail = None;
        }
        self.inbox_selected = self
            .inbox_selected
            .min(snapshot.inbox.len().saturating_sub(1));
        self.snapshot = snapshot;
        self.selected = kept;
    }

    /// Hands the surface what the host read back for one node. A reply about
    /// a node the reader has already moved off is dropped rather than shown
    /// under the wrong title.
    pub fn set_detail(&mut self, detail: TeamNodeDetail) {
        if self.selected == Some(detail.node_id) {
            self.detail = Some(detail);
        }
    }

    #[must_use]
    pub const fn detail(&self) -> Option<&TeamNodeDetail> {
        self.detail.as_ref()
    }

    #[must_use]
    pub const fn is_expanded(&self) -> bool {
        self.expanded
    }

    #[must_use]
    pub const fn inbox_selected(&self) -> usize {
        self.inbox_selected
    }

    pub(crate) const fn answering(&self) -> Option<&AnswerPrompt> {
        self.answering.as_ref()
    }

    /// Says what went wrong, so a board that stopped refreshing does not keep
    /// showing a stale fleet as if it were current.
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Records one line of the fleet's journal, dropping the oldest once the
    /// window is full.
    pub fn push_log(&mut self, line: TeamLogLine) {
        if self.logs.len() == LOG_CAPACITY {
            self.logs.pop_front();
        }

        self.logs.push_back(line);
    }

    /// The journal as the current lens admits it, oldest first.
    pub fn logs(&self) -> impl Iterator<Item = &TeamLogLine> {
        self.logs.iter().filter(|line| self.lens.admits(line))
    }

    #[must_use]
    pub const fn lens(&self) -> TeamLens {
        self.lens
    }

    #[must_use]
    pub const fn tab(&self) -> TeamTab {
        self.tab
    }

    #[must_use]
    pub const fn selected(&self) -> Option<i64> {
        self.selected
    }

    #[must_use]
    pub fn selected_node(&self) -> Option<&TeamNode> {
        self.selected.and_then(|id| self.snapshot.node(id))
    }

    #[must_use]
    pub const fn snapshot(&self) -> &TeamSnapshot {
        &self.snapshot
    }

    /// Applies one key press.
    ///
    /// An open answer prompt owns the keyboard: it is the one thing on this
    /// surface that writes anywhere, so no key reaches the board behind it.
    pub fn handle_key(&mut self, key: Key) -> TeamCommand {
        if self.answering.is_some() {
            return self.answer_key(key);
        }

        match key {
            Key::Up | Key::CtrlK => self.step(-1),
            Key::Down | Key::CtrlJ => self.step(1),
            Key::Enter => self.toggle_expanded(),
            Key::Char('a') => self.open_answer(),
            Key::Escape if self.expanded => self.toggle_expanded(),
            Key::Char('0') | Key::Escape => TeamCommand::LeaveToChat,
            Key::Char('1') => self.show(TeamTab::Tree),
            Key::Char('2') => self.show(TeamTab::Inbox),
            Key::Char('3') => self.show(TeamTab::Logs),
            Key::Char('l') if self.tab == TeamTab::Logs => {
                self.lens = self.lens.next();
                TeamCommand::Handled
            }
            _ => TeamCommand::Ignored,
        }
    }

    fn answer_key(&mut self, key: Key) -> TeamCommand {
        let Some(prompt) = self.answering.as_mut() else {
            return TeamCommand::Ignored;
        };

        match key {
            Key::Up | Key::CtrlK => {
                prompt.step(-1);
                TeamCommand::Handled
            }
            Key::Down | Key::CtrlJ => {
                prompt.step(1);
                TeamCommand::Handled
            }
            Key::Enter => match prompt.answer() {
                Some(answer) => {
                    self.answering = None;
                    TeamCommand::Answer(answer)
                }
                None => TeamCommand::Ignored,
            },
            Key::Escape => {
                self.answering = None;
                TeamCommand::Handled
            }
            _ => TeamCommand::Ignored,
        }
    }

    /// Raises the answer prompt for whatever the current view is pointing at:
    /// the selected inbox row, or the question the selected node is parked on.
    fn open_answer(&mut self) -> TeamCommand {
        let question = match self.tab {
            TeamTab::Inbox => self.snapshot.inbox.get(self.inbox_selected),
            TeamTab::Tree => self.selected.and_then(|id| self.snapshot.question_for(id)),
            // The wall selects nothing, so answering from it means answering
            // whatever is waiting. Showing the inbox as it opens is how the
            // reader sees which question that turned out to be.
            TeamTab::Logs => self.snapshot.inbox.get(self.inbox_selected),
        };
        let Some(question) = question.cloned() else {
            return TeamCommand::Ignored;
        };

        if self.tab == TeamTab::Logs {
            self.tab = TeamTab::Inbox;
        }
        self.answering = Some(AnswerPrompt::new(question));
        TeamCommand::Handled
    }

    /// Opens or closes the full-frame detail of the selected node.
    fn toggle_expanded(&mut self) -> TeamCommand {
        if self.tab == TeamTab::Logs || self.selected.is_none() {
            return TeamCommand::Ignored;
        }

        self.expanded = !self.expanded;
        TeamCommand::Handled
    }

    fn show(&mut self, tab: TeamTab) -> TeamCommand {
        self.tab = tab;
        TeamCommand::Handled
    }

    /// Moves the selection by whole nodes, ignoring the repository headers
    /// between them, and stops at both ends rather than wrapping.
    fn step(&mut self, delta: isize) -> TeamCommand {
        match self.tab {
            TeamTab::Inbox => return self.step_inbox(delta),
            // The wall tails the journal. Moving here would silently walk the
            // tree behind it, and fetch details for a node nobody can see.
            TeamTab::Logs => return TeamCommand::Ignored,
            TeamTab::Tree => {}
        }

        let ids: Vec<i64> = self
            .snapshot
            .repos
            .iter()
            .flat_map(|repo| &repo.nodes)
            .map(|node| node.id)
            .collect();
        let Some(current) = self
            .selected
            .and_then(|id| ids.iter().position(|candidate| *candidate == id))
        else {
            return match ids.first() {
                Some(first) => {
                    self.selected = Some(*first);
                    TeamCommand::Selected(*first)
                }
                None => TeamCommand::Ignored,
            };
        };

        let next = current
            .saturating_add_signed(delta)
            .min(ids.len().saturating_sub(1));
        let Some(id) = ids.get(next) else {
            return TeamCommand::Ignored;
        };

        if Some(*id) == self.selected {
            return TeamCommand::Handled;
        }

        self.selected = Some(*id);
        self.detail = None;
        TeamCommand::Selected(*id)
    }

    fn step_inbox(&mut self, delta: isize) -> TeamCommand {
        if self.snapshot.inbox.is_empty() {
            return TeamCommand::Ignored;
        }

        self.inbox_selected = self
            .inbox_selected
            .saturating_add_signed(delta)
            .min(self.snapshot.inbox.len() - 1);
        TeamCommand::Handled
    }

    /// The rows the tree draws, repository headers included.
    pub(crate) fn rows(&self) -> Vec<TeamRow<'_>> {
        let mut rows = Vec::new();

        for repo in &self.snapshot.repos {
            rows.push(TeamRow::Repo(&repo.label));

            for node in &repo.nodes {
                let depth = usize::from(
                    node.parent
                        .is_some_and(|parent| repo.nodes.iter().any(|other| other.id == parent)),
                );
                rows.push(TeamRow::Node { node, depth });
            }
        }

        rows
    }
}

fn first_node_id(snapshot: &TeamSnapshot) -> Option<i64> {
    snapshot
        .repos
        .iter()
        .flat_map(|repo| &repo.nodes)
        .map(|node| node.id)
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fleet() -> TeamSnapshot {
        TeamSnapshot {
            inbox: Vec::new(),
            repos: vec![
                TeamRepo {
                    id: "agens".to_owned(),
                    label: "agens".to_owned(),
                    nodes: vec![
                        TeamNode::run(11, "ship the api", TeamState::Running),
                        TeamNode {
                            parent: Some(11),
                            ..TeamNode::run(12, "review it", TeamState::AwaitingInput)
                        },
                    ],
                },
                TeamRepo {
                    id: "harness".to_owned(),
                    label: "harness".to_owned(),
                    nodes: vec![TeamNode::chat(90, "~/dev/harness", TeamState::Idle)],
                },
            ],
        }
    }

    #[test]
    fn the_first_node_is_selected_so_the_board_opens_on_something() {
        let surface = TeamSurface::new(fleet());

        assert_eq!(surface.selected(), Some(11));
        assert_eq!(surface.tab(), TeamTab::Tree);
    }

    #[test]
    fn moving_walks_nodes_across_repositories_and_stops_at_the_ends() {
        let mut surface = TeamSurface::new(fleet());

        assert_eq!(surface.handle_key(Key::Up), TeamCommand::Handled);
        assert_eq!(surface.selected(), Some(11));
        assert_eq!(surface.handle_key(Key::Down), TeamCommand::Selected(12));
        assert_eq!(surface.handle_key(Key::Down), TeamCommand::Selected(90));
        assert_eq!(surface.handle_key(Key::Down), TeamCommand::Handled);
        assert_eq!(surface.selected(), Some(90));
    }

    #[test]
    fn a_refresh_keeps_the_reader_on_the_node_they_were_reading() {
        let mut surface = TeamSurface::new(fleet());
        surface.handle_key(Key::Down);

        surface.refresh(fleet());

        assert_eq!(surface.selected(), Some(12));
    }

    #[test]
    fn a_refresh_that_drops_the_selected_node_falls_back_to_the_first() {
        let mut surface = TeamSurface::new(fleet());
        surface.handle_key(Key::Down);

        surface.refresh(TeamSnapshot {
            inbox: Vec::new(),
            repos: vec![TeamRepo {
                id: "agens".to_owned(),
                label: "agens".to_owned(),
                nodes: vec![TeamNode::run(11, "ship the api", TeamState::Running)],
            }],
        });

        assert_eq!(surface.selected(), Some(11));
    }

    #[test]
    fn zero_is_the_conversation_and_leaves_the_board() {
        let mut surface = TeamSurface::new(fleet());

        assert_eq!(surface.handle_key(Key::Char('0')), TeamCommand::LeaveToChat);
        assert_eq!(surface.handle_key(Key::Escape), TeamCommand::LeaveToChat);
    }

    #[test]
    fn the_numbered_tab_selects_the_view_it_names() {
        let mut surface = TeamSurface::new(fleet());

        assert_eq!(surface.handle_key(Key::Char('1')), TeamCommand::Handled);
        assert_eq!(surface.tab(), TeamTab::Tree);
    }

    #[test]
    fn a_child_run_is_drawn_under_the_run_it_came_from() {
        let surface = TeamSurface::new(fleet());
        let rows = surface.rows();

        assert!(matches!(rows.first(), Some(TeamRow::Repo("agens"))));
        assert!(matches!(
            rows.get(1),
            Some(TeamRow::Node { node, depth: 0 }) if node.id == 11
        ));
        assert!(matches!(
            rows.get(2),
            Some(TeamRow::Node { node, depth: 1 }) if node.id == 12
        ));
        assert!(matches!(rows.get(3), Some(TeamRow::Repo("harness"))));
    }

    #[test]
    fn enter_opens_the_detail_of_the_selected_node_and_escape_closes_it() {
        let mut surface = TeamSurface::new(fleet());

        assert!(!surface.is_expanded());
        assert_eq!(surface.handle_key(Key::Enter), TeamCommand::Handled);
        assert!(surface.is_expanded());
        assert_eq!(surface.handle_key(Key::Escape), TeamCommand::Handled);
        assert!(!surface.is_expanded());
        assert_eq!(surface.handle_key(Key::Escape), TeamCommand::LeaveToChat);
    }

    #[test]
    fn a_detail_about_a_node_the_reader_left_is_dropped_rather_than_mislabelled() {
        let mut surface = TeamSurface::new(fleet());
        surface.handle_key(Key::Down);

        surface.set_detail(TeamNodeDetail {
            node_id: 11,
            ..TeamNodeDetail::default()
        });

        assert_eq!(surface.detail(), None);

        surface.set_detail(TeamNodeDetail {
            node_id: 12,
            task: "review it".to_owned(),
            ..TeamNodeDetail::default()
        });

        assert_eq!(surface.detail().map(|detail| detail.node_id), Some(12));
    }

    #[test]
    fn moving_off_a_node_drops_the_detail_that_belonged_to_it() {
        let mut surface = TeamSurface::new(fleet());
        surface.set_detail(TeamNodeDetail {
            node_id: 11,
            ..TeamNodeDetail::default()
        });

        surface.handle_key(Key::Down);

        assert_eq!(surface.detail(), None);
    }

    fn log(kind: &str, class: TeamEventClass, ts: i64) -> TeamLogLine {
        TeamLogLine {
            repo: "agens".to_owned(),
            run_id: Some(11),
            class,
            kind: kind.to_owned(),
            payload: String::new(),
            ts,
        }
    }

    #[test]
    fn the_wall_keeps_the_newest_lines_and_drops_what_scrolled_off() {
        let mut surface = TeamSurface::new(fleet());

        for index in 0..(LOG_CAPACITY + 5) {
            let ts = i64::try_from(index).unwrap();
            surface.push_log(log("turn_started", TeamEventClass::Agent, ts));
        }

        let kept: Vec<i64> = surface.logs().map(|line| line.ts).collect();

        assert_eq!(kept.len(), LOG_CAPACITY);
        assert_eq!(kept.first(), Some(&5));
        assert_eq!(kept.last(), Some(&i64::try_from(LOG_CAPACITY + 4).unwrap()));
    }

    #[test]
    fn the_lens_cycles_through_each_class_and_back_to_the_whole_journal() {
        let mut surface = TeamSurface::new(fleet());
        surface.push_log(log("turn_started", TeamEventClass::Agent, 1));
        surface.push_log(log("worktree_created", TeamEventClass::Infra, 2));
        surface.handle_key(Key::Char('3'));

        assert_eq!(surface.lens(), TeamLens::Everything);
        assert_eq!(surface.logs().count(), 2);

        assert_eq!(surface.handle_key(Key::Char('l')), TeamCommand::Handled);
        assert_eq!(surface.lens(), TeamLens::Class(TeamEventClass::Agent));
        assert_eq!(surface.logs().count(), 1);

        surface.handle_key(Key::Char('l'));
        assert_eq!(surface.lens(), TeamLens::Class(TeamEventClass::Infra));

        surface.handle_key(Key::Char('l'));
        assert_eq!(surface.lens(), TeamLens::Everything);
    }

    #[test]
    fn the_lens_key_means_nothing_where_there_is_no_wall_to_narrow() {
        let mut surface = TeamSurface::new(fleet());

        assert_eq!(surface.handle_key(Key::Char('l')), TeamCommand::Ignored);
        assert_eq!(surface.lens(), TeamLens::Everything);
    }

    #[test]
    fn an_empty_fleet_has_nothing_selected_and_nothing_to_move_to() {
        let mut surface = TeamSurface::new(TeamSnapshot::default());

        assert_eq!(surface.selected(), None);
        assert_eq!(surface.handle_key(Key::Down), TeamCommand::Ignored);
    }
}

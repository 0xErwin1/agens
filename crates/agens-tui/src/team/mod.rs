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
    TeamAttempt, TeamEvent, TeamEventClass, TeamNode, TeamNodeDetail, TeamNodeKind, TeamQuestion,
    TeamRepo, TeamSnapshot, TeamState, format_cost, format_duration,
};
pub use render::TeamScreen;

use crate::Key;

/// Which tab the surface is showing.
///
/// Zero is the conversation, and it is not drawn here: the chat is the main
/// entry and this surface is what a reader steps out to. Selecting it is a
/// request to leave, which is why it is a command rather than a view.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TeamTab {
    #[default]
    Tree,
}

impl TeamTab {
    /// The tabs in the order the header shows them, the chat included.
    pub(crate) const HEADER: [(u8, &'static str); 2] = [(0, "chat"), (1, "tree")];

    pub(crate) const fn digit(self) -> u8 {
        match self {
            Self::Tree => 1,
        }
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
    pub fn handle_key(&mut self, key: Key) -> TeamCommand {
        match key {
            Key::Up | Key::CtrlK => self.step(-1),
            Key::Down | Key::CtrlJ => self.step(1),
            Key::Enter => self.toggle_expanded(),
            Key::Escape if self.expanded => self.toggle_expanded(),
            Key::Char('0') | Key::Escape => TeamCommand::LeaveToChat,
            Key::Char('1') => self.show(TeamTab::Tree),
            _ => TeamCommand::Ignored,
        }
    }

    /// Opens or closes the full-frame detail of the selected node.
    fn toggle_expanded(&mut self) -> TeamCommand {
        if self.selected.is_none() {
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

    /// The rows the tree draws, repository headers included.
    pub(crate) fn rows(&self) -> Vec<TeamRow<'_>> {
        let mut rows = Vec::new();

        for repo in &self.snapshot.repos {
            rows.push(TeamRow::Repo(&repo.id));

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
            repos: vec![
                TeamRepo {
                    id: "agens".to_owned(),
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
            repos: vec![TeamRepo {
                id: "agens".to_owned(),
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

    #[test]
    fn an_empty_fleet_has_nothing_selected_and_nothing_to_move_to() {
        let mut surface = TeamSurface::new(TeamSnapshot::default());

        assert_eq!(surface.selected(), None);
        assert_eq!(surface.handle_key(Key::Down), TeamCommand::Ignored);
    }
}

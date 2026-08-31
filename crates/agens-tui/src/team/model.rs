//! What the supervision surface knows about the fleet, independent of how it
//! is painted.
//!
//! Every value here arrives from the daemon's read plane as a string or an
//! integer. The vocabulary that turns those into something a reader can scan —
//! the per-state glyph, the parked label, the cost — lives here so the terminal
//! and any other consumer describe the same fleet the same way.

use std::time::Duration;

use crate::widgets::UnicodeLevel;

/// Where one node of the fleet sits right now.
///
/// The run states are the daemon's own, parked ones included. A chat is not a
/// run and has no lifecycle beyond whether it is mid-answer, so it carries
/// [`TeamState::Answering`] or [`TeamState::Idle`] instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TeamState {
    Draft,
    Queued,
    Running,
    AwaitingInput,
    AwaitingQuota,
    Done,
    Failed,
    Interrupted,
    Cancelled,
    Answering,
    Idle,
    /// A state this build does not know, kept verbatim rather than guessed at.
    Other(String),
}

impl TeamState {
    /// Reads the daemon's own spelling of a state.
    #[must_use]
    pub fn parse(state: &str) -> Self {
        match state {
            "draft" => Self::Draft,
            "queued" => Self::Queued,
            "running" => Self::Running,
            "awaiting_input" => Self::AwaitingInput,
            "awaiting_quota" => Self::AwaitingQuota,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "interrupted" => Self::Interrupted,
            "cancelled" => Self::Cancelled,
            "answering" => Self::Answering,
            "idle" => Self::Idle,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The state as the daemon spells it.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Draft => "draft",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::AwaitingInput => "awaiting_input",
            Self::AwaitingQuota => "awaiting_quota",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
            Self::Answering => "answering",
            Self::Idle => "idle",
            Self::Other(other) => other,
        }
    }

    /// The one-column marker this state is recognised by.
    ///
    /// Both sets are one column wide, so the columns right of the glyph do not
    /// move with the terminal's locale.
    #[must_use]
    pub const fn glyph(&self, level: UnicodeLevel) -> &'static str {
        match level {
            UnicodeLevel::Extended => match self {
                Self::Draft => "◇",
                Self::Queued => "○",
                Self::Running => "●",
                Self::AwaitingInput => "◐",
                Self::AwaitingQuota => "⏱",
                Self::Done => "✓",
                Self::Failed => "✗",
                Self::Interrupted => "▪",
                Self::Cancelled => "⊘",
                Self::Answering => "◆",
                Self::Idle => "·",
                Self::Other(_) => "▫",
            },
            UnicodeLevel::Ascii => match self {
                Self::Draft => ".",
                Self::Queued => "-",
                Self::Running => "o",
                Self::AwaitingInput => "?",
                Self::AwaitingQuota => "%",
                Self::Done => "+",
                Self::Failed => "x",
                Self::Interrupted => "!",
                Self::Cancelled => "/",
                Self::Answering => ">",
                Self::Idle => "_",
                Self::Other(_) => "*",
            },
        }
    }

    /// Whether the node is holding nothing and waiting to be released.
    ///
    /// A parked node is neither working nor finished, which is the distinction
    /// the board exists to make: it needs an answer or a clock, not a reader
    /// wondering whether it died.
    #[must_use]
    pub const fn is_parked(&self) -> bool {
        matches!(self, Self::AwaitingInput | Self::AwaitingQuota)
    }

    /// Whether the node is doing work right now.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::Answering)
    }

    /// Whether the node has stopped for good.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Done | Self::Failed | Self::Interrupted | Self::Cancelled
        )
    }
}

/// What kind of thing a node is. The reader's own chat is a node like any
/// other, which is the point: the board shows the whole fleet, including the
/// terminal looking at it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeamNodeKind {
    Run,
    Chat,
}

impl TeamNodeKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Chat => "chat",
        }
    }
}

/// One row of the tree: a run, or a chat somebody has open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamNode {
    pub id: i64,
    pub kind: TeamNodeKind,
    /// The task a run was opened for, or the checkout a chat is attached to.
    pub title: String,
    pub state: TeamState,
    /// The run this one was opened from, when the daemon reports lineage.
    pub parent: Option<i64>,
    /// Which try this is, counting from one.
    pub attempt: Option<i64>,
    /// The provider or model carrying the node's work.
    pub model: Option<String>,
    /// Spend in millionths of a currency unit, never a float.
    pub cost_micros: Option<i64>,
    /// How long the node has been alive.
    pub duration: Option<Duration>,
    /// What a parked node is waiting for, in the reader's words.
    pub waiting: Option<String>,
    /// How long it has been parked, for a node that is not moving.
    pub parked_for: Option<Duration>,
    /// Whether this node is the terminal's own session.
    pub is_self: bool,
}

impl TeamNode {
    /// A run node with only the fields the tree cannot do without.
    #[must_use]
    pub fn run(id: i64, title: impl Into<String>, state: TeamState) -> Self {
        Self {
            id,
            kind: TeamNodeKind::Run,
            title: title.into(),
            state,
            parent: None,
            attempt: None,
            model: None,
            cost_micros: None,
            duration: None,
            waiting: None,
            parked_for: None,
            is_self: false,
        }
    }

    /// A chat node, which has no attempts and no lineage.
    #[must_use]
    pub fn chat(id: i64, title: impl Into<String>, state: TeamState) -> Self {
        Self {
            kind: TeamNodeKind::Chat,
            ..Self::run(id, title, state)
        }
    }

    /// The facts that fit beside the title: attempt, model and cost, in that
    /// order, with whatever is unknown left out rather than shown as a dash.
    #[must_use]
    pub fn metrics_line(&self) -> String {
        let mut parts = Vec::new();

        if let Some(attempt) = self.attempt {
            parts.push(format!("attempt {attempt}"));
        }
        if let Some(model) = &self.model {
            parts.push(model.clone());
        }
        if let Some(cost) = self.cost_micros {
            parts.push(format_cost(cost));
        }
        if let Some(duration) = self.duration {
            parts.push(format_duration(duration));
        }

        parts.join(" · ")
    }

    /// What a parked node is held by, and for how long.
    ///
    /// Absent for a node that is not parked: a running node has nothing to
    /// wait for, and saying so would make the two look alike.
    #[must_use]
    pub fn parked_line(&self) -> Option<String> {
        if !self.state.is_parked() {
            return None;
        }

        let reason = self
            .waiting
            .clone()
            .unwrap_or_else(|| default_parked_reason(&self.state).to_owned());

        Some(match self.parked_for {
            Some(parked) => format!("{reason} · parked {}", format_duration(parked)),
            None => reason,
        })
    }
}

/// What holds a parked node when the daemon named no question.
const fn default_parked_reason(state: &TeamState) -> &'static str {
    match state {
        TeamState::AwaitingQuota => "quota",
        _ => "an answer",
    }
}

/// One repository's runs, as the daemon holds them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamRepo {
    pub id: String,
    pub nodes: Vec<TeamNode>,
}

/// The whole fleet at one instant.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TeamSnapshot {
    pub repos: Vec<TeamRepo>,
    /// Every question open across the whole fleet, newest last.
    pub inbox: Vec<TeamInboxItem>,
}

impl TeamSnapshot {
    #[must_use]
    pub fn node(&self, id: i64) -> Option<&TeamNode> {
        self.repos
            .iter()
            .flat_map(|repo| &repo.nodes)
            .find(|node| node.id == id)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.repos.iter().all(|repo| repo.nodes.is_empty())
    }

    /// The open question addressed to one node, when there is one.
    #[must_use]
    pub fn question_for(&self, run_id: i64) -> Option<&TeamInboxItem> {
        self.inbox.iter().find(|item| item.run_id == run_id)
    }
}

/// One try at a run, with its own outcome and cost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamAttempt {
    pub n: i64,
    /// Absent while the attempt is still running.
    pub outcome: Option<String>,
    pub tokens: Option<i64>,
    pub cost_micros: Option<i64>,
    pub duration: Option<Duration>,
}

/// A question a run is stopped on, as the inbox and the detail both show it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamQuestion {
    pub question_id: i64,
    pub run_id: i64,
    /// "question" or "approval", as the daemon classes it.
    pub kind: String,
    pub blocked_decision: String,
    /// The answers the question admits, already parsed out of its JSON array.
    pub options: Vec<String>,
    pub recommendation: Option<String>,
}

impl TeamQuestion {
    /// What the question is asking for, in the words the fleet console uses.
    #[must_use]
    pub fn waiting_label(&self) -> &'static str {
        waiting_label_for_kind(&self.kind)
    }
}

/// What a question of this class is asking for.
///
/// The daemon classes a question as `approval` or as anything else, and the
/// difference matters to the reader: one authorizes a merge, the other answers
/// a question. Shared so the terminal and the fleet console name it identically.
#[must_use]
pub fn waiting_label_for_kind(kind: &str) -> &'static str {
    if kind == "approval" {
        "merge authorization"
    } else {
        "question"
    }
}

/// One question the fleet is stopped on, addressed to whoever is watching.
///
/// Questions are born at runtime: nothing declares them in advance, so the
/// inbox is whatever the daemon reports open right now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamInboxItem {
    pub repo_id: String,
    pub run_id: i64,
    pub question_id: i64,
    /// "question" or "approval", as the daemon classes it.
    pub kind: String,
    pub blocked_decision: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
    /// How long it has been waiting, when the reading carries a clock.
    pub age: Option<Duration>,
}

impl TeamInboxItem {
    #[must_use]
    pub fn waiting_label(&self) -> &'static str {
        waiting_label_for_kind(&self.kind)
    }

    /// Whether answering this authorizes a merge rather than answering a
    /// question, which is a different call on the daemon.
    #[must_use]
    pub fn is_approval(&self) -> bool {
        self.kind == "approval"
    }
}

/// Which half of the journal an event came from.
///
/// The two are shown apart because they answer different questions: what the
/// agent did, and what the machine did to it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeamEventClass {
    Agent,
    Infra,
}

impl TeamEventClass {
    #[must_use]
    pub fn parse(class: &str) -> Self {
        if class == "infra" {
            Self::Infra
        } else {
            Self::Agent
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Infra => "infra",
        }
    }
}

/// One line of a run's journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TeamEvent {
    pub class: TeamEventClass,
    pub kind: String,
    pub payload: String,
    pub ts: i64,
}

/// Everything the detail view knows about one node, as the daemon reports it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TeamNodeDetail {
    pub node_id: i64,
    pub task: String,
    pub scope: String,
    pub definition_of_done: String,
    pub worktree: Option<String>,
    pub attempts: Vec<TeamAttempt>,
    pub questions: Vec<TeamQuestion>,
    pub events: Vec<TeamEvent>,
}

impl TeamNodeDetail {
    /// The journal restricted to one class, newest last.
    #[must_use]
    pub fn events_of(&self, class: TeamEventClass) -> Vec<&TeamEvent> {
        self.events
            .iter()
            .filter(|event| event.class == class)
            .collect()
    }
}

/// Spend as money, from the millionths the daemon counts in.
///
/// A non-zero spend never rounds to nothing: a run that cost a fraction of a
/// cent reads as under a cent rather than as free.
#[must_use]
pub fn format_cost(micros: i64) -> String {
    if micros == 0 {
        return "$0.00".to_owned();
    }
    if micros.abs() < 10_000 {
        return "<$0.01".to_owned();
    }

    let cents = micros / 10_000;
    format!("${}.{:02}", cents / 100, (cents % 100).abs())
}

/// A span as the shortest thing that still says how long it was.
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();

    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3_600 {
        return format!("{}m {:02}s", seconds / 60, seconds % 60);
    }

    format!("{}h {:02}m", seconds / 3_600, (seconds % 3_600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parked_state_is_neither_running_nor_finished() {
        assert!(TeamState::AwaitingInput.is_parked());
        assert!(TeamState::AwaitingQuota.is_parked());
        assert!(!TeamState::AwaitingInput.is_active());
        assert!(!TeamState::AwaitingInput.is_terminal());
        assert!(!TeamState::Running.is_parked());
        assert!(!TeamState::Failed.is_parked());
    }

    #[test]
    fn every_state_glyph_is_one_column_wide_in_both_sets() {
        let states = [
            TeamState::Draft,
            TeamState::Queued,
            TeamState::Running,
            TeamState::AwaitingInput,
            TeamState::AwaitingQuota,
            TeamState::Done,
            TeamState::Failed,
            TeamState::Interrupted,
            TeamState::Cancelled,
            TeamState::Answering,
            TeamState::Idle,
            TeamState::Other("weird".to_owned()),
        ];

        for state in states {
            for level in [UnicodeLevel::Extended, UnicodeLevel::Ascii] {
                let glyph = state.glyph(level);
                assert_eq!(
                    unicode_width::UnicodeWidthStr::width(glyph),
                    1,
                    "{} at {level:?} is not one column",
                    state.label()
                );
            }
        }
    }

    #[test]
    fn the_parked_states_carry_the_glyphs_the_board_is_read_by() {
        assert_eq!(TeamState::AwaitingInput.glyph(UnicodeLevel::Extended), "◐");
        assert_eq!(TeamState::AwaitingQuota.glyph(UnicodeLevel::Extended), "⏱");
        assert_eq!(TeamState::Draft.glyph(UnicodeLevel::Extended), "◇");
    }

    #[test]
    fn an_unknown_state_is_kept_verbatim_instead_of_guessed_at() {
        let state = TeamState::parse("reconciling");

        assert_eq!(state, TeamState::Other("reconciling".to_owned()));
        assert_eq!(state.label(), "reconciling");
        assert!(!state.is_parked());
        assert!(!state.is_terminal());
    }

    #[test]
    fn a_node_shows_only_the_metrics_it_actually_has() {
        let bare = TeamNode::run(7, "ship the api", TeamState::Running);

        assert_eq!(bare.metrics_line(), "");

        let measured = TeamNode {
            attempt: Some(2),
            model: Some("gpt-5.5".to_owned()),
            cost_micros: Some(420_000),
            duration: Some(Duration::from_secs(95)),
            ..bare
        };

        assert_eq!(
            measured.metrics_line(),
            "attempt 2 · gpt-5.5 · $0.42 · 1m 35s"
        );
    }

    #[test]
    fn only_a_parked_node_says_what_it_is_waiting_for() {
        let running = TeamNode::run(7, "ship the api", TeamState::Running);

        assert_eq!(running.parked_line(), None);

        let parked = TeamNode {
            state: TeamState::AwaitingInput,
            waiting: Some("merge authorization".to_owned()),
            parked_for: Some(Duration::from_secs(240)),
            ..running.clone()
        };

        assert_eq!(
            parked.parked_line().as_deref(),
            Some("merge authorization · parked 4m 00s")
        );

        let capped = TeamNode {
            state: TeamState::AwaitingQuota,
            ..running
        };

        assert_eq!(capped.parked_line().as_deref(), Some("quota"));
    }

    #[test]
    fn the_journal_splits_what_the_agent_did_from_what_the_machine_did() {
        let detail = TeamNodeDetail {
            events: vec![
                TeamEvent {
                    class: TeamEventClass::parse("agent"),
                    kind: "tool_call".to_owned(),
                    payload: "{}".to_owned(),
                    ts: 1,
                },
                TeamEvent {
                    class: TeamEventClass::parse("infra"),
                    kind: "quota_reached".to_owned(),
                    payload: "{}".to_owned(),
                    ts: 2,
                },
                TeamEvent {
                    class: TeamEventClass::parse("unheard-of"),
                    kind: "surprise".to_owned(),
                    payload: "{}".to_owned(),
                    ts: 3,
                },
            ],
            ..TeamNodeDetail::default()
        };

        let agent: Vec<&str> = detail
            .events_of(TeamEventClass::Agent)
            .iter()
            .map(|event| event.kind.as_str())
            .collect();
        let infra: Vec<&str> = detail
            .events_of(TeamEventClass::Infra)
            .iter()
            .map(|event| event.kind.as_str())
            .collect();

        assert_eq!(agent, ["tool_call", "surprise"]);
        assert_eq!(infra, ["quota_reached"]);
    }

    #[test]
    fn an_approval_is_named_a_merge_authorization_and_nothing_else_is() {
        let approval = TeamQuestion {
            question_id: 3,
            run_id: 11,
            kind: "approval".to_owned(),
            blocked_decision: "merge".to_owned(),
            options: vec!["merge".to_owned()],
            recommendation: None,
        };
        let question = TeamQuestion {
            kind: "question".to_owned(),
            ..approval.clone()
        };

        assert_eq!(approval.waiting_label(), "merge authorization");
        assert_eq!(question.waiting_label(), "question");
    }

    #[test]
    fn spend_never_rounds_a_real_cost_down_to_free() {
        assert_eq!(format_cost(0), "$0.00");
        assert_eq!(format_cost(1), "<$0.01");
        assert_eq!(format_cost(9_999), "<$0.01");
        assert_eq!(format_cost(10_000), "$0.01");
        assert_eq!(format_cost(420_000), "$0.42");
        assert_eq!(format_cost(12_300_000), "$12.30");
    }

    #[test]
    fn a_span_reads_as_the_shortest_thing_that_still_says_how_long() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
        assert_eq!(format_duration(Duration::from_secs(95)), "1m 35s");
        assert_eq!(format_duration(Duration::from_secs(3_600)), "1h 00m");
        assert_eq!(format_duration(Duration::from_secs(7_500)), "2h 05m");
    }
}

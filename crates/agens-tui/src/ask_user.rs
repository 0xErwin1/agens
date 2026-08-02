//! Keyboard reduction for a bounded structured `native::ask_user` interaction.
//!
//! This module owns the interaction state and its key reduction only. It has
//! no knowledge of terminal size, layout thresholds, or how a frame is
//! painted — that is [`crate`]'s own `render_ask_user`, added separately.
//! Selections are stored as option indices rather than option IDs so that
//! "unknown option id" stays unrepresentable on this surface: the reply is
//! only ever built by re-projecting those indices into the request's own
//! declared option order.

use std::collections::BTreeSet;

use agens_core::ask_user::{
    AskUserAnswer, AskUserMode, AskUserReply, AskUserRequest, MAX_ASK_USER_FREE_TEXT_CHARS,
    MAX_ASK_USER_NOTE_CHARS,
};

use crate::Key;

/// How far one `PageUp`/`PageDown` press moves the context pane.
///
/// The pane's real extent is only known once a frame is laid out, which this
/// module never sees; the caller passes that extent in as the scroll ceiling,
/// so the step stays a plain constant while the bound stays truthful.
const ASK_USER_CONTEXT_PAGE_STEP: u16 = 10;

/// The row the interaction cursor is standing on within the current question.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AskUserRow {
    Option(usize),
    Submit,
    Discuss,
    Cancel,
}

/// Which free-form buffer, if any, is currently receiving typed input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AskUserEntry {
    Browsing,
    Other,
    Note,
}

/// The visible effect of reducing one key against an [`AskUserState`].
pub(crate) enum AskUserOutcome {
    /// Nothing a reader could see changed.
    Unchanged,
    /// State changed but the interaction is still open.
    Changed,
    /// The interaction resolved; the caller now owns the terminal reply.
    Resolved(AskUserReply),
}

/// A read-only projection of an open ask-user interaction, for callers that
/// only need to observe state (tests, and eventually a renderer) without
/// reaching into [`AskUserState`] itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AskUserSnapshot {
    pub question_index: usize,
    pub question_count: usize,
    pub row: AskUserRowSnapshot,
    pub selected: Vec<usize>,
    pub other: String,
    pub note: String,
    pub editing: AskUserEditing,
    pub incomplete: Option<usize>,
    pub discuss_available: bool,
    pub context_scroll: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AskUserRowSnapshot {
    Option(usize),
    Submit,
    Discuss,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AskUserEditing {
    Browsing,
    Other,
    Note,
}

impl From<AskUserRow> for AskUserRowSnapshot {
    fn from(row: AskUserRow) -> Self {
        match row {
            AskUserRow::Option(index) => Self::Option(index),
            AskUserRow::Submit => Self::Submit,
            AskUserRow::Discuss => Self::Discuss,
            AskUserRow::Cancel => Self::Cancel,
        }
    }
}

impl From<AskUserEntry> for AskUserEditing {
    fn from(entry: AskUserEntry) -> Self {
        match entry {
            AskUserEntry::Browsing => Self::Browsing,
            AskUserEntry::Other => Self::Other,
            AskUserEntry::Note => Self::Note,
        }
    }
}

/// Dedicated interaction state for one open `native::ask_user` request,
/// sibling to `SecretEntryState` and `DeviceAuthState`.
#[derive(Clone, Debug)]
pub(crate) struct AskUserState {
    id: u64,
    request: AskUserRequest,
    question: usize,
    row: AskUserRow,
    selections: Vec<BTreeSet<usize>>,
    other: Vec<String>,
    notes: Vec<String>,
    entry: AskUserEntry,
    /// The option whose context the pane is showing.
    ///
    /// Tracked separately from [`Self::row`] so walking down to the action rows
    /// leaves the pane — and its scroll offset — exactly where the reader left
    /// it, instead of blanking the explanation they are still consulting.
    context_option: usize,
    context_scroll: u16,
    incomplete: Option<usize>,
}

impl AskUserState {
    pub(crate) fn new(id: u64, request: AskUserRequest) -> Self {
        let question_count = request.questions().len();
        Self {
            id,
            selections: vec![BTreeSet::new(); question_count],
            other: vec![String::new(); question_count],
            notes: vec![String::new(); question_count],
            request,
            question: 0,
            row: AskUserRow::Option(0),
            entry: AskUserEntry::Browsing,
            context_option: 0,
            context_scroll: 0,
            incomplete: None,
        }
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) const fn request(&self) -> &AskUserRequest {
        &self.request
    }

    pub(crate) const fn question_index(&self) -> usize {
        self.question
    }

    pub(crate) const fn row(&self) -> AskUserRow {
        self.row
    }

    pub(crate) fn current_selections(&self) -> &BTreeSet<usize> {
        &self.selections[self.question]
    }

    pub(crate) fn current_other(&self) -> &str {
        &self.other[self.question]
    }

    pub(crate) fn current_note(&self) -> &str {
        &self.notes[self.question]
    }

    pub(crate) const fn entry(&self) -> AskUserEntry {
        self.entry
    }

    pub(crate) const fn context_option(&self) -> usize {
        self.context_option
    }

    pub(crate) const fn context_scroll(&self) -> u16 {
        self.context_scroll
    }

    pub(crate) const fn incomplete(&self) -> Option<usize> {
        self.incomplete
    }

    /// How many questions currently hold a valid answer.
    pub(crate) fn answered_count(&self) -> usize {
        (0..self.request.questions().len())
            .filter(|index| self.question_is_answered(*index))
            .count()
    }

    /// Pulls a stored scroll offset back inside a freshly measured pane.
    ///
    /// Returns whether anything moved, so a caller that only resized can tell a
    /// real change from a no-op.
    pub(crate) fn clamp_context_scroll(&mut self, max: u16) -> bool {
        if self.context_scroll <= max {
            return false;
        }
        self.context_scroll = max;
        true
    }

    pub(crate) fn snapshot(&self) -> AskUserSnapshot {
        AskUserSnapshot {
            question_index: self.question,
            question_count: self.request.questions().len(),
            row: self.row.into(),
            selected: self.selections[self.question].iter().copied().collect(),
            other: self.other[self.question].clone(),
            note: self.notes[self.question].clone(),
            editing: self.entry.into(),
            incomplete: self.incomplete,
            discuss_available: self.current_allows_discuss(),
            context_scroll: self.context_scroll,
        }
    }

    /// Reduces one key against the interaction.
    ///
    /// `max_context_scroll` is the scroll ceiling the caller measured against
    /// the frame it last laid out. It is a parameter rather than state because
    /// the extent depends on the terminal's width and height, which this module
    /// deliberately knows nothing about.
    pub(crate) fn reduce(&mut self, key: Key, max_context_scroll: u16) -> AskUserOutcome {
        if self.entry != AskUserEntry::Browsing {
            return self.reduce_entry_mode(key, max_context_scroll);
        }
        self.reduce_browse_mode(key, max_context_scroll)
    }

    fn reduce_entry_mode(&mut self, key: Key, max_context_scroll: u16) -> AskUserOutcome {
        match key {
            Key::Char(character) => self.type_char(character),
            Key::Backspace | Key::Delete => self.backspace(),
            Key::DeleteToLineStart => self.clear_buffer(),
            Key::Enter => self.commit_entry(),
            Key::Escape => self.escape(),
            Key::PageUp => self.scroll_context(-1, max_context_scroll),
            Key::PageDown => self.scroll_context(1, max_context_scroll),
            Key::Home => self.scroll_context_to(0),
            Key::End => self.scroll_context_to(max_context_scroll),
            _ => AskUserOutcome::Unchanged,
        }
    }

    fn reduce_browse_mode(&mut self, key: Key, max_context_scroll: u16) -> AskUserOutcome {
        match key {
            Key::Up => self.move_row(-1),
            Key::Down => self.move_row(1),
            Key::Tab => self.move_to_next_question_wrapping(),
            Key::Left => self.move_to_previous_question(),
            Key::Right => self.move_to_next_question(),
            Key::Enter => self.activate_row(),
            Key::Char(' ') => self.activate_option_row(),
            Key::Char('o') => self.open_other(),
            Key::Char('n') => self.open_note(),
            Key::Escape => self.escape(),
            Key::PageUp => self.scroll_context(-1, max_context_scroll),
            Key::PageDown => self.scroll_context(1, max_context_scroll),
            Key::Home => self.scroll_context_to(0),
            Key::End => self.scroll_context_to(max_context_scroll),
            _ => AskUserOutcome::Unchanged,
        }
    }

    fn escape(&mut self) -> AskUserOutcome {
        if self.entry != AskUserEntry::Browsing {
            self.entry = AskUserEntry::Browsing;
            return AskUserOutcome::Changed;
        }
        AskUserOutcome::Resolved(AskUserReply::Cancelled)
    }

    fn current_question_options_len(&self) -> usize {
        self.request.questions()[self.question].options().len()
    }

    fn current_allows_discuss(&self) -> bool {
        self.request.questions()[self.question].allow_discuss()
    }

    fn current_allows_other(&self) -> bool {
        self.request.questions()[self.question].allow_other()
    }

    fn current_allows_note(&self) -> bool {
        self.request.questions()[self.question].allow_note()
    }

    fn row_count(&self) -> usize {
        self.current_question_options_len() + 2 + usize::from(self.current_allows_discuss())
    }

    fn row_index(&self, row: AskUserRow) -> usize {
        let options = self.current_question_options_len();
        match row {
            AskUserRow::Option(index) => index,
            AskUserRow::Submit => options,
            AskUserRow::Discuss => options + 1,
            AskUserRow::Cancel => options + usize::from(self.current_allows_discuss()) + 1,
        }
    }

    fn row_at(&self, index: usize) -> AskUserRow {
        let options = self.current_question_options_len();
        if index < options {
            return AskUserRow::Option(index);
        }
        if index == options {
            return AskUserRow::Submit;
        }
        if self.current_allows_discuss() && index == options + 1 {
            return AskUserRow::Discuss;
        }
        AskUserRow::Cancel
    }

    fn move_row(&mut self, delta: i32) -> AskUserOutcome {
        let current = self.row_index(self.row);
        let last = self.row_count() - 1;
        let next = if delta < 0 {
            current.saturating_sub(1)
        } else {
            (current + 1).min(last)
        };
        if next == current {
            return AskUserOutcome::Unchanged;
        }
        self.row = self.row_at(next);
        if let AskUserRow::Option(index) = self.row
            && index != self.context_option
        {
            self.context_option = index;
            self.context_scroll = 0;
        }
        AskUserOutcome::Changed
    }

    fn move_to_question(&mut self, target: usize) -> AskUserOutcome {
        if target == self.question {
            return AskUserOutcome::Unchanged;
        }
        self.question = target;
        self.row = AskUserRow::Option(0);
        self.context_option = 0;
        self.context_scroll = 0;
        AskUserOutcome::Changed
    }

    fn move_to_next_question_wrapping(&mut self) -> AskUserOutcome {
        let count = self.request.questions().len();
        self.move_to_question((self.question + 1) % count)
    }

    fn move_to_previous_question(&mut self) -> AskUserOutcome {
        self.move_to_question(self.question.saturating_sub(1))
    }

    fn move_to_next_question(&mut self) -> AskUserOutcome {
        let count = self.request.questions().len();
        self.move_to_question((self.question + 1).min(count - 1))
    }

    fn activate_row(&mut self) -> AskUserOutcome {
        match self.row {
            AskUserRow::Option(index) => self.toggle_or_select(index),
            AskUserRow::Submit => self.submit(),
            AskUserRow::Discuss => self.discuss(),
            AskUserRow::Cancel => AskUserOutcome::Resolved(AskUserReply::Cancelled),
        }
    }

    fn activate_option_row(&mut self) -> AskUserOutcome {
        match self.row {
            AskUserRow::Option(index) => self.toggle_or_select(index),
            AskUserRow::Submit | AskUserRow::Discuss | AskUserRow::Cancel => {
                AskUserOutcome::Unchanged
            }
        }
    }

    fn toggle_or_select(&mut self, option_index: usize) -> AskUserOutcome {
        let mode = self.request.questions()[self.question].mode();
        let selected = &mut self.selections[self.question];
        match mode {
            AskUserMode::Single => {
                if selected.len() == 1 && selected.contains(&option_index) {
                    return AskUserOutcome::Unchanged;
                }
                selected.clear();
                selected.insert(option_index);
            }
            AskUserMode::Multiple => {
                if !selected.insert(option_index) {
                    selected.remove(&option_index);
                }
            }
        }
        self.clear_incomplete_if_resolved();
        AskUserOutcome::Changed
    }

    fn open_other(&mut self) -> AskUserOutcome {
        if !self.current_allows_other() {
            return AskUserOutcome::Unchanged;
        }
        self.entry = AskUserEntry::Other;
        AskUserOutcome::Changed
    }

    fn open_note(&mut self) -> AskUserOutcome {
        if !self.current_allows_note() {
            return AskUserOutcome::Unchanged;
        }
        self.entry = AskUserEntry::Note;
        AskUserOutcome::Changed
    }

    fn buffer_mut(&mut self) -> Option<&mut String> {
        let question = self.question;
        match self.entry {
            AskUserEntry::Other => Some(&mut self.other[question]),
            AskUserEntry::Note => Some(&mut self.notes[question]),
            AskUserEntry::Browsing => None,
        }
    }

    fn max_buffer_chars(&self) -> usize {
        match self.entry {
            AskUserEntry::Other => MAX_ASK_USER_FREE_TEXT_CHARS,
            AskUserEntry::Note => MAX_ASK_USER_NOTE_CHARS,
            AskUserEntry::Browsing => 0,
        }
    }

    fn type_char(&mut self, character: char) -> AskUserOutcome {
        if character.is_control() {
            return AskUserOutcome::Unchanged;
        }
        let max_chars = self.max_buffer_chars();
        let Some(buffer) = self.buffer_mut() else {
            return AskUserOutcome::Unchanged;
        };
        if buffer.chars().count() >= max_chars {
            return AskUserOutcome::Unchanged;
        }
        buffer.push(character);
        AskUserOutcome::Changed
    }

    fn backspace(&mut self) -> AskUserOutcome {
        match self.buffer_mut() {
            Some(buffer) => {
                if buffer.pop().is_some() {
                    AskUserOutcome::Changed
                } else {
                    AskUserOutcome::Unchanged
                }
            }
            None => AskUserOutcome::Unchanged,
        }
    }

    fn clear_buffer(&mut self) -> AskUserOutcome {
        match self.buffer_mut() {
            Some(buffer) if !buffer.is_empty() => {
                buffer.clear();
                AskUserOutcome::Changed
            }
            _ => AskUserOutcome::Unchanged,
        }
    }

    fn commit_entry(&mut self) -> AskUserOutcome {
        self.entry = AskUserEntry::Browsing;
        self.clear_incomplete_if_resolved();
        AskUserOutcome::Changed
    }

    /// Clears a stale "answer this question first" flag once the flagged
    /// question itself becomes answered, so the header does not keep
    /// pointing at a question the user has already fixed.
    fn clear_incomplete_if_resolved(&mut self) {
        if let Some(index) = self.incomplete
            && self.question_is_answered(index)
        {
            self.incomplete = None;
        }
    }

    fn scroll_context(&mut self, direction: i32, max: u16) -> AskUserOutcome {
        let next = if direction < 0 {
            self.context_scroll
                .saturating_sub(ASK_USER_CONTEXT_PAGE_STEP)
        } else {
            self.context_scroll
                .saturating_add(ASK_USER_CONTEXT_PAGE_STEP)
        };
        self.scroll_context_to(next.min(max))
    }

    fn scroll_context_to(&mut self, offset: u16) -> AskUserOutcome {
        if offset == self.context_scroll {
            return AskUserOutcome::Unchanged;
        }
        self.context_scroll = offset;
        AskUserOutcome::Changed
    }

    fn question_is_answered(&self, index: usize) -> bool {
        let allow_other = self.request.questions()[index].allow_other();
        !self.selections[index].is_empty() || (allow_other && !self.other[index].trim().is_empty())
    }

    fn first_incomplete(&self) -> Option<usize> {
        (0..self.request.questions().len()).find(|index| !self.question_is_answered(*index))
    }

    fn submit(&mut self) -> AskUserOutcome {
        if let Some(index) = self.first_incomplete() {
            if self.incomplete == Some(index) {
                return AskUserOutcome::Unchanged;
            }
            self.incomplete = Some(index);
            return AskUserOutcome::Changed;
        }
        self.incomplete = None;
        AskUserOutcome::Resolved(AskUserReply::Answered(self.build_answers()))
    }

    fn discuss(&mut self) -> AskUserOutcome {
        if !self.current_allows_discuss() {
            return AskUserOutcome::Unchanged;
        }
        let question_id = self.request.questions()[self.question].id().to_owned();
        let note = non_blank(&self.notes[self.question]);
        AskUserOutcome::Resolved(AskUserReply::Discuss { question_id, note })
    }

    fn build_answers(&self) -> Vec<AskUserAnswer> {
        self.request
            .questions()
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let selected = question
                    .options()
                    .iter()
                    .enumerate()
                    .filter(|(option_index, _)| self.selections[index].contains(option_index))
                    .map(|(_, option)| option.id().to_owned())
                    .collect();
                AskUserAnswer {
                    question_id: question.id().to_owned(),
                    selected,
                    other: non_blank(&self.other[index]),
                    note: non_blank(&self.notes[index]),
                }
            })
            .collect()
    }
}

fn non_blank(value: &str) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

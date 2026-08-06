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
    /// The row that carries the interaction forward: the next question while
    /// any remain, review on the last question, and final submit while
    /// reviewing. It is a single row rather than two because its position
    /// never moves — the reader's hand learns one place to press Enter —
    /// while what pressing it means is exactly the difference between "there
    /// is more to answer", "check everything once", and "send".
    Proceed,
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
    pub entry_cursor: usize,
    pub discuss_available: bool,
    pub context_scroll: u16,
    /// Whether the reader is on the pre-submit review of every answer.
    pub reviewing: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AskUserRowSnapshot {
    Option(usize),
    Proceed,
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
            AskUserRow::Proceed => Self::Proceed,
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
    /// The delegated execution that raised this question, if it was not the
    /// main thread.
    origin: Option<crate::PromptOrigin>,
    question: usize,
    row: AskUserRow,
    selections: Vec<BTreeSet<usize>>,
    other: Vec<String>,
    notes: Vec<String>,
    entry: AskUserEntry,
    /// Where the caret sits in the buffer [`Self::entry`] names, as a `char`
    /// index. Only meaningful while a buffer is open; opening one places it at
    /// the end of whatever is already typed.
    entry_cursor: usize,
    /// The option whose context the pane is showing.
    ///
    /// Tracked separately from [`Self::row`] so walking down to the action rows
    /// leaves the pane — and its scroll offset — exactly where the reader left
    /// it, instead of blanking the explanation they are still consulting.
    context_option: usize,
    context_scroll: u16,
    /// Pre-submit screen that lists every question with its answer before the
    /// final commit. While true the list shows only review content plus
    /// Submit/Cancel — no option rows, free-text entry, or discuss.
    reviewing: bool,
}

impl AskUserState {
    pub(crate) fn new(
        id: u64,
        request: AskUserRequest,
        origin: Option<crate::PromptOrigin>,
    ) -> Self {
        let question_count = request.questions().len();
        Self {
            id,
            selections: vec![BTreeSet::new(); question_count],
            other: vec![String::new(); question_count],
            notes: vec![String::new(); question_count],
            request,
            origin,
            question: 0,
            row: AskUserRow::Option(0),
            entry: AskUserEntry::Browsing,
            entry_cursor: 0,
            context_option: 0,
            context_scroll: 0,
            reviewing: false,
        }
    }

    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) const fn request(&self) -> &AskUserRequest {
        &self.request
    }

    pub(crate) const fn origin(&self) -> Option<&crate::PromptOrigin> {
        self.origin.as_ref()
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

    pub(crate) const fn entry_cursor(&self) -> usize {
        self.entry_cursor
    }

    pub(crate) const fn context_option(&self) -> usize {
        self.context_option
    }

    pub(crate) const fn context_scroll(&self) -> u16 {
        self.context_scroll
    }

    pub(crate) const fn reviewing(&self) -> bool {
        self.reviewing
    }

    pub(crate) fn selections(&self) -> &[BTreeSet<usize>] {
        &self.selections
    }

    pub(crate) fn others(&self) -> &[String] {
        &self.other
    }

    pub(crate) fn notes(&self) -> &[String] {
        &self.notes
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
            entry_cursor: self.entry_cursor,
            discuss_available: self.current_allows_discuss() && !self.reviewing,
            context_scroll: self.context_scroll,
            reviewing: self.reviewing,
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

    /// Reduces one key while a free-form buffer is open.
    ///
    /// The editing keys are the composer's, resolved through the same
    /// [`Key::composer_equivalent`] mapping, because a text field that silently
    /// drops the motions a reader's hands already know is a text field they
    /// cannot see what they are typing in. `Home`/`End` therefore move the
    /// caret here rather than paging the context beside it — the pane keeps
    /// `PageUp`/`PageDown`, which no text field claims.
    fn reduce_entry_mode(&mut self, key: Key, max_context_scroll: u16) -> AskUserOutcome {
        let cursor = self.entry_cursor;
        match key.composer_equivalent() {
            Key::Char(character) => self.type_char(character),
            Key::Backspace => self.delete_range(cursor.saturating_sub(1), cursor),
            Key::Delete => self.delete_range(cursor, cursor.saturating_add(1)),
            Key::DeletePreviousWord => self.delete_range(self.previous_word(), cursor),
            Key::DeleteNextWord => self.delete_range(cursor, self.next_word()),
            Key::DeleteToLineStart => self.delete_range(0, cursor),
            Key::DeleteToLineEnd => self.delete_range(cursor, self.buffer_len()),
            Key::Left => self.move_entry_cursor(cursor.saturating_sub(1)),
            Key::Right => self.move_entry_cursor(cursor.saturating_add(1)),
            Key::PreviousWord => self.move_entry_cursor(self.previous_word()),
            Key::NextWord => self.move_entry_cursor(self.next_word()),
            Key::Home | Key::LineStart => self.move_entry_cursor(0),
            Key::End | Key::LineEnd => self.move_entry_cursor(self.buffer_len()),
            Key::Enter => self.commit_entry(),
            Key::Escape => self.escape(),
            Key::PageUp => self.scroll_context(-1, max_context_scroll),
            Key::PageDown => self.scroll_context(1, max_context_scroll),
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
            self.close_entry();
            return AskUserOutcome::Changed;
        }
        AskUserOutcome::Resolved(AskUserReply::Cancelled)
    }

    fn close_entry(&mut self) {
        self.entry = AskUserEntry::Browsing;
        self.entry_cursor = 0;
    }

    fn current_question_options_len(&self) -> usize {
        self.request.questions()[self.question].options().len()
    }

    fn current_allows_discuss(&self) -> bool {
        !self.reviewing && self.request.questions()[self.question].allow_discuss()
    }

    /// Free-text "other" is always available on this surface, independent of
    /// the agent's `allow_other` flag — except while reviewing, where only
    /// Submit and Cancel are live.
    fn current_allows_other(&self) -> bool {
        !self.reviewing
    }

    fn current_allows_note(&self) -> bool {
        !self.reviewing && self.request.questions()[self.question].allow_note()
    }

    fn row_count(&self) -> usize {
        if self.reviewing {
            // Proceed + Cancel only.
            return 2;
        }
        self.current_question_options_len() + 2 + usize::from(self.current_allows_discuss())
    }

    fn row_index(&self, row: AskUserRow) -> usize {
        if self.reviewing {
            return match row {
                AskUserRow::Proceed => 0,
                AskUserRow::Cancel => 1,
                AskUserRow::Option(_) | AskUserRow::Discuss => 0,
            };
        }
        let options = self.current_question_options_len();
        match row {
            AskUserRow::Option(index) => index,
            AskUserRow::Proceed => options,
            AskUserRow::Discuss => options + 1,
            AskUserRow::Cancel => options + usize::from(self.current_allows_discuss()) + 1,
        }
    }

    fn row_at(&self, index: usize) -> AskUserRow {
        if self.reviewing {
            return if index == 0 {
                AskUserRow::Proceed
            } else {
                AskUserRow::Cancel
            };
        }
        let options = self.current_question_options_len();
        if index < options {
            return AskUserRow::Option(index);
        }
        if index == options {
            return AskUserRow::Proceed;
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
        // Leaving review counts as a real navigation even when the question
        // index does not change (single-question Tab/Left, or re-focus).
        if target == self.question && !self.reviewing {
            return AskUserOutcome::Unchanged;
        }
        self.focus_question(target)
    }

    /// Puts the cursor on the first option of `target`, whether or not that is
    /// the question already on screen.
    ///
    /// Distinct from [`Self::move_to_question`], which is navigation and so
    /// treats "already there" as nothing to do. This is used where the point
    /// is to put the reader in front of something specific, and landing on the
    /// right question with the cursor still parked on a button would not be
    /// that. Always leaves review mode.
    fn focus_question(&mut self, target: usize) -> AskUserOutcome {
        let settled = !self.reviewing
            && self.question == target
            && self.row == AskUserRow::Option(0)
            && self.context_option == 0
            && self.context_scroll == 0;

        self.reviewing = false;
        self.question = target;
        self.row = AskUserRow::Option(0);
        self.context_option = 0;
        self.context_scroll = 0;

        if settled {
            AskUserOutcome::Unchanged
        } else {
            AskUserOutcome::Changed
        }
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
        if self.reviewing {
            return match self.row {
                AskUserRow::Proceed => self.submit(),
                AskUserRow::Cancel => AskUserOutcome::Resolved(AskUserReply::Cancelled),
                AskUserRow::Option(_) | AskUserRow::Discuss => AskUserOutcome::Unchanged,
            };
        }
        match self.row {
            AskUserRow::Option(index) => {
                let mode = self.request.questions()[self.question].mode();
                if self.is_last_question()
                    && mode == AskUserMode::Single
                    && self.selections[self.question].contains(&index)
                {
                    // Single mode, last question, cursor on the chosen option:
                    // Enter here cannot change the answer (it is a no-op as a
                    // selection), so it reads as "move on" and opens review.
                    // Moving to another option and pressing Enter still corrects
                    // the selection; multiple mode still accumulates in place.
                    return self.enter_review();
                }
                let selected = self.toggle_or_select(index);
                if !self.is_last_question() {
                    return self.move_to_next_question();
                }
                selected
            }
            AskUserRow::Proceed => self.proceed(),
            AskUserRow::Discuss => self.discuss(),
            AskUserRow::Cancel => AskUserOutcome::Resolved(AskUserReply::Cancelled),
        }
    }

    /// Space on an option toggles selection without advancing, so multi-select
    /// can accumulate choices before the reader moves on with Enter or Proceed.
    fn activate_option_row(&mut self) -> AskUserOutcome {
        if self.reviewing {
            return AskUserOutcome::Unchanged;
        }
        match self.row {
            AskUserRow::Option(index) => self.toggle_or_select(index),
            AskUserRow::Proceed | AskUserRow::Discuss | AskUserRow::Cancel => {
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
        AskUserOutcome::Changed
    }

    fn open_other(&mut self) -> AskUserOutcome {
        if !self.current_allows_other() {
            return AskUserOutcome::Unchanged;
        }
        self.open_entry(AskUserEntry::Other)
    }

    fn open_note(&mut self) -> AskUserOutcome {
        if !self.current_allows_note() {
            return AskUserOutcome::Unchanged;
        }
        self.open_entry(AskUserEntry::Note)
    }

    /// Opens a buffer with the caret after the text already in it, which is
    /// where someone returning to a half-written note expects to continue.
    fn open_entry(&mut self, entry: AskUserEntry) -> AskUserOutcome {
        self.entry = entry;
        self.entry_cursor = self.buffer_len();
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

    fn buffer(&self) -> &str {
        let question = self.question;
        match self.entry {
            AskUserEntry::Other => &self.other[question],
            AskUserEntry::Note => &self.notes[question],
            AskUserEntry::Browsing => "",
        }
    }

    fn buffer_len(&self) -> usize {
        self.buffer().chars().count()
    }

    fn previous_word(&self) -> usize {
        crate::previous_word_boundary(self.buffer(), self.entry_cursor)
    }

    fn next_word(&self) -> usize {
        crate::next_word_boundary(self.buffer(), self.entry_cursor)
    }

    fn move_entry_cursor(&mut self, target: usize) -> AskUserOutcome {
        let target = target.min(self.buffer_len());
        if target == self.entry_cursor {
            return AskUserOutcome::Unchanged;
        }
        self.entry_cursor = target;
        AskUserOutcome::Changed
    }

    fn type_char(&mut self, character: char) -> AskUserOutcome {
        if character.is_control() {
            return AskUserOutcome::Unchanged;
        }
        if self.buffer_len() >= self.max_buffer_chars() {
            return AskUserOutcome::Unchanged;
        }

        let cursor = self.entry_cursor;
        let Some(buffer) = self.buffer_mut() else {
            return AskUserOutcome::Unchanged;
        };
        let at = crate::byte_index(buffer, cursor);
        buffer.insert(at, character);

        self.entry_cursor = cursor + 1;
        AskUserOutcome::Changed
    }

    /// Removes the `char` range `start..end` from the open buffer and leaves
    /// the caret where the removed text began.
    fn delete_range(&mut self, start: usize, end: usize) -> AskUserOutcome {
        let length = self.buffer_len();
        let start = start.min(length);
        let end = end.min(length);
        if start >= end {
            return AskUserOutcome::Unchanged;
        }

        let Some(buffer) = self.buffer_mut() else {
            return AskUserOutcome::Unchanged;
        };
        let range = crate::byte_index(buffer, start)..crate::byte_index(buffer, end);
        buffer.replace_range(range, "");

        self.entry_cursor = start;
        AskUserOutcome::Changed
    }

    fn commit_entry(&mut self) -> AskUserOutcome {
        self.close_entry();
        AskUserOutcome::Changed
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
        !self.selections[index].is_empty() || !self.other[index].trim().is_empty()
    }

    pub(crate) fn is_last_question(&self) -> bool {
        self.question + 1 == self.request.questions().len()
    }

    /// Carries the interaction forward from the proceed row.
    ///
    /// On any question but the last this only advances; on the last it opens
    /// the review screen; while reviewing it submits. Unanswered questions
    /// are left as skips and accepted at submit time.
    fn proceed(&mut self) -> AskUserOutcome {
        if self.reviewing {
            return self.submit();
        }
        if self.is_last_question() {
            return self.enter_review();
        }
        self.move_to_next_question()
    }

    /// Opens the pre-submit review of every answer.
    fn enter_review(&mut self) -> AskUserOutcome {
        self.close_entry();
        self.reviewing = true;
        self.row = AskUserRow::Proceed;
        AskUserOutcome::Changed
    }

    /// Ends the interaction with whatever answers are present.
    ///
    /// Unanswered questions go out as empty selections so the model can see
    /// which ones the reader skipped.
    fn submit(&mut self) -> AskUserOutcome {
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

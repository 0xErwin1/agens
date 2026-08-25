//! Prompt history and LIFO stash domain state, plus the surface-facing port.
//!
//! History is chronological with consecutive-dedupe and linear browse state.
//! Stash is an independent LIFO. Neither shares state with the FIFO prompt queue.
//! Overlay helpers window for display only; stores are unbounded.
//!
//! This module has no I/O. Durable adapters implement [`PromptMemory`] elsewhere.

use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::MessagePart;

/// One staged media attachment carried by a recorded prompt (durable id, no source path).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptAttachment {
    pub media_id: i64,
    pub mime: String,
}

impl PromptAttachment {
    pub fn new(media_id: i64, mime: impl Into<String>) -> Self {
        Self {
            media_id,
            mime: mime.into(),
        }
    }
}

/// One recorded prompt (history or stash): text plus staged attachments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptMemoryEntry {
    pub text: String,
    pub attachments: Vec<PromptAttachment>,
    /// Canonical ordered Text/Media content. Legacy fields are projections for surfaces.
    pub parts: Vec<MessagePart>,
    /// Unix seconds (UTC).
    pub created_at: i64,
}

impl PromptMemoryEntry {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let parts = (!text.is_empty())
            .then(|| MessagePart::Text(text.clone()))
            .into_iter()
            .collect();
        Self {
            text,
            attachments: Vec::new(),
            parts,
            created_at: unix_now_secs(),
        }
    }

    pub fn with_created_at(text: impl Into<String>, created_at: i64) -> Self {
        let text = text.into();
        let parts = (!text.is_empty())
            .then(|| MessagePart::Text(text.clone()))
            .into_iter()
            .collect();
        Self {
            text,
            attachments: Vec::new(),
            parts,
            created_at,
        }
    }

    pub fn with_attachments(mut self, attachments: Vec<PromptAttachment>) -> Self {
        self.parts
            .extend(attachments.iter().map(|attachment| MessagePart::Media {
                media_id: attachment.media_id,
                mime: attachment.mime.clone(),
            }));
        self.attachments = attachments;
        self
    }

    pub fn with_parts(mut self, parts: Vec<MessagePart>) -> Self {
        self.parts = parts;
        self
    }

    fn recall(&self) -> PromptRecall {
        PromptRecall {
            text: self.text.clone(),
            attachments: self.attachments.clone(),
        }
    }
}

/// Composer content handed back by a restore (stash pop, overlay paste, browse).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptRecall {
    pub text: String,
    pub attachments: Vec<PromptAttachment>,
}

/// Result of moving toward newer history while browsing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryBrowseResult {
    Entry(PromptRecall),
    RestoreDraft(PromptRecall),
    Idle,
}

/// One overlay row with the store index used for stash removal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptOverlayItem {
    pub text: String,
    pub created_at: i64,
    /// Index into the oldest-first store (history index is informational; stash uses it for remove).
    pub store_index: usize,
    pub attachments: Vec<PromptAttachment>,
}

/// Domain error for prompt-memory ports (no I/O types).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptMemoryError {
    message: String,
}

impl PromptMemoryError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PromptMemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PromptMemoryError {}

/// Surface-facing port for global prompt history and stash.
///
/// Owned by logic so any UI can drive the same behavior. Durable adapters may
/// fail mutators with [`PromptMemoryError`]; browse and overlays are pure reads.
pub trait PromptMemory: Send {
    /// Append a submitted prompt unless it is a consecutive duplicate.
    ///
    /// A duplicate means the same text AND the same attachments as the newest
    /// entry: the same text sent with different media is a distinct prompt.
    /// Returns `Ok(true)` when recorded, `Ok(false)` when skipped as a duplicate.
    /// Clears browse either way on success.
    fn record_submission(
        &mut self,
        text: &str,
        attachments: &[PromptAttachment],
    ) -> Result<bool, PromptMemoryError>;

    /// Enter or move browse toward older entries when input is empty (or already browsing).
    ///
    /// `staged_attachments` is captured into the draft on browse entry so that
    /// walking past the newest entry hands staged chips back with the draft text.
    fn browse_up(
        &mut self,
        composer_input: &str,
        staged_attachments: &[PromptAttachment],
    ) -> Option<PromptRecall>;

    /// Move toward newer history, or restore the draft once past the newest entry.
    fn browse_down(&mut self) -> HistoryBrowseResult;

    /// Drop browse state while keeping whatever text the caller holds.
    fn clear_browse(&mut self);

    fn is_browsing(&self) -> bool;

    /// Push onto the LIFO top. Returns `Ok(true)` when pushed.
    fn stash_push(
        &mut self,
        text: &str,
        attachments: &[PromptAttachment],
    ) -> Result<bool, PromptMemoryError>;

    /// Pop the LIFO top, or `Ok(None)` when empty.
    fn stash_pop(&mut self) -> Result<Option<PromptRecall>, PromptMemoryError>;

    /// Remove by oldest-first store index. Returns `Ok(true)` when removed.
    fn stash_remove_at(&mut self, index: usize) -> Result<bool, PromptMemoryError>;

    /// Newest-first filtered window for overlay display.
    fn history_overlay(&self, query: &str, limit: usize) -> Vec<PromptOverlayItem>;

    /// Newest-first filtered window with store indices for remove.
    fn stash_overlay(&self, query: &str, limit: usize) -> Vec<PromptOverlayItem>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BrowseState {
    index: usize,
    draft_text: String,
    draft_attachments: Vec<PromptAttachment>,
}

/// Pure in-memory history + stash + browse reducers.
#[derive(Clone, Debug, Default)]
pub struct PromptMemoryState {
    history: Vec<PromptMemoryEntry>,
    stash: Vec<PromptMemoryEntry>,
    browse: Option<BrowseState>,
}

impl PromptMemoryState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed chronological history (oldest first). Browse starts idle.
    pub fn seed_history(&mut self, entries: impl IntoIterator<Item = PromptMemoryEntry>) {
        self.history = entries.into_iter().collect();
        self.browse = None;
    }

    /// Seed stash oldest-first (last element is LIFO top).
    pub fn seed_stash(&mut self, entries: impl IntoIterator<Item = PromptMemoryEntry>) {
        self.stash = entries.into_iter().collect();
    }

    pub fn history(&self) -> &[PromptMemoryEntry] {
        &self.history
    }

    pub fn stash(&self) -> &[PromptMemoryEntry] {
        &self.stash
    }

    pub fn is_browsing(&self) -> bool {
        self.browse.is_some()
    }

    /// Append a prompt unless it is a consecutive duplicate of the last entry.
    ///
    /// A duplicate requires the same text AND the same attachments: the same
    /// text sent with different media is a distinct prompt and is recorded.
    /// Returns `false` when skipped. Clears browse either way.
    pub fn record_submission(
        &mut self,
        text: impl Into<String>,
        attachments: &[PromptAttachment],
    ) -> bool {
        self.record_submission_at(text, attachments, unix_now_secs())
    }

    pub fn record_submission_at(
        &mut self,
        text: impl Into<String>,
        attachments: &[PromptAttachment],
        created_at: i64,
    ) -> bool {
        let text = text.into();
        self.browse = None;

        if self
            .history
            .last()
            .is_some_and(|entry| entry.text == text && entry.attachments == attachments)
        {
            return false;
        }

        self.history.push(
            PromptMemoryEntry::with_created_at(text, created_at)
                .with_attachments(attachments.to_vec()),
        );
        true
    }

    /// Enter or move browse toward older entries when input is empty (or already browsing).
    ///
    /// On browse entry the draft captures `staged_attachments` alongside the
    /// (empty) input, so browsing down past the newest entry restores both.
    pub fn browse_up(
        &mut self,
        input: &str,
        staged_attachments: &[PromptAttachment],
    ) -> Option<PromptRecall> {
        if self.history.is_empty() {
            return None;
        }

        match &mut self.browse {
            Some(state) => {
                if state.index > 0 {
                    state.index -= 1;
                }
                self.history.get(state.index).map(PromptMemoryEntry::recall)
            }
            None => {
                if !input.is_empty() {
                    return None;
                }

                let index = self.history.len().saturating_sub(1);
                self.browse = Some(BrowseState {
                    index,
                    draft_text: input.to_string(),
                    draft_attachments: staged_attachments.to_vec(),
                });
                self.history.get(index).map(PromptMemoryEntry::recall)
            }
        }
    }

    /// Move toward newer history, or restore the draft once past the newest entry.
    pub fn browse_down(&mut self) -> HistoryBrowseResult {
        let Some(state) = self.browse.as_ref() else {
            return HistoryBrowseResult::Idle;
        };

        if state.index + 1 < self.history.len() {
            let index = state.index + 1;
            if let Some(browse) = self.browse.as_mut() {
                browse.index = index;
            }
            match self.history.get(index) {
                Some(entry) => HistoryBrowseResult::Entry(entry.recall()),
                None => HistoryBrowseResult::Idle,
            }
        } else {
            let draft = PromptRecall {
                text: state.draft_text.clone(),
                attachments: state.draft_attachments.clone(),
            };
            self.browse = None;
            HistoryBrowseResult::RestoreDraft(draft)
        }
    }

    pub fn clear_browse(&mut self) {
        self.browse = None;
    }

    pub fn stash_push(&mut self, text: impl Into<String>, attachments: &[PromptAttachment]) {
        self.stash_push_at(text, attachments, unix_now_secs());
    }

    pub fn stash_push_at(
        &mut self,
        text: impl Into<String>,
        attachments: &[PromptAttachment],
        created_at: i64,
    ) {
        self.stash.push(
            PromptMemoryEntry::with_created_at(text, created_at)
                .with_attachments(attachments.to_vec()),
        );
    }

    pub fn stash_pop(&mut self) -> Option<PromptMemoryEntry> {
        self.stash.pop()
    }

    /// Remove by store index (0 = oldest). Returns the removed entry when in range.
    pub fn stash_remove_at(&mut self, index: usize) -> Option<PromptMemoryEntry> {
        if index < self.stash.len() {
            Some(self.stash.remove(index))
        } else {
            None
        }
    }

    /// Newest-first filtered history window for overlay display.
    pub fn history_overlay(&self, query: &str, limit: usize) -> Vec<PromptOverlayItem> {
        overlay_items(&self.history, query, limit)
    }

    /// Newest-first filtered stash window with original store indices.
    pub fn stash_overlay(&self, query: &str, limit: usize) -> Vec<PromptOverlayItem> {
        overlay_items(&self.stash, query, limit)
    }
}

fn overlay_items(
    entries: &[PromptMemoryEntry],
    query: &str,
    limit: usize,
) -> Vec<PromptOverlayItem> {
    let query_lower = query.to_lowercase();
    entries
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, entry)| entry_matches_query(entry, &query_lower))
        .take(limit)
        .map(|(store_index, entry)| PromptOverlayItem {
            text: entry.text.clone(),
            created_at: entry.created_at,
            store_index,
            attachments: entry.attachments.clone(),
        })
        .collect()
}

/// Matches an entry against an already-lowercased query.
///
/// Attachments are searchable by the chip label and mime they are shown as: an entry with no
/// text is displayed by its attachments alone, so matching text only would put every
/// attachments-only prompt out of reach of any query the reader can type.
fn entry_matches_query(entry: &PromptMemoryEntry, query_lower: &str) -> bool {
    if query_lower.is_empty() || entry.text.to_lowercase().contains(query_lower) {
        return true;
    }

    entry
        .attachments
        .iter()
        .enumerate()
        .any(|(index, attachment)| {
            crate::media_chip_label(index + 1, &attachment.mime)
                .to_lowercase()
                .contains(query_lower)
                || attachment.mime.to_lowercase().contains(query_lower)
        })
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// In-memory [`PromptMemory`] with no disk. For tests and non-SQLite surfaces.
#[derive(Clone, Debug, Default)]
pub struct EphemeralPromptMemory {
    state: PromptMemoryState,
}

impl EphemeralPromptMemory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> &PromptMemoryState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut PromptMemoryState {
        &mut self.state
    }
}

impl PromptMemory for EphemeralPromptMemory {
    fn record_submission(
        &mut self,
        text: &str,
        attachments: &[PromptAttachment],
    ) -> Result<bool, PromptMemoryError> {
        Ok(self.state.record_submission(text, attachments))
    }

    fn browse_up(
        &mut self,
        composer_input: &str,
        staged_attachments: &[PromptAttachment],
    ) -> Option<PromptRecall> {
        self.state.browse_up(composer_input, staged_attachments)
    }

    fn browse_down(&mut self) -> HistoryBrowseResult {
        self.state.browse_down()
    }

    fn clear_browse(&mut self) {
        self.state.clear_browse();
    }

    fn is_browsing(&self) -> bool {
        self.state.is_browsing()
    }

    fn stash_push(
        &mut self,
        text: &str,
        attachments: &[PromptAttachment],
    ) -> Result<bool, PromptMemoryError> {
        self.state.stash_push(text, attachments);
        Ok(true)
    }

    fn stash_pop(&mut self) -> Result<Option<PromptRecall>, PromptMemoryError> {
        Ok(self.state.stash_pop().map(|entry| entry.recall()))
    }

    fn stash_remove_at(&mut self, index: usize) -> Result<bool, PromptMemoryError> {
        Ok(self.state.stash_remove_at(index).is_some())
    }

    fn history_overlay(&self, query: &str, limit: usize) -> Vec<PromptOverlayItem> {
        self.state.history_overlay(query, limit)
    }

    fn stash_overlay(&self, query: &str, limit: usize) -> Vec<PromptOverlayItem> {
        self.state.stash_overlay(query, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(media_id: i64) -> PromptAttachment {
        PromptAttachment::new(media_id, "image/png")
    }

    #[test]
    fn prompt_entry_shape_has_text_attachments_and_unix_created_at() {
        let entry = PromptMemoryEntry::new("hello world");

        assert_eq!(entry.text, "hello world");
        assert!(entry.attachments.is_empty());
        assert!(entry.created_at > 0);

        let with_media = PromptMemoryEntry::new("see this").with_attachments(vec![attachment(7)]);
        assert_eq!(with_media.attachments, vec![attachment(7)]);
    }

    #[test]
    fn record_submission_skips_consecutive_duplicate_text() {
        let mut state = PromptMemoryState::new();

        assert!(state.record_submission("same", &[]));
        assert!(!state.record_submission("same", &[]));
        assert_eq!(state.history().len(), 1);

        assert!(state.record_submission("other", &[]));
        assert!(state.record_submission("same", &[]));
        assert_eq!(
            state
                .history()
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            vec!["same", "other", "same"]
        );
    }

    #[test]
    fn record_submission_same_text_with_different_attachments_is_not_a_duplicate() {
        let mut state = PromptMemoryState::new();

        assert!(state.record_submission("look", &[attachment(1)]));
        assert!(
            state.record_submission("look", &[attachment(2)]),
            "different attachments make a distinct prompt"
        );
        assert!(
            state.record_submission("look", &[]),
            "dropping attachments makes a distinct prompt"
        );
        assert!(
            !state.record_submission("look", &[]),
            "identical text and attachments dedupe"
        );
        assert_eq!(state.history().len(), 3);
    }

    #[test]
    fn record_submission_has_no_hard_product_cap() {
        let mut state = PromptMemoryState::new();

        for index in 0..200 {
            assert!(state.record_submission(format!("entry-{index}"), &[]));
        }

        assert_eq!(state.history().len(), 200);
        assert_eq!(state.history()[0].text, "entry-0");
        assert_eq!(state.history()[199].text, "entry-199");
    }

    #[test]
    fn seed_history_and_stash() {
        let mut state = PromptMemoryState::new();
        state.seed_history([
            PromptMemoryEntry::with_created_at("a", 10),
            PromptMemoryEntry::with_created_at("b", 20).with_attachments(vec![attachment(3)]),
        ]);
        assert_eq!(state.history().len(), 2);
        assert_eq!(state.history()[0].text, "a");
        assert_eq!(state.history()[1].created_at, 20);
        assert_eq!(state.history()[1].attachments, vec![attachment(3)]);

        state.seed_stash([
            PromptMemoryEntry::with_created_at("s1", 1),
            PromptMemoryEntry::with_created_at("s2", 2),
        ]);
        assert_eq!(state.stash().len(), 2);
        assert_eq!(state.stash()[1].text, "s2");
    }

    #[test]
    fn browse_up_from_empty_input_shows_newest_and_preserves_draft() {
        let mut state = PromptMemoryState::new();
        state.record_submission("older", &[]);
        state.record_submission("newer", &[]);

        let shown = state.browse_up("", &[]).expect("enter browse");
        assert_eq!(shown.text, "newer");
        assert!(state.is_browsing());

        let older = state.browse_up("newer", &[]).expect("older");
        assert_eq!(older.text, "older");

        match state.browse_down() {
            HistoryBrowseResult::Entry(recall) => assert_eq!(recall.text, "newer"),
            other => panic!("expected newer entry, got {other:?}"),
        }

        match state.browse_down() {
            HistoryBrowseResult::RestoreDraft(draft) => assert_eq!(draft.text, ""),
            other => panic!("expected draft restore, got {other:?}"),
        }
        assert!(!state.is_browsing());
    }

    #[test]
    fn browse_restores_entry_attachments_and_hands_staged_chips_back_with_the_draft() {
        let mut state = PromptMemoryState::new();
        state.record_submission("with media", &[attachment(9)]);

        let shown = state.browse_up("", &[attachment(4)]).expect("enter browse");
        assert_eq!(shown.text, "with media");
        assert_eq!(shown.attachments, vec![attachment(9)]);

        match state.browse_down() {
            HistoryBrowseResult::RestoreDraft(draft) => {
                assert_eq!(draft.text, "");
                assert_eq!(
                    draft.attachments,
                    vec![attachment(4)],
                    "staged chips return with the draft"
                );
            }
            other => panic!("expected draft restore, got {other:?}"),
        }
    }

    #[test]
    fn browse_up_ignores_non_empty_input_when_not_browsing() {
        let mut state = PromptMemoryState::new();
        state.record_submission("only", &[]);

        assert_eq!(state.browse_up("draft text", &[]), None);
        assert!(!state.is_browsing());
    }

    #[test]
    fn browse_down_outside_browse_is_idle() {
        let mut state = PromptMemoryState::new();
        state.record_submission("only", &[]);

        assert_eq!(state.browse_down(), HistoryBrowseResult::Idle);
    }

    #[test]
    fn clear_browse_drops_browse_state() {
        let mut state = PromptMemoryState::new();
        state.record_submission("a", &[]);
        state.record_submission("b", &[]);

        let _ = state.browse_up("", &[]);
        assert!(state.is_browsing());

        state.clear_browse();
        assert!(!state.is_browsing());
        assert_eq!(state.browse_down(), HistoryBrowseResult::Idle);
    }

    #[test]
    fn record_submission_clears_browse_state() {
        let mut state = PromptMemoryState::new();
        state.record_submission("a", &[]);
        let _ = state.browse_up("", &[]);
        assert!(state.is_browsing());

        assert!(state.record_submission("b", &[]));
        assert!(!state.is_browsing());
    }

    #[test]
    fn consecutive_dupe_record_still_clears_browse() {
        let mut state = PromptMemoryState::new();
        state.record_submission("a", &[]);
        let _ = state.browse_up("", &[]);
        assert!(state.is_browsing());

        assert!(!state.record_submission("a", &[]));
        assert!(!state.is_browsing());
    }

    #[test]
    fn stash_push_pop_is_lifo() {
        let mut state = PromptMemoryState::new();
        state.stash_push("first", &[]);
        state.stash_push("second", &[attachment(2)]);

        let top = state.stash_pop().expect("second");
        assert_eq!(top.text, "second");
        assert_eq!(top.attachments, vec![attachment(2)]);

        let next = state.stash_pop().expect("first");
        assert_eq!(next.text, "first");
        assert!(state.stash().is_empty());
    }

    #[test]
    fn stash_pop_empty_returns_none() {
        let mut state = PromptMemoryState::new();
        assert_eq!(state.stash_pop(), None);
    }

    #[test]
    fn stash_remove_by_index_keeps_other_order() {
        let mut state = PromptMemoryState::new();
        state.stash_push("a", &[]);
        state.stash_push("b", &[]);
        state.stash_push("c", &[]);

        let removed = state.stash_remove_at(1).expect("middle");
        assert_eq!(removed.text, "b");
        assert_eq!(
            state
                .stash()
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert_eq!(state.stash_remove_at(99), None);
    }

    #[test]
    fn stash_has_no_hard_cap() {
        let mut state = PromptMemoryState::new();
        for index in 0..120 {
            state.stash_push(format!("s-{index}"), &[]);
        }
        assert_eq!(state.stash().len(), 120);
        assert_eq!(
            state.stash_pop().map(|entry| entry.text),
            Some("s-119".into())
        );
    }

    #[test]
    fn history_overlay_filters_case_insensitive_newest_first_limit() {
        let mut state = PromptMemoryState::new();
        for index in 0..80 {
            state.record_submission(format!("note-{index}"), &[]);
        }
        state.record_submission("FindMe Unique", &[]);
        state.record_submission("other", &[]);

        let before_len = state.history().len();
        let matches = state.history_overlay("findme", 64);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text, "FindMe Unique");
        assert_eq!(
            state.history().len(),
            before_len,
            "windowing must not prune store"
        );

        let window = state.history_overlay("", 64);
        assert_eq!(window.len(), 64);
        assert_eq!(window[0].text, "other");
        assert_eq!(window[1].text, "FindMe Unique");
        assert_eq!(state.history().len(), before_len);
    }

    #[test]
    fn stash_overlay_newest_first_with_indices_and_limit() {
        let mut state = PromptMemoryState::new();
        for index in 0..70 {
            state.stash_push(format!("stash-{index}"), &[]);
        }

        let before = state.stash().len();
        let window = state.stash_overlay("", 64);
        assert_eq!(window.len(), 64);
        assert_eq!(window[0].text, "stash-69");
        assert_eq!(window[0].store_index, 69);
        assert_eq!(state.stash().len(), before);

        let filtered = state.stash_overlay("STASH-5", 64);
        assert!(
            filtered
                .iter()
                .all(|entry| entry.text.to_ascii_lowercase().contains("stash-5"))
        );
        assert!(!filtered.is_empty());
    }

    #[test]
    fn overlay_items_carry_entry_attachments() {
        let mut state = PromptMemoryState::new();
        state.record_submission("with media", &[attachment(5), attachment(6)]);
        state.stash_push("parked media", &[attachment(8)]);

        let history = state.history_overlay("", 64);
        assert_eq!(history[0].attachments, vec![attachment(5), attachment(6)]);

        let stash = state.stash_overlay("", 64);
        assert_eq!(stash[0].attachments, vec![attachment(8)]);
    }

    #[test]
    fn overlay_search_reaches_attachment_only_entries_and_folds_non_ascii_case() {
        let mut state = PromptMemoryState::new();
        state.record_submission("", &[attachment(5)]);
        state.record_submission("ÉCRIRE un test", &[]);
        state.record_submission("plain text", &[]);

        let by_chip = state.history_overlay("image", 64);
        assert_eq!(
            by_chip.len(),
            1,
            "an attachments-only entry must be findable"
        );
        assert_eq!(by_chip[0].attachments, vec![attachment(5)]);

        let by_mime = state.history_overlay("image/png", 64);
        assert_eq!(by_mime.len(), 1);

        let non_ascii = state.history_overlay("écrire", 64);
        assert_eq!(
            non_ascii.len(),
            1,
            "case folding must not stop at ASCII: {non_ascii:?}"
        );
        assert_eq!(non_ascii[0].text, "ÉCRIRE un test");
    }

    #[test]
    fn ephemeral_implements_prompt_memory_port() {
        let mut memory = EphemeralPromptMemory::new();
        assert!(memory.record_submission("one", &[]).unwrap());
        assert!(!memory.record_submission("one", &[]).unwrap());
        assert_eq!(
            memory.browse_up("", &[]).map(|recall| recall.text),
            Some("one".into())
        );
        memory.clear_browse();

        assert!(memory.stash_push("parked", &[attachment(3)]).unwrap());
        let popped = memory.stash_pop().unwrap().expect("parked");
        assert_eq!(popped.text, "parked");
        assert_eq!(popped.attachments, vec![attachment(3)]);
        assert_eq!(memory.stash_pop().unwrap(), None);
    }
}

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

/// One recorded prompt (history or stash). Text only; media deferred.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptMemoryEntry {
    pub text: String,
    /// Unix seconds (UTC).
    pub created_at: i64,
}

impl PromptMemoryEntry {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            created_at: unix_now_secs(),
        }
    }

    pub fn with_created_at(text: impl Into<String>, created_at: i64) -> Self {
        Self {
            text: text.into(),
            created_at,
        }
    }
}

/// Result of moving toward newer history while browsing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HistoryBrowseResult {
    Entry(String),
    RestoreDraft(String),
    Idle,
}

/// One overlay row with the store index used for stash removal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptOverlayItem {
    pub text: String,
    pub created_at: i64,
    /// Index into the oldest-first store (history index is informational; stash uses it for remove).
    pub store_index: usize,
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
    /// Returns `Ok(true)` when recorded, `Ok(false)` when skipped as a duplicate.
    /// Clears browse either way on success.
    fn record_submission(&mut self, text: &str) -> Result<bool, PromptMemoryError>;

    /// Enter or move browse toward older entries when input is empty (or already browsing).
    fn browse_up(&mut self, composer_input: &str) -> Option<String>;

    /// Move toward newer history, or restore the draft once past the newest entry.
    fn browse_down(&mut self) -> HistoryBrowseResult;

    /// Drop browse state while keeping whatever text the caller holds.
    fn clear_browse(&mut self);

    fn is_browsing(&self) -> bool;

    /// Push onto the LIFO top. Returns `Ok(true)` when pushed.
    fn stash_push(&mut self, text: &str) -> Result<bool, PromptMemoryError>;

    /// Pop the LIFO top, or `Ok(None)` when empty.
    fn stash_pop(&mut self) -> Result<Option<String>, PromptMemoryError>;

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
    draft: String,
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

    /// Append text unless it is a consecutive duplicate of the last entry.
    /// Returns `false` when skipped. Clears browse either way.
    pub fn record_submission(&mut self, text: impl Into<String>) -> bool {
        self.record_submission_at(text, unix_now_secs())
    }

    pub fn record_submission_at(&mut self, text: impl Into<String>, created_at: i64) -> bool {
        let text = text.into();
        self.browse = None;

        if self.history.last().is_some_and(|entry| entry.text == text) {
            return false;
        }

        self.history
            .push(PromptMemoryEntry::with_created_at(text, created_at));
        true
    }

    /// Enter or move browse toward older entries when input is empty (or already browsing).
    pub fn browse_up(&mut self, input: &str) -> Option<String> {
        if self.history.is_empty() {
            return None;
        }

        match &mut self.browse {
            Some(state) => {
                if state.index > 0 {
                    state.index -= 1;
                }
                self.history
                    .get(state.index)
                    .map(|entry| entry.text.clone())
            }
            None => {
                if !input.is_empty() {
                    return None;
                }

                let index = self.history.len().saturating_sub(1);
                self.browse = Some(BrowseState {
                    index,
                    draft: input.to_string(),
                });
                self.history.get(index).map(|entry| entry.text.clone())
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
                Some(entry) => HistoryBrowseResult::Entry(entry.text.clone()),
                None => HistoryBrowseResult::Idle,
            }
        } else {
            let draft = state.draft.clone();
            self.browse = None;
            HistoryBrowseResult::RestoreDraft(draft)
        }
    }

    pub fn clear_browse(&mut self) {
        self.browse = None;
    }

    pub fn stash_push(&mut self, text: impl Into<String>) {
        self.stash_push_at(text, unix_now_secs());
    }

    pub fn stash_push_at(&mut self, text: impl Into<String>, created_at: i64) {
        self.stash
            .push(PromptMemoryEntry::with_created_at(text, created_at));
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
        let query_lower = query.to_ascii_lowercase();
        self.stash
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, entry)| {
                query_lower.is_empty() || entry.text.to_ascii_lowercase().contains(&query_lower)
            })
            .take(limit)
            .map(|(store_index, entry)| PromptOverlayItem {
                text: entry.text.clone(),
                created_at: entry.created_at,
                store_index,
            })
            .collect()
    }
}

fn overlay_items(
    entries: &[PromptMemoryEntry],
    query: &str,
    limit: usize,
) -> Vec<PromptOverlayItem> {
    let query_lower = query.to_ascii_lowercase();
    entries
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, entry)| {
            query_lower.is_empty() || entry.text.to_ascii_lowercase().contains(&query_lower)
        })
        .take(limit)
        .map(|(store_index, entry)| PromptOverlayItem {
            text: entry.text.clone(),
            created_at: entry.created_at,
            store_index,
        })
        .collect()
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
    fn record_submission(&mut self, text: &str) -> Result<bool, PromptMemoryError> {
        Ok(self.state.record_submission(text))
    }

    fn browse_up(&mut self, composer_input: &str) -> Option<String> {
        self.state.browse_up(composer_input)
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

    fn stash_push(&mut self, text: &str) -> Result<bool, PromptMemoryError> {
        self.state.stash_push(text);
        Ok(true)
    }

    fn stash_pop(&mut self) -> Result<Option<String>, PromptMemoryError> {
        Ok(self.state.stash_pop().map(|entry| entry.text))
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

    #[test]
    fn prompt_entry_shape_has_text_and_unix_created_at() {
        let entry = PromptMemoryEntry::new("hello world");

        assert_eq!(entry.text, "hello world");
        assert!(entry.created_at > 0);
    }

    #[test]
    fn record_submission_skips_consecutive_duplicate_text() {
        let mut state = PromptMemoryState::new();

        assert!(state.record_submission("same"));
        assert!(!state.record_submission("same"));
        assert_eq!(state.history().len(), 1);

        assert!(state.record_submission("other"));
        assert!(state.record_submission("same"));
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
    fn record_submission_has_no_hard_product_cap() {
        let mut state = PromptMemoryState::new();

        for index in 0..200 {
            assert!(state.record_submission(format!("entry-{index}")));
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
            PromptMemoryEntry::with_created_at("b", 20),
        ]);
        assert_eq!(state.history().len(), 2);
        assert_eq!(state.history()[0].text, "a");
        assert_eq!(state.history()[1].created_at, 20);

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
        state.record_submission("older");
        state.record_submission("newer");

        let shown = state.browse_up("").expect("enter browse");
        assert_eq!(shown, "newer");
        assert!(state.is_browsing());

        let older = state.browse_up("newer").expect("older");
        assert_eq!(older, "older");

        match state.browse_down() {
            HistoryBrowseResult::Entry(text) => assert_eq!(text, "newer"),
            other => panic!("expected newer entry, got {other:?}"),
        }

        match state.browse_down() {
            HistoryBrowseResult::RestoreDraft(draft) => assert_eq!(draft, ""),
            other => panic!("expected draft restore, got {other:?}"),
        }
        assert!(!state.is_browsing());
    }

    #[test]
    fn browse_up_ignores_non_empty_input_when_not_browsing() {
        let mut state = PromptMemoryState::new();
        state.record_submission("only");

        assert_eq!(state.browse_up("draft text"), None);
        assert!(!state.is_browsing());
    }

    #[test]
    fn browse_down_outside_browse_is_idle() {
        let mut state = PromptMemoryState::new();
        state.record_submission("only");

        assert_eq!(state.browse_down(), HistoryBrowseResult::Idle);
    }

    #[test]
    fn clear_browse_drops_browse_state() {
        let mut state = PromptMemoryState::new();
        state.record_submission("a");
        state.record_submission("b");

        let _ = state.browse_up("");
        assert!(state.is_browsing());

        state.clear_browse();
        assert!(!state.is_browsing());
        assert_eq!(state.browse_down(), HistoryBrowseResult::Idle);
    }

    #[test]
    fn record_submission_clears_browse_state() {
        let mut state = PromptMemoryState::new();
        state.record_submission("a");
        let _ = state.browse_up("");
        assert!(state.is_browsing());

        assert!(state.record_submission("b"));
        assert!(!state.is_browsing());
    }

    #[test]
    fn consecutive_dupe_record_still_clears_browse() {
        let mut state = PromptMemoryState::new();
        state.record_submission("a");
        let _ = state.browse_up("");
        assert!(state.is_browsing());

        assert!(!state.record_submission("a"));
        assert!(!state.is_browsing());
    }

    #[test]
    fn stash_push_pop_is_lifo() {
        let mut state = PromptMemoryState::new();
        state.stash_push("first");
        state.stash_push("second");

        let top = state.stash_pop().expect("second");
        assert_eq!(top.text, "second");

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
        state.stash_push("a");
        state.stash_push("b");
        state.stash_push("c");

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
            state.stash_push(format!("s-{index}"));
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
            state.record_submission(format!("note-{index}"));
        }
        state.record_submission("FindMe Unique");
        state.record_submission("other");

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
            state.stash_push(format!("stash-{index}"));
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
    fn ephemeral_implements_prompt_memory_port() {
        let mut memory = EphemeralPromptMemory::new();
        assert!(memory.record_submission("one").unwrap());
        assert!(!memory.record_submission("one").unwrap());
        assert_eq!(memory.browse_up("").as_deref(), Some("one"));
        memory.clear_browse();

        assert!(memory.stash_push("parked").unwrap());
        assert_eq!(memory.stash_pop().unwrap().as_deref(), Some("parked"));
        assert_eq!(memory.stash_pop().unwrap(), None);
    }
}

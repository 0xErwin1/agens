//! Pure in-memory prompt history and stash reducers.
//!
//! History is chronological with consecutive-dedupe and linear browse state.
//! Stash is an independent LIFO. Neither shares state with the FIFO prompt queue.
//! Overlay helpers window for display only; stores are unbounded.
//!
//! Persistence is injected via [`PromptMemoryPersist`]; this module never touches
//! the filesystem or SQLite.

use std::time::{SystemTime, UNIX_EPOCH};

/// Surface-owned persistence port for global prompt history and stash.
///
/// Implemented by the composition root (`agens-tui-app`) over `agens-store`.
/// Mutations on the pure stores run first; callers best-effort invoke these methods
/// afterward and ignore errors (in-memory state stays authoritative for the session).
pub trait PromptMemoryPersist: Send {
    fn append_history(&mut self, text: &str, created_at: i64) -> Result<(), String>;
    fn replace_stash(&mut self, entries: &[(String, i64)]) -> Result<(), String>;
}

/// One recorded prompt (history or stash). Text only; media deferred.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PromptEntry {
    pub text: String,
    /// Unix seconds (UTC).
    pub created_at: i64,
}

impl PromptEntry {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            created_at: unix_now_secs(),
        }
    }

    pub(crate) fn with_created_at(text: impl Into<String>, created_at: i64) -> Self {
        Self {
            text: text.into(),
            created_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BrowseState {
    index: usize,
    draft: String,
}

/// Result of moving toward newer history while browsing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BrowseResult {
    Entry(String),
    RestoreDraft(String),
    Idle,
}

/// Chronological prompt history with optional Up/Down browse state.
#[derive(Clone, Debug, Default)]
pub(crate) struct PromptHistory {
    entries: Vec<PromptEntry>,
    browse: Option<BrowseState>,
}

impl PromptHistory {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Seed chronological entries (oldest first). Browse state starts idle.
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = PromptEntry>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
            browse: None,
        }
    }

    pub(crate) fn entries(&self) -> &[PromptEntry] {
        &self.entries
    }

    #[allow(dead_code)] // tests / capacity assertions
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)] // tests / capacity assertions
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn is_browsing(&self) -> bool {
        self.browse.is_some()
    }

    /// Append text unless it is a consecutive duplicate of the last entry.
    /// Returns `false` when skipped as a consecutive duplicate. Clears browse either way.
    pub(crate) fn append(&mut self, text: impl Into<String>) -> bool {
        let text = text.into();
        self.browse = None;

        if self.entries.last().is_some_and(|entry| entry.text == text) {
            return false;
        }

        self.entries.push(PromptEntry::new(text));
        true
    }

    /// Enter or move browse toward older entries when input is empty (or already browsing).
    pub(crate) fn browse_up(&mut self, input: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }

        match &mut self.browse {
            Some(state) => {
                if state.index > 0 {
                    state.index -= 1;
                }
                self.entries
                    .get(state.index)
                    .map(|entry| entry.text.clone())
            }
            None => {
                if !input.is_empty() {
                    return None;
                }

                let index = self.entries.len().saturating_sub(1);
                self.browse = Some(BrowseState {
                    index,
                    draft: input.to_string(),
                });
                self.entries.get(index).map(|entry| entry.text.clone())
            }
        }
    }

    /// Move toward newer history, or restore the draft once past the newest entry.
    pub(crate) fn browse_down(&mut self) -> BrowseResult {
        let Some(state) = self.browse.as_ref() else {
            return BrowseResult::Idle;
        };

        if state.index + 1 < self.entries.len() {
            let index = state.index + 1;
            if let Some(browse) = self.browse.as_mut() {
                browse.index = index;
            }
            match self.entries.get(index) {
                Some(entry) => BrowseResult::Entry(entry.text.clone()),
                None => BrowseResult::Idle,
            }
        } else {
            let draft = state.draft.clone();
            self.browse = None;
            BrowseResult::RestoreDraft(draft)
        }
    }

    /// Drop browse state while keeping whatever text the caller holds.
    pub(crate) fn clear_browse(&mut self) {
        self.browse = None;
    }

    /// Newest-first filtered window for overlay display. Does not mutate the store.
    pub(crate) fn overlay_entries(&self, query: &str, limit: usize) -> Vec<&PromptEntry> {
        overlay_filter(&self.entries, query, limit)
    }
}

/// LIFO prompt stash (vector end is the top). Independent of history.
#[derive(Clone, Debug, Default)]
pub(crate) struct PromptStash {
    entries: Vec<PromptEntry>,
}

impl PromptStash {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Seed oldest-first entries (last element is LIFO top).
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = PromptEntry>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[PromptEntry] {
        &self.entries
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn push(&mut self, text: impl Into<String>) {
        self.entries.push(PromptEntry::new(text));
    }

    pub(crate) fn pop(&mut self) -> Option<PromptEntry> {
        self.entries.pop()
    }

    /// Remove by store index (0 = oldest). Returns the removed entry when in range.
    pub(crate) fn remove(&mut self, index: usize) -> Option<PromptEntry> {
        if index < self.entries.len() {
            Some(self.entries.remove(index))
        } else {
            None
        }
    }

    /// Snapshot for full-stack rewrite persistence (oldest first).
    pub(crate) fn persist_pairs(&self) -> Vec<(String, i64)> {
        self.entries
            .iter()
            .map(|entry| (entry.text.clone(), entry.created_at))
            .collect()
    }

    /// Newest-first filtered window with original indices for remove. Display only.
    pub(crate) fn overlay_entries(&self, query: &str, limit: usize) -> Vec<(usize, &PromptEntry)> {
        let query_lower = query.to_ascii_lowercase();
        self.entries
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, entry)| {
                query_lower.is_empty() || entry.text.to_ascii_lowercase().contains(&query_lower)
            })
            .take(limit)
            .collect()
    }
}

fn overlay_filter<'a>(
    entries: &'a [PromptEntry],
    query: &str,
    limit: usize,
) -> Vec<&'a PromptEntry> {
    let query_lower = query.to_ascii_lowercase();
    entries
        .iter()
        .rev()
        .filter(|entry| {
            query_lower.is_empty() || entry.text.to_ascii_lowercase().contains(&query_lower)
        })
        .take(limit)
        .collect()
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Format unix seconds as `YYYY-MM-DDTHH:MM:SSZ` for overlay labels.
pub(crate) fn format_unix_secs_rfc3339(secs: i64) -> String {
    let secs = u64::try_from(secs.max(0)).unwrap_or(0);

    const SECS_PER_DAY: u64 = 86_400;
    const DAYS_PER_CYCLE: u64 = 146_097;
    const SECS_PER_HOUR: u64 = 3_600;
    const SECS_PER_MIN: u64 = 60;

    let days = secs / SECS_PER_DAY;
    let day_secs = secs % SECS_PER_DAY;
    let hour = day_secs / SECS_PER_HOUR;
    let minute = (day_secs % SECS_PER_HOUR) / SECS_PER_MIN;
    let second = day_secs % SECS_PER_MIN;

    // Civil date from days since Unix epoch (1970-01-01), Howard Hinnant algorithm.
    let z = days + 719_468;
    let era = z / DAYS_PER_CYCLE;
    let doe = z - era * DAYS_PER_CYCLE;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Date portion (`YYYY-MM-DD`) of a unix-seconds timestamp for overlay right labels.
pub(crate) fn prompt_entry_date_label(created_at: i64) -> String {
    format_unix_secs_rfc3339(created_at)
        .get(..10)
        .unwrap_or("")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- 1.1 PromptEntry shape, consecutive-dedupe, no hard cap ---

    #[test]
    fn prompt_entry_shape_has_text_and_unix_created_at() {
        let entry = PromptEntry::new("hello world");

        assert_eq!(entry.text, "hello world");
        assert!(entry.created_at > 0);
        let label = format_unix_secs_rfc3339(entry.created_at);
        assert!(
            label.ends_with('Z') && label.contains('T'),
            "formatted created_at should be RFC3339 UTC, got {label}"
        );
    }

    #[test]
    fn append_records_text_with_unix_created_at() {
        let mut history = PromptHistory::new();

        assert!(history.append("first submit"));
        assert_eq!(history.len(), 1);
        assert_eq!(history.entries()[0].text, "first submit");
        assert!(history.entries()[0].created_at > 0);
    }

    #[test]
    fn append_skips_consecutive_duplicate_text() {
        let mut history = PromptHistory::new();

        assert!(history.append("same"));
        assert!(!history.append("same"));
        assert_eq!(history.len(), 1);

        assert!(history.append("other"));
        assert!(history.append("same"));
        assert_eq!(history.len(), 3);
        assert_eq!(
            history
                .entries()
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            vec!["same", "other", "same"]
        );
    }

    #[test]
    fn append_has_no_hard_product_cap() {
        let mut history = PromptHistory::new();

        for index in 0..200 {
            assert!(history.append(format!("entry-{index}")));
        }

        assert_eq!(history.len(), 200);
        assert_eq!(history.entries()[0].text, "entry-0");
        assert_eq!(history.entries()[199].text, "entry-199");
    }

    #[test]
    fn from_entries_seeds_history_and_stash() {
        let history = PromptHistory::from_entries([
            PromptEntry::with_created_at("a", 10),
            PromptEntry::with_created_at("b", 20),
        ]);
        assert_eq!(history.len(), 2);
        assert_eq!(history.entries()[0].text, "a");
        assert_eq!(history.entries()[1].created_at, 20);

        let stash = PromptStash::from_entries([
            PromptEntry::with_created_at("s1", 1),
            PromptEntry::with_created_at("s2", 2),
        ]);
        assert_eq!(stash.len(), 2);
        assert_eq!(stash.entries()[1].text, "s2");
    }

    // --- 1.2 Browse state ---

    #[test]
    fn browse_up_from_empty_input_shows_newest_and_preserves_draft() {
        let mut history = PromptHistory::new();
        history.append("older");
        history.append("newer");

        let shown = history.browse_up("").expect("enter browse");
        assert_eq!(shown, "newer");
        assert!(history.is_browsing());

        let older = history.browse_up("newer").expect("older");
        assert_eq!(older, "older");

        match history.browse_down() {
            BrowseResult::Entry(text) => assert_eq!(text, "newer"),
            other => panic!("expected newer entry, got {other:?}"),
        }

        match history.browse_down() {
            BrowseResult::RestoreDraft(draft) => assert_eq!(draft, ""),
            other => panic!("expected draft restore, got {other:?}"),
        }
        assert!(!history.is_browsing());
    }

    #[test]
    fn browse_up_ignores_non_empty_input_when_not_browsing() {
        let mut history = PromptHistory::new();
        history.append("only");

        assert_eq!(history.browse_up("draft text"), None);
        assert!(!history.is_browsing());
    }

    #[test]
    fn browse_down_outside_browse_is_idle() {
        let mut history = PromptHistory::new();
        history.append("only");

        assert_eq!(history.browse_down(), BrowseResult::Idle);
    }

    #[test]
    fn clear_browse_drops_browse_state() {
        let mut history = PromptHistory::new();
        history.append("a");
        history.append("b");

        let _ = history.browse_up("");
        assert!(history.is_browsing());

        history.clear_browse();
        assert!(!history.is_browsing());
        assert_eq!(history.browse_down(), BrowseResult::Idle);
    }

    #[test]
    fn append_clears_browse_state() {
        let mut history = PromptHistory::new();
        history.append("a");
        let _ = history.browse_up("");
        assert!(history.is_browsing());

        assert!(history.append("b"));
        assert!(!history.is_browsing());
    }

    #[test]
    fn consecutive_dupe_append_still_clears_browse() {
        let mut history = PromptHistory::new();
        history.append("a");
        let _ = history.browse_up("");
        assert!(history.is_browsing());

        assert!(!history.append("a"));
        assert!(!history.is_browsing());
    }

    // --- 1.3 PromptStash LIFO ---

    #[test]
    fn stash_push_pop_is_lifo() {
        let mut stash = PromptStash::new();
        stash.push("first");
        stash.push("second");

        let top = stash.pop().expect("second");
        assert_eq!(top.text, "second");

        let next = stash.pop().expect("first");
        assert_eq!(next.text, "first");
        assert!(stash.is_empty());
    }

    #[test]
    fn stash_pop_empty_returns_none() {
        let mut stash = PromptStash::new();
        assert_eq!(stash.pop(), None);
    }

    #[test]
    fn stash_remove_by_index_keeps_other_order() {
        let mut stash = PromptStash::new();
        stash.push("a");
        stash.push("b");
        stash.push("c");

        let removed = stash.remove(1).expect("middle");
        assert_eq!(removed.text, "b");
        assert_eq!(
            stash
                .entries()
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert_eq!(stash.remove(99), None);
    }

    #[test]
    fn stash_has_no_hard_cap() {
        let mut stash = PromptStash::new();
        for index in 0..120 {
            stash.push(format!("s-{index}"));
        }
        assert_eq!(stash.len(), 120);
        assert_eq!(stash.pop().map(|entry| entry.text), Some("s-119".into()));
    }

    #[test]
    fn stash_persist_pairs_are_oldest_first() {
        let mut stash = PromptStash::new();
        stash.push("a");
        stash.push("b");
        let pairs = stash.persist_pairs();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "a");
        assert_eq!(pairs[1].0, "b");
    }

    // --- 1.5 overlay_entries windowing ---

    #[test]
    fn history_overlay_filters_case_insensitive_newest_first_limit() {
        let mut history = PromptHistory::new();
        for index in 0..80 {
            history.append(format!("note-{index}"));
        }
        history.append("FindMe Unique");
        history.append("other");

        let before_len = history.len();
        let matches = history.overlay_entries("findme", 64);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].text, "FindMe Unique");
        assert_eq!(history.len(), before_len, "windowing must not prune store");

        let window = history.overlay_entries("", 64);
        assert_eq!(window.len(), 64);
        assert_eq!(window[0].text, "other");
        assert_eq!(window[1].text, "FindMe Unique");
        assert_eq!(history.len(), before_len);
    }

    #[test]
    fn stash_overlay_newest_first_with_indices_and_limit() {
        let mut stash = PromptStash::new();
        for index in 0..70 {
            stash.push(format!("stash-{index}"));
        }

        let before = stash.len();
        let window = stash.overlay_entries("", 64);
        assert_eq!(window.len(), 64);
        assert_eq!(window[0].1.text, "stash-69");
        assert_eq!(window[0].0, 69);
        assert_eq!(stash.len(), before);

        let filtered = stash.overlay_entries("STASH-5", 64);
        assert!(
            filtered
                .iter()
                .all(|(_, entry)| entry.text.to_ascii_lowercase().contains("stash-5"))
        );
        assert!(!filtered.is_empty());
    }

    #[test]
    fn format_unix_secs_rfc3339_known_instant() {
        // 2026-08-05T12:00:00Z
        assert_eq!(
            format_unix_secs_rfc3339(1_785_931_200),
            "2026-08-05T12:00:00Z"
        );
        assert_eq!(prompt_entry_date_label(1_785_931_200), "2026-08-05");
    }
}

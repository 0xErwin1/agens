//! The keyboard shortcut catalogue and the overlay that shows it.
//!
//! This is the single place a key is described. The footer used to name a few
//! of them next to the state they changed, which put instructions in permanent
//! competition with data and — worse — drifted out of date the moment the
//! keymap moved. Everything the reader can press is listed here instead, in the
//! crate that owns the keymap, so a binding cannot change without its
//! description sitting in the same diff.

use crate::{DialogEntry, DialogView};

/// One binding: what to press, and what it does.
pub struct Shortcut {
    pub keys: &'static str,
    pub label: &'static str,
}

/// Bindings that answer the same question, under the heading that names it.
pub struct ShortcutGroup {
    pub title: &'static str,
    pub shortcuts: &'static [Shortcut],
}

const fn shortcut(keys: &'static str, label: &'static str) -> Shortcut {
    Shortcut { keys, label }
}

pub const SHORTCUTS: &[ShortcutGroup] = &[
    ShortcutGroup {
        title: "Essentials",
        shortcuts: &[
            shortcut("Enter", "Send the prompt"),
            shortcut("Shift+Enter", "Newline without sending"),
            shortcut("Esc", "Focus the transcript (never cancels)"),
            shortcut("Esc Esc", "Cancel the running turn"),
            shortcut("Ctrl+C Ctrl+C", "Quit"),
            shortcut("Ctrl+?", "This list"),
            shortcut("/", "Command palette"),
            shortcut("@", "Insert a project file"),
        ],
    },
    ShortcutGroup {
        title: "Transcript (Normal mode)",
        shortcuts: &[
            shortcut("i / a", "Back to the prompt"),
            shortcut("j / k", "Scroll one row"),
            shortcut("Ctrl+D / Ctrl+U", "Scroll half a page"),
            shortcut("gg / G", "Top / bottom"),
            shortcut("{ / }", "Previous / next prompt"),
            shortcut("J / K", "Walk tool blocks"),
            shortcut("o", "Cycle the focused block's detail"),
            shortcut("[ / ]", "Previous / next sibling transcript"),
            shortcut("m", "Back to the main transcript"),
            shortcut("gt", "Choose a transcript"),
            shortcut("x", "Cancel the focused subagent"),
        ],
    },
    ShortcutGroup {
        title: "Selection",
        shortcuts: &[
            shortcut("Drag", "Select transcript text"),
            shortcut("Click", "Drop the selection"),
            shortcut("Ctrl+Shift+C", "Copy the selection"),
            shortcut("Ctrl+C", "Copy when something is selected"),
            shortcut("Esc", "Drop the selection"),
        ],
    },
    ShortcutGroup {
        title: "Display",
        shortcuts: &[
            shortcut("Ctrl+O", "More tool output"),
            shortcut("Ctrl+Shift+O", "Less tool output"),
            shortcut("Ctrl+T", "Show or hide reasoning"),
            shortcut("Ctrl+Y", "Unfold elided history"),
        ],
    },
    ShortcutGroup {
        title: "Navigation",
        shortcuts: &[
            shortcut("Ctrl+J / Ctrl+K", "Scroll without leaving the prompt"),
            shortcut("Ctrl+G / Ctrl+Shift+G", "Top / bottom"),
            shortcut("Ctrl+N", "Jump to the last prompt"),
            shortcut("Ctrl+Shift+N", "Jump to the previous prompt"),
            shortcut("PageUp / PageDown", "Scroll a page"),
        ],
    },
    ShortcutGroup {
        title: "Subagents",
        shortcuts: &[
            shortcut("Down", "Enter the subagent tree from an empty prompt"),
            shortcut("Up / Down", "Walk the tree"),
            shortcut("Up", "Leave the tree from its first row"),
            shortcut("Enter", "Open the selected subagent"),
            shortcut("Enter on Main", "Back to the prompt"),
            shortcut("Tab", "Enter or leave the tree"),
        ],
    },
    ShortcutGroup {
        title: "Session",
        shortcuts: &[
            shortcut("Ctrl+Shift+A", "Choose a subagent"),
            shortcut("Ctrl+Shift+M", "Subagent model profiles"),
            shortcut("Ctrl+Shift+D", "Toggle dangerous mode"),
            shortcut("Ctrl+Shift+P", "Toggle permission bypass"),
            shortcut("Ctrl+B", "Move a subagent to the background"),
        ],
    },
];

/// The catalogue as a searchable, read-only overlay.
///
/// Group headings are rows rather than a nested tree: the dialog already knows
/// how to filter rows, and a filtered tree that hides its own headings would
/// answer a search with rows the reader cannot place.
pub fn shortcuts_dialog() -> DialogView {
    let mut entries = Vec::new();
    for group in SHORTCUTS {
        entries.push(DialogEntry::disabled(group.title, ""));
        for binding in group.shortcuts {
            entries.push(DialogEntry::reference(binding.label, binding.keys));
        }
    }

    DialogView::selection(
        "Keyboard shortcuts",
        Some("/ to search · Esc to close"),
        entries,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dialog caps its entries, so the catalogue has to fit inside the cap
    /// or it would silently stop listing whatever came last.
    #[test]
    fn the_catalogue_fits_the_dialog_entry_cap() {
        let rows = SHORTCUTS.len()
            + SHORTCUTS
                .iter()
                .map(|group| group.shortcuts.len())
                .sum::<usize>();

        assert!(rows <= 64, "{rows} rows exceed the dialog cap");
    }

    #[test]
    fn every_binding_names_a_key_and_what_it_does() {
        for group in SHORTCUTS {
            assert!(!group.title.is_empty());
            for binding in group.shortcuts {
                assert!(!binding.keys.is_empty(), "{} has no key", binding.label);
                assert!(!binding.label.is_empty(), "{} has no label", binding.keys);
            }
        }
    }

    /// Every row survives into the overlay, headings included.
    #[test]
    fn the_overlay_lists_the_whole_catalogue() {
        let dialog = shortcuts_dialog();
        let rows = SHORTCUTS.len()
            + SHORTCUTS
                .iter()
                .map(|group| group.shortcuts.len())
                .sum::<usize>();

        assert_eq!(dialog.entry_count(), rows);
    }
}

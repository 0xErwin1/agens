use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_core::{PromptAttachment, PromptMemoryEntry};
use agens_store::PromptMemoryStore;
use rusqlite::Connection;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-store-prompt-memory-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

#[test]
fn history_append_survives_reopen_in_agens_db() {
    let directory = data_directory();

    {
        let mut store = PromptMemoryStore::open(&directory).unwrap();
        assert_eq!(store.database_path(), directory.join("agens.db"));
        assert!(store.list_history().unwrap().is_empty());

        let first = store
            .append_history("alpha", &[])
            .unwrap()
            .expect("inserted");
        assert_eq!(first.text, "alpha");
        assert!(first.id > 0);
        assert!(first.created_at > 0);

        let second = store
            .append_history("beta", &[])
            .unwrap()
            .expect("inserted");
        assert_eq!(second.text, "beta");
        assert!(second.id > first.id);
    }

    let reopened = PromptMemoryStore::open(&directory).unwrap();
    let history = reopened.list_history().unwrap();
    assert_eq!(
        history
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "beta"]
    );
    assert!(history[0].id < history[1].id);

    let connection = Connection::open(directory.join("agens.db")).unwrap();
    let ids: Vec<String> = connection
        .prepare("SELECT id FROM schema_migrations ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        ids.iter().any(|id| id == "0008_prompt_memory"),
        "migration 0008 must be applied, got {ids:?}"
    );
    let table_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE type = 'table' AND name IN ('prompt_history', 'prompt_stash')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_count, 2);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn history_skips_consecutive_duplicate_of_last_row() {
    let directory = data_directory();
    let mut store = PromptMemoryStore::open(&directory).unwrap();

    assert!(store.append_history("same", &[]).unwrap().is_some());
    assert!(store.append_history("same", &[]).unwrap().is_none());
    assert_eq!(store.list_history().unwrap().len(), 1);

    assert!(store.append_history("other", &[]).unwrap().is_some());
    assert!(store.append_history("same", &[]).unwrap().is_some());
    assert_eq!(
        store
            .list_history()
            .unwrap()
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        vec!["same", "other", "same"]
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn stash_push_pop_remove_survive_reopen() {
    let directory = data_directory();

    {
        let mut store = PromptMemoryStore::open(&directory).unwrap();
        assert!(store.list_stash().unwrap().is_empty());

        store.push_stash("oldest", &[]).unwrap();
        store.push_stash("middle", &[]).unwrap();
        store.push_stash("newest", &[]).unwrap();

        let popped = store.pop_stash().unwrap().expect("newest");
        assert_eq!(popped.text, "newest");

        let removed = store.remove_stash_at(0).unwrap().expect("oldest");
        assert_eq!(removed.text, "oldest");

        let remaining = store.list_stash().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "middle");
    }

    let reopened = PromptMemoryStore::open(&directory).unwrap();
    let stash = reopened.list_stash().unwrap();
    assert_eq!(stash.len(), 1);
    assert_eq!(stash[0].text, "middle");

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn replace_stash_rewrites_full_stack_in_order() {
    let directory = data_directory();
    let mut store = PromptMemoryStore::open(&directory).unwrap();

    store.push_stash("drop-me", &[]).unwrap();
    store
        .replace_stash(&[
            PromptMemoryEntry::with_created_at("keep-a", 1_700_000_000),
            PromptMemoryEntry::with_created_at("keep-b", 1_700_000_100),
        ])
        .unwrap();

    let stash = store.list_stash().unwrap();
    assert_eq!(
        stash
            .iter()
            .map(|entry| (entry.text.as_str(), entry.created_at))
            .collect::<Vec<_>>(),
        vec![("keep-a", 1_700_000_000), ("keep-b", 1_700_000_100)]
    );

    store.replace_stash(&[]).unwrap();
    assert!(store.list_stash().unwrap().is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn history_and_stash_have_no_hard_product_cap() {
    let directory = data_directory();
    let mut store = PromptMemoryStore::open(&directory).unwrap();

    for index in 0..120 {
        store
            .append_history(&format!("h-{index}"), &[])
            .unwrap()
            .expect("insert");
        store.push_stash(&format!("s-{index}"), &[]).unwrap();
    }

    assert_eq!(store.list_history().unwrap().len(), 120);
    assert_eq!(store.list_stash().unwrap().len(), 120);
    assert_eq!(
        store.pop_stash().unwrap().map(|entry| entry.text),
        Some("s-119".into())
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn fully_empty_entries_are_rejected_but_attachment_only_entries_are_not() {
    let directory = data_directory();
    let mut store = PromptMemoryStore::open(&directory).unwrap();

    assert!(store.append_history("", &[]).is_err());
    assert!(store.push_stash("", &[]).is_err());
    assert!(
        store
            .replace_stash(&[PromptMemoryEntry::with_created_at("", 1)])
            .is_err()
    );
    assert!(store.list_history().unwrap().is_empty());
    assert!(store.list_stash().unwrap().is_empty());

    let chips = vec![PromptAttachment::new(11, "image/png")];
    let pushed = store.push_stash("", &chips).unwrap();
    assert_eq!(pushed.text, "");
    assert_eq!(pushed.attachments, chips);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn stash_round_trip_preserves_attachments_across_reopen() {
    let directory = data_directory();
    let chips = vec![
        PromptAttachment::new(3, "image/png"),
        PromptAttachment::new(9, "application/pdf"),
    ];

    {
        let mut store = PromptMemoryStore::open(&directory).unwrap();
        store.push_stash("with media", &chips).unwrap();
        store.push_stash("text only", &[]).unwrap();
    }

    let mut reopened = PromptMemoryStore::open(&directory).unwrap();
    let stash = reopened.list_stash().unwrap();
    assert_eq!(stash[0].attachments, chips);
    assert!(stash[1].attachments.is_empty());

    let top = reopened.pop_stash().unwrap().expect("text only");
    assert!(top.attachments.is_empty());
    let with_media = reopened.pop_stash().unwrap().expect("with media");
    assert_eq!(with_media.text, "with media");
    assert_eq!(with_media.attachments, chips);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn history_dedupe_considers_attachments_and_persists_them() {
    let directory = data_directory();
    let chips = vec![PromptAttachment::new(5, "image/jpeg")];

    {
        let mut store = PromptMemoryStore::open(&directory).unwrap();
        assert!(store.append_history("look", &chips).unwrap().is_some());
        assert!(
            store.append_history("look", &[]).unwrap().is_some(),
            "same text without the media is a distinct prompt"
        );
        assert!(
            store.append_history("look", &[]).unwrap().is_none(),
            "identical text and attachments dedupe"
        );
    }

    let reopened = PromptMemoryStore::open(&directory).unwrap();
    let history = reopened.list_history().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].attachments, chips);
    assert!(history[1].attachments.is_empty());

    fs::remove_dir_all(directory).unwrap();
}

/// Writes `agens.db` at the schema migration 0010 inherits: the pre-media prompt tables
/// (with the old `text <> ''` CHECK, no `attachments` column) and a ledger stopping at 0009.
fn seed_pre_media_prompt_tables(directory: &std::path::Path, rows: &[(i64, &str, i64)]) {
    let connection = Connection::open(directory.join("agens.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE schema_migrations (id TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);
             CREATE TABLE prompt_history (
                 id INTEGER PRIMARY KEY,
                 text TEXT NOT NULL CHECK(text <> ''),
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX prompt_history_id ON prompt_history(id);
             CREATE TABLE prompt_stash (
                 id INTEGER PRIMARY KEY,
                 text TEXT NOT NULL CHECK(text <> ''),
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX prompt_stash_id ON prompt_stash(id);",
        )
        .unwrap();

    for id in [
        "0001_permission_grants",
        "0002_model_preference",
        "0003_sessions_v5",
        "0004_tool_result_facts",
        "0005_session_confinement_root",
        "0006_session_bypass_permission_prompts",
        "0007_model_preference_by_source",
        "0008_prompt_memory",
        "0009_media",
    ] {
        connection
            .execute(
                "INSERT INTO schema_migrations (id, applied_at) VALUES (?1, 0)",
                rusqlite::params![id],
            )
            .unwrap();
    }

    for (id, text, created_at) in rows {
        for table in ["prompt_history", "prompt_stash"] {
            connection
                .execute(
                    &format!("INSERT INTO {table} (id, text, created_at) VALUES (?1, ?2, ?3)"),
                    rusqlite::params![id, text, created_at],
                )
                .unwrap();
        }
    }
}

/// Rows written before migration 0010 have no attachments column at all; opening the store
/// runs the rebuild for real and every row must come through it unchanged, text-only.
#[test]
fn pre_media_rows_survive_the_attachments_migration_as_text_only() {
    let directory = data_directory();
    let rows = [
        (1, "legacy", 100),
        (2, "parked", 200),
        (7, "sparse id", 300),
    ];
    seed_pre_media_prompt_tables(&directory, &rows);

    let store = PromptMemoryStore::open(&directory).unwrap();

    for entries in [store.list_history().unwrap(), store.list_stash().unwrap()] {
        assert_eq!(entries.len(), rows.len(), "the rebuild must lose no row");
        for (entry, (id, text, created_at)) in entries.iter().zip(rows) {
            assert_eq!(entry.id, id, "ids must survive the rebuild");
            assert_eq!(entry.text, text);
            assert_eq!(entry.created_at, created_at);
            assert!(entry.attachments.is_empty());
        }
    }

    let connection = Connection::open(directory.join("agens.db")).unwrap();
    let applied: i64 = connection
        .query_row(
            "SELECT count(*) FROM schema_migrations WHERE id = '0010_prompt_memory_media'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(applied, 1, "the migration must actually have run");

    // The relaxed CHECK is what the rebuild was for: an attachments-only row is now legal.
    connection
        .execute(
            "INSERT INTO prompt_history (text, created_at, attachments)
             VALUES ('', 400, '[[9,\"image/png\"]]')",
            [],
        )
        .unwrap();

    drop(connection);
    fs::remove_dir_all(directory).unwrap();
}

/// One row whose attachments JSON does not match the stored pair shape must cost that row's
/// attachments only, never the whole history and stash.
#[test]
fn an_undecodable_attachments_row_loads_as_text_and_is_counted() {
    let directory = data_directory();
    let chips = vec![PromptAttachment::new(4, "image/png")];

    {
        let mut store = PromptMemoryStore::open(&directory).unwrap();
        store.append_history("first", &[]).unwrap();
        store.append_history("broken", &chips).unwrap();
        store.append_history("last", &[]).unwrap();
        store.push_stash("parked", &chips).unwrap();
    }

    {
        let connection = Connection::open(directory.join("agens.db")).unwrap();
        connection
            .execute(
                "UPDATE prompt_history SET attachments = '[1,2,3]' WHERE text = 'broken'",
                [],
            )
            .unwrap();
        connection
            .execute("UPDATE prompt_stash SET attachments = '[{\"a\":1}]'", [])
            .unwrap();
    }

    let reopened = PromptMemoryStore::open(&directory).unwrap();
    let history = reopened.list_history().unwrap();
    assert_eq!(
        history
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "broken", "last"]
    );
    assert!(history[1].attachments.is_empty());
    assert_eq!(reopened.list_stash().unwrap()[0].text, "parked");
    assert_eq!(reopened.undecodable_attachment_rows(), 2);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn history_and_stash_are_independent_tables() {
    let directory = data_directory();
    let mut store = PromptMemoryStore::open(&directory).unwrap();

    store.append_history("only-history", &[]).unwrap();
    store.push_stash("only-stash", &[]).unwrap();

    assert_eq!(
        store
            .list_history()
            .unwrap()
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        vec!["only-history"]
    );
    assert_eq!(
        store
            .list_stash()
            .unwrap()
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        vec!["only-stash"]
    );

    fs::remove_dir_all(directory).unwrap();
}

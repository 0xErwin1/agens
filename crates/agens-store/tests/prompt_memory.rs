use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

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

        let first = store.append_history("alpha").unwrap().expect("inserted");
        assert_eq!(first.text, "alpha");
        assert!(first.id > 0);
        assert!(first.created_at > 0);

        let second = store.append_history("beta").unwrap().expect("inserted");
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

    assert!(store.append_history("same").unwrap().is_some());
    assert!(store.append_history("same").unwrap().is_none());
    assert_eq!(store.list_history().unwrap().len(), 1);

    assert!(store.append_history("other").unwrap().is_some());
    assert!(store.append_history("same").unwrap().is_some());
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

        store.push_stash("oldest").unwrap();
        store.push_stash("middle").unwrap();
        store.push_stash("newest").unwrap();

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

    store.push_stash("drop-me").unwrap();
    store
        .replace_stash(&[
            ("keep-a".to_owned(), 1_700_000_000),
            ("keep-b".to_owned(), 1_700_000_100),
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
            .append_history(&format!("h-{index}"))
            .unwrap()
            .expect("insert");
        store.push_stash(&format!("s-{index}")).unwrap();
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
fn empty_text_is_rejected_for_history_and_stash() {
    let directory = data_directory();
    let mut store = PromptMemoryStore::open(&directory).unwrap();

    assert!(store.append_history("").is_err());
    assert!(store.push_stash("").is_err());
    assert!(store.replace_stash(&[("".to_owned(), 1)]).is_err());
    assert!(store.list_history().unwrap().is_empty());
    assert!(store.list_stash().unwrap().is_empty());

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn history_and_stash_are_independent_tables() {
    let directory = data_directory();
    let mut store = PromptMemoryStore::open(&directory).unwrap();

    store.append_history("only-history").unwrap();
    store.push_stash("only-stash").unwrap();

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

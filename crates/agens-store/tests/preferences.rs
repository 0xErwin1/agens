use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
};

use agens_core::ReasoningEffort;
use agens_store::{ModelPreference, PreferenceStore};
use rusqlite::Connection;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

fn data_directory() -> std::path::PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "agens-store-preferences-{}-{suffix}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    directory
}

#[test]
fn a_remembered_model_and_effort_survive_a_reopen_as_one_selection() {
    let directory = data_directory();
    let first = ModelPreference::new("gpt-5.5", Some(ReasoningEffort::High));
    let second = ModelPreference::new("gpt-4.1", None);

    {
        let mut store = PreferenceStore::open(&directory).unwrap();
        assert_eq!(store.remembered_model().unwrap(), None);
        assert_eq!(store.database_path(), directory.join("preferences.db"));

        store.remember_model(&first).unwrap();
        assert_eq!(store.remembered_model().unwrap(), Some(first));
    }

    let database = directory.join("preferences.db");
    assert_eq!(
        Connection::open(&database)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );

    let mut reopened = PreferenceStore::open(&directory).unwrap();
    reopened.remember_model(&second).unwrap();
    assert_eq!(reopened.remembered_model().unwrap(), Some(second));

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_invalid_model_identifier_never_reaches_the_database() {
    let directory = data_directory();
    let mut store = PreferenceStore::open(&directory).unwrap();

    assert!(
        store
            .remember_model(&ModelPreference::new("", None))
            .is_err()
    );
    assert!(
        store
            .remember_model(&ModelPreference::new("x".repeat(65), None))
            .is_err()
    );
    assert!(
        store
            .remember_model(&ModelPreference::new("gpt 5.5", None))
            .is_err()
    );
    assert_eq!(store.remembered_model().unwrap(), None);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_corrupted_effort_is_reported_instead_of_silently_dropped() {
    let directory = data_directory();
    {
        let mut store = PreferenceStore::open(&directory).unwrap();
        store
            .remember_model(&ModelPreference::new("gpt-5.5", Some(ReasoningEffort::Low)))
            .unwrap();
    }

    Connection::open(directory.join("preferences.db"))
        .unwrap()
        .execute("UPDATE model_preference SET reasoning_effort = 'turbo'", [])
        .unwrap();

    assert!(
        PreferenceStore::open(&directory)
            .unwrap()
            .remembered_model()
            .is_err()
    );

    fs::remove_dir_all(directory).unwrap();
}

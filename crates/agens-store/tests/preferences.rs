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
        assert_eq!(store.remembered_model("openai-api").unwrap(), None);
        assert_eq!(store.database_path(), directory.join("agens.db"));

        store.remember_model("openai-api", &first).unwrap();
        assert_eq!(store.remembered_model("openai-api").unwrap(), Some(first));
    }

    let database = directory.join("agens.db");
    let connection = Connection::open(&database).unwrap();
    let ids: Vec<String> = connection
        .prepare("SELECT id FROM schema_migrations ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        ids,
        vec![
            "0001_permission_grants",
            "0002_model_preference",
            "0003_sessions_v5",
            "0004_tool_result_facts",
            "0005_session_confinement_root",
            "0006_session_bypass_permission_prompts",
            "0007_model_preference_by_source"
        ]
    );
    drop(connection);

    let mut reopened = PreferenceStore::open(&directory).unwrap();
    reopened.remember_model("openai-api", &second).unwrap();
    assert_eq!(
        reopened.remembered_model("openai-api").unwrap(),
        Some(second)
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_invalid_model_identifier_never_reaches_the_database() {
    let directory = data_directory();
    let mut store = PreferenceStore::open(&directory).unwrap();

    assert!(
        store
            .remember_model("openai-api", &ModelPreference::new("", None))
            .is_err()
    );
    assert!(
        store
            .remember_model("openai-api", &ModelPreference::new("x".repeat(65), None))
            .is_err()
    );
    assert!(
        store
            .remember_model("openai-api", &ModelPreference::new("gpt 5.5", None))
            .is_err()
    );
    assert!(
        store
            .remember_model("", &ModelPreference::new("gpt-5.5", None))
            .is_err()
    );
    assert_eq!(store.remembered_model("openai-api").unwrap(), None);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_corrupted_effort_is_reported_instead_of_silently_dropped() {
    let directory = data_directory();
    {
        let mut store = PreferenceStore::open(&directory).unwrap();
        store
            .remember_model(
                "openai-api",
                &ModelPreference::new("gpt-5.5", Some(ReasoningEffort::Low)),
            )
            .unwrap();
    }

    Connection::open(directory.join("agens.db"))
        .unwrap()
        .execute(
            "UPDATE model_preference_by_source SET reasoning_effort = 'turbo'",
            [],
        )
        .unwrap();

    assert!(
        PreferenceStore::open(&directory)
            .unwrap()
            .remembered_model("openai-api")
            .is_err()
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn corrupt_database_open_failure_uses_the_preferences_prefix() {
    let directory = data_directory();
    let database = directory.join("agens.db");
    fs::write(&database, "not a sqlite database").unwrap();

    let error = PreferenceStore::open(&directory).err().unwrap().to_string();

    assert!(
        error.starts_with("preferences check database layout"),
        "{error}"
    );
    assert!(!error.contains("permission grants"), "{error}");
    assert!(error.contains(database.to_string_lossy().as_ref()));

    fs::remove_dir_all(directory).unwrap();
}

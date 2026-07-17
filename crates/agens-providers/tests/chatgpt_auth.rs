use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use agens_core::Error;
use agens_providers::{
    ChatGptAuthState, chatgpt_capabilities, load_chatgpt_auth_state, persist_chatgpt_refresh,
};

static TEMP_DIRECTORY_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

#[test]
fn reports_subscription_capability_and_credential_readiness() {
    let directory = temporary_directory("ready");
    let credentials = directory.join("auth.json");
    write_credentials(
        &credentials,
        r#"{
            "openai-chatgpt": {
                "access_token": "synthetic-access",
                "refresh_token": "synthetic-refresh",
                "account_id": "account_123",
                "expires_at": "2026-07-17T13:00:00Z"
            }
        }"#,
    );

    assert!(chatgpt_capabilities().subscription_access);
    assert_eq!(
        load_chatgpt_auth_state(
            &credentials,
            UNIX_EPOCH + Duration::from_secs(1_784_289_600)
        ),
        Ok(ChatGptAuthState::Ready)
    );

    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn classifies_expired_credentials_as_refresh_required() {
    let directory = temporary_directory("expired");
    let credentials = directory.join("auth.json");
    write_credentials(
        &credentials,
        r#"{
            "openai-chatgpt": {
                "access_token": "synthetic-access",
                "refresh_token": "synthetic-refresh",
                "account_id": "account_123",
                "expires_at": "2026-07-17T11:00:00Z"
            }
        }"#,
    );

    assert_eq!(
        load_chatgpt_auth_state(
            &credentials,
            UNIX_EPOCH + Duration::from_secs(1_784_289_600)
        ),
        Ok(ChatGptAuthState::RefreshRequired)
    );

    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn rejects_missing_or_malformed_credentials_without_exposing_tokens() {
    let directory = temporary_directory("invalid");
    let missing_credentials = directory.join("missing.json");
    let malformed_credentials = directory.join("malformed.json");
    let incomplete_credentials = directory.join("incomplete.json");

    write_credentials(&malformed_credentials, "not json");
    write_credentials(
        &incomplete_credentials,
        r#"{"openai-chatgpt":{"access_token":"synthetic-secret"}}"#,
    );

    assert_eq!(
        load_chatgpt_auth_state(&missing_credentials, UNIX_EPOCH),
        Err(Error::Auth(
            "ChatGPT authentication required: credentials file is unavailable".to_owned()
        ))
    );
    assert_eq!(
        load_chatgpt_auth_state(&malformed_credentials, UNIX_EPOCH),
        Err(Error::Auth(
            "ChatGPT authentication required: credentials file is invalid".to_owned()
        ))
    );
    assert_eq!(
        load_chatgpt_auth_state(&incomplete_credentials, UNIX_EPOCH),
        Err(Error::Auth(
            "ChatGPT authentication required: credentials are incomplete".to_owned()
        ))
    );

    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn atomically_persists_a_refresh_without_discarding_other_credentials() {
    let directory = temporary_directory("refresh");
    let credentials = directory.join("auth.json");
    write_credentials(
        &credentials,
        r#"{
            "openai-api": {"api_key": "synthetic-api-key"},
            "openai-chatgpt": {
                "access_token": "synthetic-old-access",
                "refresh_token": "synthetic-old-refresh",
                "account_id": "account_123",
                "expires_at": "2026-07-17T11:00:00Z"
            }
        }"#,
    );

    persist_chatgpt_refresh(
        &credentials,
        "synthetic-new-access",
        Some("synthetic-new-refresh"),
        "2026-07-17T13:00:00Z",
    )
    .expect("refresh should persist atomically");

    let persisted = fs::read_to_string(&credentials).expect("credentials should remain readable");
    assert_eq!(
        persisted,
        r#"{"openai-api":{"api_key":"synthetic-api-key"},"openai-chatgpt":{"access_token":"synthetic-new-access","account_id":"account_123","expires_at":"2026-07-17T13:00:00Z","refresh_token":"synthetic-new-refresh"}}"#
    );
    assert_eq!(
        load_chatgpt_auth_state(
            &credentials,
            UNIX_EPOCH + Duration::from_secs(1_784_289_600)
        ),
        Ok(ChatGptAuthState::Ready)
    );

    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

fn temporary_directory(name: &str) -> PathBuf {
    let sequence = TEMP_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "agens-providers-chatgpt-auth-{name}-{}-{sequence}",
        std::process::id()
    ));

    fs::create_dir_all(&path).expect("temporary directory should be created");
    path
}

fn write_credentials(path: &PathBuf, contents: &str) {
    fs::write(path, contents).expect("synthetic credentials should be written");
}

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use agens_core::{
    Error, HeadlessTurnCancellation, HeadlessTurnPortError, MessagePart, TurnProvider,
};
use agens_providers::ChatGptResponsesProvider;
use serde_json::{Value, json};

static TEMP_DIRECTORY_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
const SECRET_BODY_SENTINEL: &str = "SENTINEL_CHATGPT_REMOTE_BODY";
const SECRET_HEADER_SENTINEL: &str = "SENTINEL_CHATGPT_REMOTE_HEADER";
const REFRESH_WORKER_ENV: &str = "AGENS_CHATGPT_REFRESH_WORKER";
const REFRESH_LOCK_WORKER_ENV: &str = "AGENS_CHATGPT_REFRESH_LOCK_WORKER";
const LOCK_TEST_REPETITIONS: usize = 20;
const LOCK_TEST_WAIT: Duration = Duration::from_secs(1);

#[test]
fn subscription_transport_posts_the_codex_request_and_returns_text() {
    let directory = temporary_directory("transport");
    let credentials = write_credentials(&directory);
    let mut server = LocalServer::start(ServerBehavior::Sse(completed_text_sse("hello")));
    let observed_request = server.take_observed_request();
    let mut provider = provider(&credentials, &server.base_url());

    assert_eq!(
        run(&mut provider, HeadlessTurnCancellation::new()),
        Ok(vec![MessagePart::Text("hello".to_owned())])
    );

    let request = observed_request
        .recv_timeout(Duration::from_secs(1))
        .expect("server should receive the request");
    assert_eq!(request.path, "/backend-api/codex/responses");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer synthetic-access")
    );
    assert_eq!(request.header("chatgpt-account-id"), Some("account_123"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    assert_eq!(request.header("originator"), Some("codex_cli_rs"));
    assert_eq!(request.header("user-agent"), Some("Agens/0.1.0"));
    assert!(
        request
            .header("session-id")
            .is_some_and(|session_id| session_id.starts_with("agens-"))
    );
    assert_eq!(
        request.body,
        json!({
            "model": "test-model",
            "instructions": "test instructions",
            "input": [{
                "role": "user",
                "content": [{"type": "input_text", "text": "test input"}],
            }],
            "tools": [],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "reasoning": {"summary": "auto"},
        })
    );

    server.join();
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_transport_maps_auth_provider_and_semantic_stream_failures_without_secrets() {
    for behavior in [
        ServerBehavior::Status(401),
        ServerBehavior::Status(403),
        ServerBehavior::Status(400),
        ServerBehavior::Status(429),
        ServerBehavior::Status(500),
        ServerBehavior::Sse("data: {\"type\":\"response.failed\"}\n\n".to_owned()),
        ServerBehavior::Sse("data: {\"type\":\"response.incomplete\"}\n\n".to_owned()),
    ] {
        let expected = if matches!(behavior, ServerBehavior::Status(401 | 403)) {
            HeadlessTurnPortError::Authentication
        } else {
            HeadlessTurnPortError::Provider
        };
        let directory = temporary_directory("status");
        let credentials = write_credentials(&directory);
        let server = LocalServer::start(behavior);
        let mut provider = provider(&credentials, &server.base_url());

        let result = run(&mut provider, HeadlessTurnCancellation::new());
        assert_eq!(result, Err(expected));

        let rendered = format!("{result:?}");
        assert!(!rendered.contains(SECRET_BODY_SENTINEL));
        assert!(!rendered.contains(SECRET_HEADER_SENTINEL));
        server.join();
        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }
}

#[test]
fn subscription_transport_keeps_cancellation_and_timeout_distinct() {
    for (behavior, cancellation, expected) in [
        (
            ServerBehavior::WaitForClientClose,
            HeadlessTurnCancellation::new(),
            HeadlessTurnPortError::Cancelled,
        ),
        (
            ServerBehavior::WaitForClientClose,
            HeadlessTurnCancellation::with_deadline(Duration::from_millis(25)),
            HeadlessTurnPortError::TimedOut,
        ),
    ] {
        let directory = temporary_directory("stop");
        let credentials = write_credentials(&directory);
        let mut server = LocalServer::start(behavior);
        let observed_request = server.take_observed_request();
        let mut provider = provider(&credentials, &server.base_url());
        let canceller = cancellation.clone();
        let cancellation_thread = thread::spawn(move || {
            observed_request
                .recv_timeout(Duration::from_secs(1))
                .expect("server should observe the request before cancellation");
            if expected == HeadlessTurnPortError::Cancelled {
                canceller.cancel();
            }
        });

        assert_eq!(run(&mut provider, cancellation), Err(expected));

        cancellation_thread
            .join()
            .expect("cancellation thread should finish");
        server.join();
        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }
}

#[test]
fn subscription_constructor_rejects_incomplete_existing_credentials_without_an_api_key() {
    let directory = temporary_directory("credentials");
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"synthetic-access"}}"#,
    )
    .expect("credentials should be written");

    assert!(matches!(
        ChatGptResponsesProvider::from_credentials_with_timeout(
            &credentials,
            None,
            "test-model".to_owned(),
            "test instructions".to_owned(),
            "test input".to_owned(),
            Duration::from_secs(1),
        ),
        Err(Error::Auth(error)) if error ==
            "ChatGPT authentication required: credentials are incomplete"
    ));

    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_transport_proactively_refreshes_before_the_responses_request() {
    let directory = temporary_directory("proactive-refresh");
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE3ODQyODg4MDB9.signature","refresh_token":"synthetic-old-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("credentials should be written");
    let mut responses = LocalServer::start(ServerBehavior::Sse(completed_text_sse("refreshed")));
    let response_request = responses.take_observed_request();
    let oauth = OAuthServer::start(
        200,
        r#"{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature","refresh_token":"synthetic-rotated-refresh","id_token":"ignored"}"#,
    );
    let mut provider = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
        &credentials,
        Some(&responses.base_url()),
        Some(&oauth.url()),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured");

    assert_eq!(
        run(&mut provider, HeadlessTurnCancellation::new()),
        Ok(vec![MessagePart::Text("refreshed".to_owned())])
    );

    let oauth_request = oauth.join();
    assert_eq!(oauth_request.path, "/oauth/token");
    assert_eq!(
        oauth_request.body,
        json!({
            "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
            "grant_type": "refresh_token",
            "refresh_token": "synthetic-old-refresh",
        })
    );
    assert_eq!(
        response_request
            .recv_timeout(Duration::from_secs(1))
            .expect("responses request should be observed")
            .header("authorization"),
        Some("Bearer header.eyJleHAiOjE4OTM0NTYwMDB9.signature")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(
            &fs::read(&credentials).expect("credentials should persist")
        )
        .expect("credentials should remain JSON")["openai-chatgpt"]["account_id"],
        "account_123"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(
            &fs::read(&credentials).expect("credentials should persist")
        )
        .expect("credentials should remain JSON")["openai-chatgpt"]["refresh_token"],
        "synthetic-rotated-refresh"
    );

    responses.join();
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_transport_refreshes_once_after_401_then_retries_responses_once() {
    let directory = temporary_directory("401-refresh");
    let credentials = write_credentials(&directory);
    let server = ScriptedServer::start(vec![
        ScriptedResponse::Status(401),
        ScriptedResponse::Json(
            200,
            r#"{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature"}"#.to_owned(),
        ),
        ScriptedResponse::Sse(completed_text_sse("recovered")),
    ]);
    let mut provider = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
        &credentials,
        Some(&server.responses_base_url()),
        Some(&server.oauth_url()),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured");

    assert_eq!(
        run(&mut provider, HeadlessTurnCancellation::new()),
        Ok(vec![MessagePart::Text("recovered".to_owned())])
    );

    let requests = server.join();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].path, "/backend-api/codex/responses");
    assert_eq!(requests[1].path, "/oauth/token");
    assert_eq!(requests[2].path, "/backend-api/codex/responses");
    assert_eq!(requests[1].body["refresh_token"], "synthetic-refresh");
    assert_eq!(
        requests[2].header("authorization"),
        Some("Bearer header.eyJleHAiOjE4OTM0NTYwMDB9.signature")
    );
    let persisted = serde_json::from_slice::<Value>(
        &fs::read(&credentials).expect("credentials should persist"),
    )
    .expect("credentials should remain JSON");
    assert_eq!(
        persisted["openai-chatgpt"]["refresh_token"],
        "synthetic-refresh"
    );
    assert_eq!(persisted["openai-chatgpt"]["account_id"], "account_123");

    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_transport_reloads_a_rotated_access_token_after_401_without_refreshing() {
    let directory = temporary_directory("401-reload");
    let credentials = write_credentials(&directory);
    let server = ScriptedServer::start(vec![
        ScriptedResponse::Status(401),
        ScriptedResponse::Sse(completed_text_sse("reloaded")),
    ]);
    let mut provider = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
        &credentials,
        Some(&server.responses_base_url()),
        Some(&server.oauth_url()),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("rotated credentials should be written");

    assert_eq!(
        run(&mut provider, HeadlessTurnCancellation::new()),
        Ok(vec![MessagePart::Text("reloaded".to_owned())])
    );

    let requests = server.join();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.path == "/backend-api/codex/responses")
    );
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer header.eyJleHAiOjE4OTM0NTYwMDB9.signature")
    );
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_transport_stops_after_a_second_401() {
    let directory = temporary_directory("second-401");
    let credentials = write_credentials(&directory);
    let server = ScriptedServer::start(vec![
        ScriptedResponse::Status(401),
        ScriptedResponse::Json(
            200,
            r#"{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature"}"#.to_owned(),
        ),
        ScriptedResponse::Status(401),
    ]);
    let mut provider = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
        &credentials,
        Some(&server.responses_base_url()),
        Some(&server.oauth_url()),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured");

    assert_eq!(
        run(&mut provider, HeadlessTurnCancellation::new()),
        Err(HeadlessTurnPortError::Authentication)
    );
    assert_eq!(server.join().len(), 3);
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_transport_classifies_refresh_failures_without_retrying_responses() {
    for (status, body, expected) in [
        (
            400,
            r#"{"error":"invalid_grant"}"#,
            HeadlessTurnPortError::Authentication,
        ),
        (
            400,
            r#"{"error":{"code":"refresh_token_expired"}}"#,
            HeadlessTurnPortError::Authentication,
        ),
        (
            400,
            r#"{"error":"refresh_token_reused"}"#,
            HeadlessTurnPortError::Authentication,
        ),
        (
            400,
            r#"{"error":"refresh_token_invalidated"}"#,
            HeadlessTurnPortError::Authentication,
        ),
        (
            500,
            r#"{"error":{"code":"temporary_failure"}}"#,
            HeadlessTurnPortError::Provider,
        ),
        (
            200,
            r#"{"refresh_token":"rotated-only"}"#,
            HeadlessTurnPortError::Authentication,
        ),
    ] {
        let directory = temporary_directory("refresh-failure");
        let credentials = directory.join("auth.json");
        fs::write(
            &credentials,
            r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE3ODQyODg4MDB9.signature","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
        )
        .expect("credentials should be written");
        let server = ScriptedServer::start(vec![ScriptedResponse::Json(status, body.to_owned())]);
        let mut provider = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
            &credentials,
            Some(&server.responses_base_url()),
            Some(&server.oauth_url()),
            "test-model".to_owned(),
            "test instructions".to_owned(),
            "test input".to_owned(),
            Duration::from_secs(1),
        )
        .expect("provider should be configured");

        assert_eq!(
            run(&mut provider, HeadlessTurnCancellation::new()),
            Err(expected)
        );
        assert_eq!(server.join().len(), 1);
        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }
}

#[test]
fn subscription_transport_maps_non_json_refresh_service_failures_to_provider() {
    let directory = temporary_directory("refresh-non-json");
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE3ODQyODg4MDB9.signature","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("credentials should be written");
    let server = ScriptedServer::start(vec![ScriptedResponse::Raw(
        503,
        "upstream temporarily unavailable".to_owned(),
    )]);
    let mut provider = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
        &credentials,
        Some(&server.responses_base_url()),
        Some(&server.oauth_url()),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured");

    assert_eq!(
        run(&mut provider, HeadlessTurnCancellation::new()),
        Err(HeadlessTurnPortError::Provider)
    );
    assert_eq!(server.join().len(), 1);
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn concurrent_subscription_providers_coalesce_one_proactive_refresh() {
    let directory = temporary_directory("concurrent-refresh");
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE3ODQyODg4MDB9.signature","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("credentials should be written");
    let server = ScriptedServer::start(vec![
        ScriptedResponse::Json(
            200,
            r#"{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature"}"#.to_owned(),
        ),
        ScriptedResponse::Sse(completed_text_sse("one")),
        ScriptedResponse::Sse(completed_text_sse("two")),
    ]);
    let barrier = std::sync::Arc::new(Barrier::new(3));
    let handles = (0..2)
        .map(|_| {
            let credentials = credentials.clone();
            let responses_base_url = server.responses_base_url();
            let oauth_url = server.oauth_url();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let mut provider =
                    ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
                        &credentials,
                        Some(&responses_base_url),
                        Some(&oauth_url),
                        "test-model".to_owned(),
                        "test instructions".to_owned(),
                        "test input".to_owned(),
                        Duration::from_secs(1),
                    )
                    .expect("provider should be configured");
                barrier.wait();
                run(&mut provider, HeadlessTurnCancellation::new())
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();

    for handle in handles {
        let result = handle.join().expect("provider thread should finish");
        assert!(matches!(result.as_deref(), Ok([MessagePart::Text(_)])));
    }

    let requests = server.join();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == "/oauth/token")
            .count(),
        1
    );
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[cfg(unix)]
#[test]
fn subscription_refresh_uses_one_oauth_request_across_processes_and_path_aliases() {
    let directory = temporary_directory("process-refresh");
    let credentials = directory.join("auth.json");
    let alias_directory = directory.join("credential-alias");
    let alias = alias_directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE3ODQyODg4MDB9.signature","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("credentials should be written");
    std::os::unix::fs::symlink(&directory, &alias_directory)
        .expect("credential directory alias should be created");
    let server = ScriptedServer::start(vec![
        ScriptedResponse::Json(
            200,
            r#"{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature"}"#.to_owned(),
        ),
        ScriptedResponse::Sse(completed_text_sse("one")),
        ScriptedResponse::Sse(completed_text_sse("two")),
    ]);
    let executable = std::env::current_exe().expect("test executable should be available");
    let responses_base_url = server.responses_base_url();
    let oauth_url = server.oauth_url();
    let children = [&credentials, &alias]
        .into_iter()
        .map(|path| {
            let mut command = Command::new(&executable);
            command
                .args(["--exact", "refresh_subprocess_worker", "--nocapture"])
                .env(REFRESH_WORKER_ENV, "1")
                .env("AGENS_CHATGPT_REFRESH_PATH", path)
                .env("AGENS_CHATGPT_RESPONSES_URL", &responses_base_url)
                .env("AGENS_CHATGPT_OAUTH_URL", &oauth_url);
            command.spawn().expect("refresh worker should start")
        })
        .collect::<Vec<_>>();

    for mut child in children {
        assert!(child.wait().expect("refresh worker should exit").success());
    }

    let requests = server.join();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == "/oauth/token")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == "/backend-api/codex/responses")
            .count(),
        2
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&directory)
            .expect("credential directory metadata should be readable")
            .permissions()
            .mode()
            & 0o077,
        0
    );
    assert_eq!(
        fs::metadata(&credentials)
            .expect("credential file metadata should be readable")
            .permissions()
            .mode()
            & 0o077,
        0
    );
    assert_eq!(
        fs::metadata(directory.join(".auth.json.refresh.lock"))
            .expect("refresh lock metadata should be readable")
            .permissions()
            .mode()
            & 0o077,
        0
    );
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn refresh_subprocess_worker() {
    if std::env::var_os(REFRESH_WORKER_ENV).is_none() {
        return;
    }

    let credentials = PathBuf::from(
        std::env::var_os("AGENS_CHATGPT_REFRESH_PATH")
            .expect("refresh worker requires a credentials path"),
    );
    let responses_base_url = std::env::var("AGENS_CHATGPT_RESPONSES_URL")
        .expect("refresh worker requires a Responses URL");
    let oauth_url =
        std::env::var("AGENS_CHATGPT_OAUTH_URL").expect("refresh worker requires an OAuth URL");
    let mut provider = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
        &credentials,
        Some(&responses_base_url),
        Some(&oauth_url),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(2),
    )
    .expect("refresh worker provider should be configured");

    assert!(matches!(
        run(&mut provider, HeadlessTurnCancellation::new()).as_deref(),
        Ok([MessagePart::Text(_)])
    ));
}

#[test]
fn refresh_lock_subprocess_worker() {
    let Some(mode) = std::env::var_os(REFRESH_LOCK_WORKER_ENV) else {
        return;
    };
    let credentials = PathBuf::from(
        std::env::var_os("AGENS_CHATGPT_REFRESH_PATH")
            .expect("refresh lock worker requires a credentials path"),
    );
    let responses_url = std::env::var("AGENS_CHATGPT_RESPONSES_URL")
        .expect("refresh lock worker requires a Responses URL");
    let oauth_url = std::env::var("AGENS_CHATGPT_OAUTH_URL")
        .expect("refresh lock worker requires an OAuth URL");
    let cancellation = HeadlessTurnCancellation::new();
    let mut provider = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
        &credentials,
        Some(&responses_url),
        Some(&oauth_url),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(2),
    )
    .expect("refresh lock worker provider should be configured");

    match mode.to_string_lossy().as_ref() {
        "holder" => {
            let release = PathBuf::from(
                std::env::var_os("AGENS_CHATGPT_HOLDER_RELEASE")
                    .expect("holder requires a release marker"),
            );
            let canceller = cancellation.clone();
            let watcher = thread::spawn(move || {
                wait_for_file(&release, "holder release marker should arrive");
                canceller.cancel();
            });

            assert_eq!(
                run(&mut provider, cancellation),
                Err(HeadlessTurnPortError::Cancelled)
            );
            watcher
                .join()
                .expect("holder release watcher should finish");
        }
        "crash-holder" => {
            let _ = run(&mut provider, cancellation);
            panic!("crash holder should be terminated by the parent test");
        }
        "cancelled-caller" => {
            let started = PathBuf::from(
                std::env::var_os("AGENS_CHATGPT_CALLER_STARTED")
                    .expect("cancelled caller requires a start marker"),
            );
            let cancel = PathBuf::from(
                std::env::var_os("AGENS_CHATGPT_CALLER_CANCEL")
                    .expect("cancelled caller requires a cancellation marker"),
            );
            let cancelled = PathBuf::from(
                std::env::var_os("AGENS_CHATGPT_CALLER_CANCELLED")
                    .expect("cancelled caller requires a result marker"),
            );
            fs::write(&started, b"started").expect("caller start marker should be written");
            let canceller = cancellation.clone();
            let watcher = thread::spawn(move || {
                wait_for_file(&cancel, "caller cancellation marker should arrive");
                canceller.cancel();
            });

            assert_eq!(
                run(&mut provider, cancellation),
                Err(HeadlessTurnPortError::Cancelled)
            );
            watcher
                .join()
                .expect("caller cancellation watcher should finish");
            fs::write(cancelled, b"cancelled")
                .expect("caller cancellation marker should be written");
        }
        "recovery" => {
            assert_eq!(
                run(&mut provider, cancellation),
                Ok(vec![MessagePart::Text("recovered".to_owned())])
            );
        }
        _ => panic!("unknown refresh lock worker mode"),
    }
}

#[test]
fn subscription_transport_returns_cancelled_while_waiting_for_refresh() {
    let directory = temporary_directory("refresh-cancel");
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE3ODQyODg4MDB9.signature","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("credentials should be written");
    let mut oauth = LocalServer::start(ServerBehavior::WaitForClientClose);
    let request = oauth.take_observed_request();
    let oauth_url = format!("http://{}/oauth/token", oauth.address);
    let mut provider = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
        &credentials,
        Some("http://127.0.0.1:1/backend-api/codex"),
        Some(&oauth_url),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured");
    let cancellation = HeadlessTurnCancellation::new();
    let canceller = cancellation.clone();
    let thread = thread::spawn(move || {
        request
            .recv_timeout(Duration::from_secs(1))
            .expect("refresh request should be observed");
        canceller.cancel();
    });

    assert_eq!(
        run(&mut provider, cancellation),
        Err(HeadlessTurnPortError::Cancelled)
    );
    thread.join().expect("cancellation thread should finish");
    oauth.join();
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_transport_times_out_while_waiting_for_refresh() {
    let directory = temporary_directory("refresh-timeout");
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE3ODQyODg4MDB9.signature","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("credentials should be written");
    let mut oauth = LocalServer::start(ServerBehavior::WaitForClientClose);
    let request = oauth.take_observed_request();
    let oauth_url = format!("http://{}/oauth/token", oauth.address);
    let mut provider = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
        &credentials,
        Some("http://127.0.0.1:1/backend-api/codex"),
        Some(&oauth_url),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured");
    let cancellation = HeadlessTurnCancellation::with_deadline(Duration::from_millis(25));

    assert_eq!(
        run(&mut provider, cancellation),
        Err(HeadlessTurnPortError::TimedOut)
    );
    request
        .recv_timeout(Duration::from_secs(1))
        .expect("refresh request should be observed");
    oauth.join();
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_transport_times_out_while_waiting_for_an_existing_refresh_lock() {
    let directory = temporary_directory("refresh-lock-timeout");
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE3ODQyODg4MDB9.signature","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("credentials should be written");
    let mut oauth = LocalServer::start(ServerBehavior::WaitForClientClose);
    let refresh_started = oauth.take_observed_request();
    let oauth_url = format!("http://{}/oauth/token", oauth.address);
    let first_credentials = credentials.clone();
    let first_oauth_url = oauth_url.clone();
    let first = thread::spawn(move || {
        let mut provider = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
            &first_credentials,
            Some("http://127.0.0.1:1/backend-api/codex"),
            Some(&first_oauth_url),
            "test-model".to_owned(),
            "test instructions".to_owned(),
            "test input".to_owned(),
            Duration::from_secs(1),
        )
        .expect("first provider should be configured");
        run(
            &mut provider,
            HeadlessTurnCancellation::with_deadline(Duration::from_millis(250)),
        )
    });
    refresh_started
        .recv_timeout(Duration::from_secs(1))
        .expect("first refresh request should hold the lock");
    let mut second = ChatGptResponsesProvider::from_credentials_with_timeout_and_auth_url(
        &credentials,
        Some("http://127.0.0.1:1/backend-api/codex"),
        Some(&oauth_url),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(1),
    )
    .expect("second provider should be configured");

    assert_eq!(
        run(
            &mut second,
            HeadlessTurnCancellation::with_deadline(Duration::from_millis(25)),
        ),
        Err(HeadlessTurnPortError::TimedOut)
    );
    assert_eq!(
        first.join().expect("first provider should finish"),
        Err(HeadlessTurnPortError::TimedOut)
    );
    oauth.join();
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[cfg(unix)]
#[test]
fn subscription_refresh_lock_wait_cancellation_preserves_holder_and_credentials() {
    for iteration in 0..LOCK_TEST_REPETITIONS {
        let directory = temporary_directory(&format!("refresh-lock-cancel-{iteration}"));
        let credentials = write_expired_credentials(&directory);
        let holder_release = directory.join("holder-release");
        let caller_started = directory.join("caller-started");
        let caller_cancelled = directory.join("caller-cancelled");
        let oauth = ControlledOAuthServer::start();

        let mut holder = spawn_refresh_lock_worker(
            "holder",
            &credentials,
            "http://127.0.0.1:1/backend-api/codex",
            &oauth.url(),
            &[("AGENS_CHATGPT_HOLDER_RELEASE", &holder_release)],
        );
        let first_request = oauth
            .requests()
            .recv_timeout(LOCK_TEST_WAIT)
            .expect("holder should signal that it acquired the refresh lock");
        assert_eq!(first_request.path, "/oauth/token");
        let credentials_before =
            fs::read(&credentials).expect("credentials should remain readable");

        let mut caller = spawn_refresh_lock_worker(
            "cancelled-caller",
            &credentials,
            "http://127.0.0.1:1/backend-api/codex",
            &oauth.url(),
            &[
                ("AGENS_CHATGPT_CALLER_STARTED", &caller_started),
                (
                    "AGENS_CHATGPT_CALLER_CANCEL",
                    &directory.join("caller-cancel"),
                ),
                ("AGENS_CHATGPT_CALLER_CANCELLED", &caller_cancelled),
            ],
        );
        wait_for_file(
            &caller_started,
            "caller should start after the holder signal",
        );
        thread::sleep(Duration::from_millis(25));
        fs::write(directory.join("caller-cancel"), b"cancel")
            .expect("caller cancellation marker should be written");

        wait_for_success(&mut caller, "cancelled lock waiter should exit promptly");
        assert!(caller_cancelled.exists());
        assert_eq!(
            fs::read(&credentials).expect("credentials should remain readable"),
            credentials_before
        );
        assert_eq!(oauth.request_count(), 1);
        assert!(
            holder
                .try_wait()
                .expect("holder status should be readable")
                .is_none()
        );

        fs::write(&holder_release, b"release").expect("holder release marker should be written");
        wait_for_success(
            &mut holder,
            "holder should release normally after cancellation test",
        );
        oauth.join();
        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }
}

#[cfg(unix)]
#[test]
fn subscription_refresh_recovers_after_lock_holder_crash_without_stale_sidecar_deadlock() {
    for iteration in 0..LOCK_TEST_REPETITIONS {
        let directory = temporary_directory(&format!("refresh-lock-crash-{iteration}"));
        let credentials = write_expired_credentials(&directory);
        let oauth = ControlledOAuthServer::start();
        let mut responses =
            LocalServer::start(ServerBehavior::Sse(completed_text_sse("recovered")));
        let response_request = responses.take_observed_request();
        let mut holder = spawn_refresh_lock_worker(
            "crash-holder",
            &credentials,
            "http://127.0.0.1:1/backend-api/codex",
            &oauth.url(),
            &[],
        );

        oauth
            .requests()
            .recv_timeout(LOCK_TEST_WAIT)
            .expect("holder should acquire the refresh lock before it crashes");
        assert!(directory.join(".auth.json.refresh.lock").exists());
        holder.kill().expect("holder should be force-killed");
        holder.wait().expect("force-killed holder should be reaped");

        let mut recovery = spawn_refresh_lock_worker(
            "recovery",
            &credentials,
            &responses.base_url(),
            &oauth.url(),
            &[],
        );
        wait_for_success(
            &mut recovery,
            "subsequent process should reacquire the OS lock",
        );
        assert_eq!(oauth.request_count(), 2);
        assert_eq!(
            response_request
                .recv_timeout(LOCK_TEST_WAIT)
                .expect("recovery should issue one Responses request")
                .header("authorization"),
            Some("Bearer header.eyJleHAiOjE4OTM0NTYwMDB9.signature")
        );
        let persisted = serde_json::from_slice::<Value>(
            &fs::read(&credentials).expect("credentials should persist after recovery"),
        )
        .expect("recovered credentials should remain JSON");
        assert_eq!(
            persisted["openai-chatgpt"]["access_token"],
            "header.eyJleHAiOjE4OTM0NTYwMDB9.signature"
        );
        assert!(directory.join(".auth.json.refresh.lock").exists());

        oauth.join();
        responses.join();
        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }
}

#[test]
fn subscription_tool_replay_replays_reasoning_calls_outputs_and_tools_without_response_id() {
    let directory = temporary_directory("tool-replay");
    let credentials = write_credentials(&directory);
    let tool = agens_providers::OpenAiFunctionTool::new(
        "weather",
        "Looks up weather",
        json!({"type":"object","properties":{"city":{"type":"string"}}}),
    )
    .expect("tool should be valid");
    let server = ScriptedServer::start(vec![
        ScriptedResponse::Sse(tool_call_sse(
            "item_call_1",
            "call_1",
            "weather",
            r#"{"city":"Paris"}"#,
        )),
        ScriptedResponse::Sse(completed_text_sse("done")),
    ]);
    let mut provider =
        ChatGptResponsesProvider::from_credentials_with_tools_and_timeout_and_auth_url(
            &credentials,
            Some(&server.responses_base_url()),
            Some(&server.oauth_url()),
            "test-model".to_owned(),
            "test instructions".to_owned(),
            "test input".to_owned(),
            vec![tool],
            Duration::from_secs(1),
        )
        .expect("provider should be configured");

    assert_eq!(
        run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()),
        Ok(vec![
            MessagePart::Reasoning("checking weather".to_owned()),
            MessagePart::ToolCall {
                id: "call_1".to_owned(),
                name: "weather".to_owned(),
                input: r#"{"city":"Paris"}"#.to_owned(),
            },
        ])
    );
    assert_eq!(
        run_with_events(
            &mut provider,
            &[agens_core::TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "call_1".to_owned(),
                content: "sunny".to_owned(),
                is_error: false,
            })],
            HeadlessTurnCancellation::new(),
        ),
        Ok(vec![MessagePart::Text("done".to_owned())])
    );

    let requests = server.join();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["store"], false);
    assert!(requests[0].body.get("previous_response_id").is_none());
    assert_eq!(requests[1].body["store"], false);
    assert!(requests[1].body.get("previous_response_id").is_none());
    assert_eq!(requests[1].body["tools"], requests[0].body["tools"]);
    assert_eq!(
        requests[1].body["input"],
        json!([
            {"role":"user","content":[{"type":"input_text","text":"test input"}]},
            {
                "type":"reasoning",
                "id":"item_reasoning_1",
                "summary":[{"type":"summary_text","text":"checking weather"}],
                "encrypted_content":"encrypted-reasoning-1"
            },
            {
                "type":"function_call",
                "id":"item_call_1",
                "call_id":"call_1",
                "name":"weather",
                "arguments":"{\"city\":\"Paris\"}"
            },
            {"type":"function_call_output","call_id":"call_1","output":"sunny"}
        ])
    );
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_tool_replay_rejects_duplicate_outputs_before_another_http_request() {
    let directory = temporary_directory("tool-replay-duplicate");
    let credentials = write_credentials(&directory);
    let server = ScriptedServer::start(vec![ScriptedResponse::Sse(tool_call_sse(
        "item_call_1",
        "call_1",
        "weather",
        r#"{"city":"Paris"}"#,
    ))]);
    let mut provider =
        ChatGptResponsesProvider::from_credentials_with_tools_and_timeout_and_auth_url(
            &credentials,
            Some(&server.responses_base_url()),
            Some(&server.oauth_url()),
            "test-model".to_owned(),
            "test instructions".to_owned(),
            "test input".to_owned(),
            Vec::new(),
            Duration::from_secs(1),
        )
        .expect("provider should be configured");
    let result = MessagePart::ToolResult {
        tool_call_id: "call_1".to_owned(),
        content: "sunny".to_owned(),
        is_error: false,
    };

    assert!(matches!(
        run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()),
        Ok(parts) if parts.iter().any(|part| matches!(part, MessagePart::ToolCall { .. }))
    ));
    assert_eq!(
        run_with_events(
            &mut provider,
            &[
                agens_core::TurnEvent::ToolResult(result.clone()),
                agens_core::TurnEvent::ToolResult(result),
            ],
            HeadlessTurnCancellation::new(),
        ),
        Err(HeadlessTurnPortError::Provider)
    );
    assert_eq!(
        run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()),
        Err(HeadlessTurnPortError::Provider)
    );
    assert_eq!(server.join().len(), 1);
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_tool_replay_keeps_completed_message_reasoning_and_calls_in_three_round_order() {
    let directory = temporary_directory("tool-replay-three-rounds");
    let credentials = write_credentials(&directory);
    let server = ScriptedServer::start(vec![
        ScriptedResponse::Sse(tool_round_sse(
            &[
                json!({
                    "type": "message",
                    "id": "item_message_1",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "I will check both cities."}],
                }),
                json!({
                    "type": "reasoning",
                    "id": "item_reasoning_1",
                    "summary": [{"type": "summary_text", "text": "checking cities"}],
                    "encrypted_content": "encrypted-reasoning-1",
                }),
            ],
            &[
                ("item_call_1", "call_1", "weather", r#"{"city":"Paris"}"#),
                ("item_call_2", "call_2", "weather", r#"{"city":"Rome"}"#),
            ],
        )),
        ScriptedResponse::Sse(tool_round_sse(
            &[json!({
                "type": "message",
                "id": "item_message_2",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "One more check."}],
            })],
            &[("item_call_3", "call_3", "weather", r#"{"city":"Berlin"}"#)],
        )),
        ScriptedResponse::Sse(completed_text_sse("done")),
    ]);
    let mut provider = subscription_provider(&credentials, &server);
    let cancellation = HeadlessTurnCancellation::new();

    assert!(matches!(
        run_with_events(&mut provider, &[], cancellation.clone()),
        Ok(parts) if parts.iter().filter(|part| matches!(part, MessagePart::ToolCall { .. })).count() == 2
    ));
    assert!(matches!(
        run_with_events(
            &mut provider,
            &[
                tool_result("call_2", "Rome is sunny", false),
                tool_result("call_1", "Paris is rainy", false),
            ],
            cancellation.clone(),
        ),
        Ok(parts) if parts.iter().any(|part| matches!(part, MessagePart::ToolCall { id, .. } if id == "call_3"))
    ));
    assert_eq!(
        run_with_events(
            &mut provider,
            &[
                tool_result("call_2", "Rome is sunny", false),
                tool_result("call_1", "Paris is rainy", false),
                tool_result("call_3", "Berlin is cloudy", false),
            ],
            cancellation,
        ),
        Ok(vec![MessagePart::Text("done".to_owned())])
    );

    let requests = server.join();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].body["input"],
        json!([
            {"role":"user","content":[{"type":"input_text","text":"test input"}]},
            {"type":"message","id":"item_message_1","role":"assistant","content":[{"type":"output_text","text":"I will check both cities."}]},
            {"type":"reasoning","id":"item_reasoning_1","summary":[{"type":"summary_text","text":"checking cities"}],"encrypted_content":"encrypted-reasoning-1"},
            {"type":"function_call","id":"item_call_1","call_id":"call_1","name":"weather","arguments":"{\"city\":\"Paris\"}"},
            {"type":"function_call","id":"item_call_2","call_id":"call_2","name":"weather","arguments":"{\"city\":\"Rome\"}"},
            {"type":"function_call_output","call_id":"call_1","output":"Paris is rainy"},
            {"type":"function_call_output","call_id":"call_2","output":"Rome is sunny"},
        ])
    );
    assert_eq!(requests[2].body["input"][7]["id"], "item_message_2");
    assert_eq!(requests[2].body["input"][8]["call_id"], "call_3");
    assert_eq!(
        requests[2].body["input"][9],
        json!({"type":"function_call_output","call_id":"call_3","output":"Berlin is cloudy"})
    );
    assert!(
        requests
            .iter()
            .all(|request| request.body["store"] == false)
    );
    assert!(
        requests
            .iter()
            .all(|request| request.body.get("previous_response_id").is_none())
    );
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_tool_replay_replaces_added_function_call_with_its_completed_wire_item_once() {
    let directory = temporary_directory("completed-function-call");
    let credentials = write_credentials(&directory);
    let completed_call = json!({
        "type":"function_call",
        "id":"item_call_1",
        "call_id":"call_1",
        "name":"weather",
        "arguments":"{\"city\":\"Paris\"}",
        "status":"completed",
    });
    let server = ScriptedServer::start(vec![
        ScriptedResponse::Sse(format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {{\"type\":\"response.completed\"}}\n\n",
            json!({"type":"response.output_item.added","item":{"type":"function_call","id":"item_call_1","call_id":"call_1","name":"weather","arguments":""}}),
            json!({"type":"response.function_call_arguments.done","item_id":"item_call_1","arguments":"{\"city\":\"Paris\"}"}),
            json!({"type":"response.output_item.done","item":completed_call}),
        )),
        ScriptedResponse::Sse(completed_text_sse("done")),
    ]);
    let mut provider = subscription_provider(&credentials, &server);

    assert!(run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()).is_ok());
    assert!(
        run_with_events(
            &mut provider,
            &[tool_result("call_1", "sunny", false)],
            HeadlessTurnCancellation::new(),
        )
        .is_ok()
    );

    let requests = server.join();
    let input = requests[1].body["input"]
        .as_array()
        .expect("input should be an array");
    assert_eq!(
        input
            .iter()
            .filter(|item| item["id"] == "item_call_1")
            .count(),
        1
    );
    assert_eq!(input[1]["status"], "completed");
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_tool_replay_rejects_missing_foreign_duplicate_and_truncated_results_before_http() {
    for (name, initial_events, continuation_events) in [
        ("missing", Vec::new(), Vec::new()),
        (
            "foreign",
            Vec::new(),
            vec![tool_result("foreign", "no", false)],
        ),
        (
            "duplicate",
            Vec::new(),
            vec![
                tool_result("call_1", "first", false),
                tool_result("call_1", "second", false),
            ],
        ),
        (
            "truncated",
            vec![tool_result("earlier", "ignored", false)],
            Vec::new(),
        ),
    ] {
        let directory = temporary_directory(name);
        let credentials = write_credentials(&directory);
        let server = ScriptedServer::start(vec![ScriptedResponse::Sse(tool_call_sse(
            "item_call_1",
            "call_1",
            "weather",
            r#"{"city":"Paris"}"#,
        ))]);
        let mut provider = subscription_provider(&credentials, &server);

        assert!(
            run_with_events(
                &mut provider,
                &initial_events,
                HeadlessTurnCancellation::new(),
            )
            .is_ok()
        );
        assert_eq!(
            run_with_events(
                &mut provider,
                &continuation_events,
                HeadlessTurnCancellation::new(),
            ),
            Err(HeadlessTurnPortError::Provider),
        );
        assert_eq!(
            run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()),
            Err(HeadlessTurnPortError::Provider),
        );
        assert_eq!(
            server.join().len(),
            1,
            "{name} must not issue a continuation"
        );
        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }
}

#[test]
fn subscription_tool_replay_rejects_replayed_or_malformed_wire_items_without_retrying() {
    for (name, responses, expected_requests) in [
        (
            "replayed-id",
            vec![
                ScriptedResponse::Sse(tool_round_sse(
                    &[
                        json!({"type":"message","id":"item_message","role":"assistant","content":[]}),
                    ],
                    &[("item_call_1", "call_1", "weather", "{}")],
                )),
                ScriptedResponse::Sse(tool_round_sse(
                    &[
                        json!({"type":"message","id":"item_message","role":"assistant","content":[]}),
                    ],
                    &[("item_call_2", "call_2", "weather", "{}")],
                )),
            ],
            2,
        ),
        (
            "missing-encrypted-reasoning",
            vec![ScriptedResponse::Sse(output_item_sse(json!({
                "type":"reasoning",
                "id":"item_reasoning",
                "summary":[],
            })))],
            1,
        ),
        (
            "unsupported-item",
            vec![ScriptedResponse::Sse(output_item_sse(json!({
                "type":"computer_call",
                "id":"item_computer",
            })))],
            1,
        ),
    ] {
        let directory = temporary_directory(name);
        let credentials = write_credentials(&directory);
        let server = ScriptedServer::start(responses);
        let mut provider = subscription_provider(&credentials, &server);

        if name == "replayed-id" {
            assert!(run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()).is_ok());
            assert_eq!(
                run_with_events(
                    &mut provider,
                    &[tool_result("call_1", "first", false)],
                    HeadlessTurnCancellation::new(),
                ),
                Err(HeadlessTurnPortError::Provider),
            );
        } else {
            assert_eq!(
                run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()),
                Err(HeadlessTurnPortError::Provider),
            );
        }
        assert_eq!(
            run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()),
            Err(HeadlessTurnPortError::Provider),
        );
        assert_eq!(
            server.join().len(),
            expected_requests,
            "{name} request count"
        );
        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }
}

#[test]
fn subscription_tool_replay_sanitizes_error_outputs_and_rejects_item_history_and_round_bounds_before_http()
 {
    let directory = temporary_directory("error-output");
    let credentials = write_credentials(&directory);
    let server = ScriptedServer::start(vec![
        ScriptedResponse::Sse(tool_call_sse("item_call_1", "call_1", "weather", "{}")),
        ScriptedResponse::Sse(completed_text_sse("done")),
    ]);
    let mut provider = subscription_provider(&credentials, &server);
    assert!(run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()).is_ok());
    assert!(
        run_with_events(
            &mut provider,
            &[tool_result("call_1", "secret=must-not-leak", true)],
            HeadlessTurnCancellation::new(),
        )
        .is_ok()
    );
    let requests = server.join();
    assert_eq!(
        requests[1].body["input"][3]["output"],
        "Tool execution failed"
    );
    assert!(
        !requests[1]
            .body
            .to_string()
            .contains("secret=must-not-leak")
    );
    fs::remove_dir_all(directory).expect("temporary directory should be removed");

    let oversized_items = (0..=512)
        .map(|index| json!({"type":"message","id":format!("item_{index}"),"role":"assistant","content":[]}))
        .collect::<Vec<_>>();
    assert_replay_response_rejection(
        "item-bound",
        tool_round_sse(&oversized_items, &[("item_call", "call", "weather", "{}")]),
    );

    let large_content = "x".repeat(65_000);
    let history_items = (0..65)
        .map(|index| {
            json!({
                "type":"message",
                "id":format!("history_{index}"),
                "role":"assistant",
                "content":[{"type":"output_text","text":large_content}],
            })
        })
        .collect::<Vec<_>>();
    assert_replay_response_rejection(
        "history-bound",
        tool_round_sse(&history_items, &[("item_call", "call", "weather", "{}")]),
    );

    let directory = temporary_directory("round-bound");
    let credentials = write_credentials(&directory);
    let responses = (0..128)
        .map(|index| {
            ScriptedResponse::Sse(tool_round_sse(
                &[],
                &[(
                    &format!("item_call_{index}"),
                    &format!("call_{index}"),
                    "weather",
                    "{}",
                )],
            ))
        })
        .collect::<Vec<_>>();
    let server = ScriptedServer::start(responses);
    let mut provider = subscription_provider(&credentials, &server);
    let mut events = Vec::new();
    assert!(run_with_events(&mut provider, &events, HeadlessTurnCancellation::new()).is_ok());
    for index in 0..127 {
        events.push(tool_result(&format!("call_{index}"), "ok", false));
        assert!(run_with_events(&mut provider, &events, HeadlessTurnCancellation::new()).is_ok());
    }
    events.push(tool_result("call_127", "ok", false));
    assert_eq!(
        run_with_events(&mut provider, &events, HeadlessTurnCancellation::new()),
        Err(HeadlessTurnPortError::Provider),
    );
    assert_eq!(server.join().len(), 128);
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn subscription_tool_replay_cancellation_and_timeout_stop_second_and_third_rounds() {
    for (round, cancellation_mode) in [(2, false), (2, true), (3, false), (3, true)] {
        let directory = temporary_directory(&format!("stop-round-{round}-{cancellation_mode}"));
        let credentials = write_credentials(&directory);
        let mut responses = vec![ScriptedResponse::Sse(tool_call_sse(
            "item_call_1",
            "call_1",
            "weather",
            "{}",
        ))];
        if round == 3 {
            responses.push(ScriptedResponse::Sse(tool_round_sse(
                &[],
                &[("item_call_2", "call_2", "weather", "{}")],
            )));
        }
        responses.push(ScriptedResponse::WaitForClientClose);
        let server = ScriptedServer::start(responses);
        let mut provider = subscription_provider(&credentials, &server);

        assert!(run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()).is_ok());
        let mut events = vec![tool_result("call_1", "first", false)];
        if round == 3 {
            assert!(
                run_with_events(&mut provider, &events, HeadlessTurnCancellation::new()).is_ok()
            );
            events.push(tool_result("call_2", "second", false));
        }

        let cancellation = if cancellation_mode {
            HeadlessTurnCancellation::new()
        } else {
            HeadlessTurnCancellation::with_deadline(Duration::from_millis(25))
        };
        let canceller = cancellation_mode.then(|| {
            let cancellation = cancellation.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(25));
                cancellation.cancel();
            })
        });
        let expected = if cancellation_mode {
            HeadlessTurnPortError::Cancelled
        } else {
            HeadlessTurnPortError::TimedOut
        };

        assert_eq!(
            run_with_events(&mut provider, &events, cancellation),
            Err(expected),
        );
        assert_eq!(
            run_with_events(&mut provider, &events, HeadlessTurnCancellation::new()),
            Err(HeadlessTurnPortError::Provider),
        );
        if let Some(canceller) = canceller {
            canceller.join().expect("canceller should finish");
        }
        assert_eq!(server.join().len(), round);
        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }
}

fn provider(credentials: &Path, base_url: &str) -> ChatGptResponsesProvider {
    ChatGptResponsesProvider::from_credentials_with_timeout(
        credentials,
        Some(base_url),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured")
}

fn subscription_provider(credentials: &Path, server: &ScriptedServer) -> ChatGptResponsesProvider {
    ChatGptResponsesProvider::from_credentials_with_tools_and_timeout_and_auth_url(
        credentials,
        Some(&server.responses_base_url()),
        Some(&server.oauth_url()),
        "test-model".to_owned(),
        "test instructions".to_owned(),
        "test input".to_owned(),
        Vec::new(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured")
}

fn assert_replay_response_rejection(name: &str, response: String) {
    let directory = temporary_directory(name);
    let credentials = write_credentials(&directory);
    let server = ScriptedServer::start(vec![ScriptedResponse::Sse(response)]);
    let mut provider = subscription_provider(&credentials, &server);

    assert_eq!(
        run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()),
        Err(HeadlessTurnPortError::Provider),
    );
    assert_eq!(
        run_with_events(&mut provider, &[], HeadlessTurnCancellation::new()),
        Err(HeadlessTurnPortError::Provider),
    );
    assert_eq!(server.join().len(), 1, "{name} must not retry");
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

fn tool_result(call_id: &str, content: &str, is_error: bool) -> agens_core::TurnEvent {
    agens_core::TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: call_id.to_owned(),
        content: content.to_owned(),
        is_error,
    })
}

fn run(
    provider: &mut ChatGptResponsesProvider,
    cancellation: HeadlessTurnCancellation,
) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should build");

    runtime.block_on(provider.next_parts(&[], &cancellation))
}

fn run_with_events(
    provider: &mut ChatGptResponsesProvider,
    events: &[agens_core::TurnEvent],
    cancellation: HeadlessTurnCancellation,
) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should build");

    runtime.block_on(provider.next_parts(events, &cancellation))
}

fn write_credentials(directory: &Path) -> PathBuf {
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"synthetic-access","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("credentials should be written");
    credentials
}

fn write_expired_credentials(directory: &Path) -> PathBuf {
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"header.eyJleHAiOjE3ODQyODg4MDB9.signature","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("expired credentials should be written");
    credentials
}

fn spawn_refresh_lock_worker(
    mode: &str,
    credentials: &Path,
    responses_url: &str,
    oauth_url: &str,
    markers: &[(&str, &Path)],
) -> std::process::Child {
    let executable = std::env::current_exe().expect("test executable should be available");
    let mut command = Command::new(executable);
    command
        .args(["--exact", "refresh_lock_subprocess_worker", "--nocapture"])
        .env(REFRESH_LOCK_WORKER_ENV, mode)
        .env("AGENS_CHATGPT_REFRESH_PATH", credentials)
        .env("AGENS_CHATGPT_RESPONSES_URL", responses_url)
        .env("AGENS_CHATGPT_OAUTH_URL", oauth_url);

    for (name, marker) in markers {
        command.env(name, marker);
    }

    command.spawn().expect("refresh lock worker should start")
}

fn wait_for_file(path: &Path, description: &str) {
    let deadline = Instant::now() + LOCK_TEST_WAIT;
    while !path.exists() {
        assert!(Instant::now() < deadline, "{description}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_success(child: &mut std::process::Child, description: &str) {
    let deadline = Instant::now() + LOCK_TEST_WAIT;
    loop {
        if let Some(status) = child.try_wait().expect("child status should be readable") {
            assert!(status.success(), "{description}");
            return;
        }

        assert!(Instant::now() < deadline, "{description}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn temporary_directory(name: &str) -> PathBuf {
    let sequence = TEMP_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "agens-providers-chatgpt-http-{name}-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temporary directory should be created");
    path
}

fn completed_text_sse(text: &str) -> String {
    format!(
        "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\n\
data: {{\"type\":\"response.completed\"}}\n\n"
    )
}

fn tool_call_sse(item_id: &str, call_id: &str, name: &str, arguments: &str) -> String {
    let reasoning = json!({
        "type": "response.output_item.done",
        "item": {
            "type": "reasoning",
            "id": "item_reasoning_1",
            "summary": [{"type": "summary_text", "text": "checking weather"}],
            "encrypted_content": "encrypted-reasoning-1",
        },
    });
    let added = json!({
        "type": "response.output_item.added",
        "item": {
            "type": "function_call",
            "id": item_id,
            "call_id": call_id,
            "name": name,
            "arguments": "",
        },
    });
    let done = json!({
        "type": "response.function_call_arguments.done",
        "item_id": item_id,
        "arguments": arguments,
    });

    format!(
        "data: {reasoning}\n\ndata: {added}\n\ndata: {done}\n\ndata: {{\"type\":\"response.completed\"}}\n\n"
    )
}

fn tool_round_sse(items: &[Value], calls: &[(&str, &str, &str, &str)]) -> String {
    let mut events = items
        .iter()
        .map(|item| json!({"type": "response.output_item.done", "item": item}).to_string())
        .collect::<Vec<_>>();

    for (item_id, call_id, name, arguments) in calls {
        events.push(json!({
            "type": "response.output_item.added",
            "item": {"type": "function_call", "id": item_id, "call_id": call_id, "name": name, "arguments": ""},
        }).to_string());
        events.push(
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": item_id,
                "arguments": arguments,
            })
            .to_string(),
        );
    }
    events.push(json!({"type": "response.completed"}).to_string());
    events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn output_item_sse(item: Value) -> String {
    format!(
        "data: {}\n\ndata: {{\"type\":\"response.completed\"}}\n\n",
        json!({"type":"response.output_item.done","item":item}),
    )
}

#[derive(Clone)]
enum ServerBehavior {
    Status(u16),
    Sse(String),
    WaitForClientClose,
}

struct ObservedRequest {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl ObservedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find_map(|(candidate, value)| (candidate == name).then_some(value.as_str()))
    }
}

struct LocalServer {
    address: std::net::SocketAddr,
    observed_request: Option<mpsc::Receiver<ObservedRequest>>,
    worker: thread::JoinHandle<()>,
}

struct OAuthServer {
    address: std::net::SocketAddr,
    worker: thread::JoinHandle<ObservedRequest>,
}

struct ControlledOAuthServer {
    address: std::net::SocketAddr,
    requests: mpsc::Receiver<ObservedRequest>,
    request_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

impl OAuthServer {
    fn start(status: u16, body: &'static str) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("OAuth server should bind");
        let address = listener
            .local_addr()
            .expect("OAuth server address should be available");
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener
                .accept()
                .expect("OAuth server should accept a request");
            let request = read_request(&stream);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .expect("OAuth response should be written");
            request
        });

        Self { address, worker }
    }

    fn url(&self) -> String {
        format!("http://{}/oauth/token", self.address)
    }

    fn join(self) -> ObservedRequest {
        self.worker.join().expect("OAuth server should finish")
    }
}

impl ControlledOAuthServer {
    fn start() -> Self {
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).expect("controlled OAuth server should bind");
        listener
            .set_nonblocking(true)
            .expect("controlled OAuth listener should be nonblocking");
        let address = listener
            .local_addr()
            .expect("controlled OAuth address should be available");
        let (sender, requests) = mpsc::channel();
        let request_count = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_request_count = request_count.clone();
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let sender = sender.clone();
                        let request_number = worker_request_count.fetch_add(1, Ordering::AcqRel);
                        thread::spawn(move || {
                            let mut stream = stream;
                            let request = read_request(&stream);
                            sender
                                .send(request)
                                .expect("test should receive the OAuth request");

                            if request_number == 0 {
                                wait_for_client_close(&stream);
                            } else {
                                write_json(
                                    &mut stream,
                                    200,
                                    r#"{"access_token":"header.eyJleHAiOjE4OTM0NTYwMDB9.signature"}"#,
                                );
                            }
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                }
            }
        });

        Self {
            address,
            requests,
            request_count,
            stop,
            worker,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/oauth/token", self.address)
    }

    fn requests(&self) -> &mpsc::Receiver<ObservedRequest> {
        &self.requests
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::Acquire)
    }

    fn join(self) {
        self.stop.store(true, Ordering::Release);
        self.worker
            .join()
            .expect("controlled OAuth server should finish");
    }
}

enum ScriptedResponse {
    Status(u16),
    Json(u16, String),
    Raw(u16, String),
    Sse(String),
    WaitForClientClose,
}

struct ScriptedServer {
    address: std::net::SocketAddr,
    worker: thread::JoinHandle<Vec<ObservedRequest>>,
}

impl ScriptedServer {
    fn start(responses: Vec<ScriptedResponse>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("scripted server should bind");
        let address = listener
            .local_addr()
            .expect("scripted server address should be available");
        let worker = thread::spawn(move || {
            let mut requests = Vec::with_capacity(responses.len());
            for response in responses {
                let (mut stream, _) = listener
                    .accept()
                    .expect("scripted server should accept a request");
                requests.push(read_request(&stream));
                match response {
                    ScriptedResponse::Status(status) => write_status(&mut stream, status),
                    ScriptedResponse::Json(status, body) => write_json(&mut stream, status, &body),
                    ScriptedResponse::Raw(status, body) => write_raw(&mut stream, status, &body),
                    ScriptedResponse::Sse(events) => write_sse(&mut stream, &events),
                    ScriptedResponse::WaitForClientClose => wait_for_client_close(&stream),
                }
            }
            requests
        });

        Self { address, worker }
    }

    fn responses_base_url(&self) -> String {
        format!("http://{}/backend-api/codex", self.address)
    }

    fn oauth_url(&self) -> String {
        format!("http://{}/oauth/token", self.address)
    }

    fn join(self) -> Vec<ObservedRequest> {
        self.worker.join().expect("scripted server should finish")
    }
}

impl LocalServer {
    fn start(behavior: ServerBehavior) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("server should bind");
        let address = listener
            .local_addr()
            .expect("server address should be available");
        let (sender, observed_request) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("server should accept a request");
            sender
                .send(read_request(&stream))
                .expect("test should receive the request");

            match behavior {
                ServerBehavior::Status(status) => write_status(&mut stream, status),
                ServerBehavior::Sse(events) => write_sse(&mut stream, &events),
                ServerBehavior::WaitForClientClose => wait_for_client_close(&stream),
            }
        });

        Self {
            address,
            observed_request: Some(observed_request),
            worker,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/backend-api/codex", self.address)
    }

    fn take_observed_request(&mut self) -> mpsc::Receiver<ObservedRequest> {
        self.observed_request
            .take()
            .expect("request receiver should only be taken once")
    }

    fn join(self) {
        self.worker.join().expect("server worker should finish");
    }
}

fn read_request(stream: &TcpStream) -> ObservedRequest {
    let mut reader = BufReader::new(stream.try_clone().expect("stream should clone"));
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("request line should be readable");
    let path = request_line
        .split_whitespace()
        .nth(1)
        .expect("request line should contain a path")
        .to_owned();

    let mut headers = Vec::new();
    let mut content_length = None;
    loop {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .expect("header should be readable");
        if header == "\r\n" {
            break;
        }
        let (name, value) = header
            .trim_end()
            .split_once(": ")
            .expect("header should be well formed");
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(
                value
                    .parse::<usize>()
                    .expect("content length should be numeric"),
            );
        }
        headers.push((name.to_ascii_lowercase(), value.to_owned()));
    }

    let mut body = vec![0; content_length.expect("request should have a content length")];
    reader
        .read_exact(&mut body)
        .expect("body should be readable");
    ObservedRequest {
        path,
        headers,
        body: serde_json::from_slice(&body).expect("body should be JSON"),
    }
}

fn write_status(stream: &mut TcpStream, status: u16) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} Test\r\nX-Secret: {SECRET_HEADER_SENTINEL}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{SECRET_BODY_SENTINEL}",
                SECRET_BODY_SENTINEL.len()
            )
            .as_bytes(),
        )
        .expect("status response should be written");
}

fn write_sse(stream: &mut TcpStream, events: &str) {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .expect("SSE headers should be written");
    stream
        .write_all(events.as_bytes())
        .expect("SSE body should be written");
}

fn write_json(stream: &mut TcpStream, status: u16, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .expect("JSON response should be written");
}

fn write_raw(stream: &mut TcpStream, status: u16, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .expect("raw response should be written");
}

fn wait_for_client_close(stream: &TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("read timeout should be configured");
    let mut byte = [0_u8; 1];
    let _ = stream
        .try_clone()
        .expect("stream should clone")
        .read(&mut byte);
}

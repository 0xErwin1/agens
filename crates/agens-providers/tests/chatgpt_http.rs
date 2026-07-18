use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Barrier, mpsc};
use std::thread;
use std::time::Duration;

use agens_core::{
    Error, HeadlessTurnCancellation, HeadlessTurnPortError, MessagePart, TurnProvider,
};
use agens_providers::ChatGptResponsesProvider;
use serde_json::{Value, json};

static TEMP_DIRECTORY_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
const SECRET_BODY_SENTINEL: &str = "SENTINEL_CHATGPT_REMOTE_BODY";
const SECRET_HEADER_SENTINEL: &str = "SENTINEL_CHATGPT_REMOTE_HEADER";
const REFRESH_WORKER_ENV: &str = "AGENS_CHATGPT_REFRESH_WORKER";

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

fn write_credentials(directory: &Path) -> PathBuf {
    let credentials = directory.join("auth.json");
    fs::write(
        &credentials,
        r#"{"openai-chatgpt":{"access_token":"synthetic-access","refresh_token":"synthetic-refresh","account_id":"account_123","expires_at":"2030-07-17T13:00:00Z"}}"#,
    )
    .expect("credentials should be written");
    credentials
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

enum ScriptedResponse {
    Status(u16),
    Json(u16, String),
    Raw(u16, String),
    Sse(String),
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

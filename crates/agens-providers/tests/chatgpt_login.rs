use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use agens_providers::chatgpt_login::{
    ChatGptCredentials, ChatGptLoginOptions, LoginCancellation, LoginError, authorization_url,
    generate_pkce, generate_state, login, upsert_chatgpt_credentials,
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

#[test]
fn authorization_url_uses_the_codex_pkce_contract_without_workspace_selection() {
    let url = authorization_url(
        "http://localhost:1455/auth/callback",
        "challenge-value",
        "state-value",
    );
    let query = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<std::collections::BTreeMap<_, _>>();

    assert_eq!(url.scheme(), "https");
    assert_eq!(url.host_str(), Some("auth.openai.com"));
    assert_eq!(url.path(), "/oauth/authorize");
    assert_eq!(
        query.get("client_id"),
        Some(&"app_EMoamEEZ73f0CkXaXp7hrann".to_owned())
    );
    assert_eq!(
        query.get("redirect_uri"),
        Some(&"http://localhost:1455/auth/callback".to_owned())
    );
    assert_eq!(
        query.get("scope"),
        Some(
            &"openid profile email offline_access api.connectors.read api.connectors.invoke"
                .to_owned()
        )
    );
    assert_eq!(
        query.get("code_challenge"),
        Some(&"challenge-value".to_owned())
    );
    assert_eq!(query.get("code_challenge_method"), Some(&"S256".to_owned()));
    assert_eq!(query.get("state"), Some(&"state-value".to_owned()));
    assert_eq!(query.get("originator"), Some(&"codex_cli_rs".to_owned()));
    assert_eq!(
        query.get("id_token_add_organizations"),
        Some(&"true".to_owned())
    );
    assert_eq!(
        query.get("codex_cli_simplified_flow"),
        Some(&"true".to_owned())
    );
    assert!(!query.contains_key("allowed_workspace_id"));
}

#[test]
fn pkce_and_state_are_unpadded_url_safe_and_derived_from_injected_randomness() {
    let random = |length| Ok((0..length).map(|value| value as u8).collect::<Vec<_>>());
    let pkce = generate_pkce(&random).expect("PKCE should be generated");
    let state = generate_state(&random).expect("state should be generated");

    assert_eq!(
        pkce.verifier,
        "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0-Pw"
    );
    assert_eq!(
        pkce.challenge,
        "wsNdZaf3VpLTsEDmR5gPk2C6xYVWxKb0xcaG3O6kX10"
    );
    assert_eq!(state, "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8");
    assert!(!pkce.verifier.contains('='));
    assert!(!state.contains('='));
}

#[test]
fn credential_and_pkce_debug_output_redact_all_secret_material() {
    let pkce = agens_providers::chatgpt_login::Pkce {
        verifier: "secret-verifier".to_owned(),
        challenge: "public-challenge".to_owned(),
    };
    let credentials = ChatGptCredentials {
        access_token: "secret-access-token".to_owned(),
        refresh_token: "secret-refresh-token".to_owned(),
        account_id: "account_123".to_owned(),
        expires_at: "2030-01-01T00:00:00Z".to_owned(),
    };

    let pkce_debug = format!("{pkce:?}");
    let credentials_debug = format!("{credentials:?}");

    for secret in [
        "secret-verifier",
        "secret-access-token",
        "secret-refresh-token",
    ] {
        assert!(!pkce_debug.contains(secret));
        assert!(!credentials_debug.contains(secret));
    }
    assert!(!pkce_debug.contains("public-challenge"));
}

#[test]
fn opener_failure_still_publishes_the_authorization_url() {
    let published = Arc::new(Mutex::new(Vec::new()));
    let publication = published.clone();
    let options = ChatGptLoginOptions {
        callback_ports: vec![0],
        timeout: Duration::from_millis(10),
        open_browser: Arc::new(|_| Err(std::io::Error::other("browser unavailable"))),
        publish_url: Arc::new(move |url| publication.lock().expect("lock").push(url.to_owned())),
        ..ChatGptLoginOptions::for_test("http://127.0.0.1:1/authorize", "http://127.0.0.1:1/token")
    };

    let result = login(options, LoginCancellation::new());

    assert!(result.is_err());
    assert_eq!(published.lock().expect("lock").len(), 1);
}

#[test]
fn login_accepts_only_the_expected_callback_then_exchanges_exact_form_and_extracts_jwt_claims() {
    let token_listener = TcpListener::bind("127.0.0.1:0").expect("token listener should bind");
    let token_url = format!(
        "http://{}/oauth/token",
        token_listener.local_addr().expect("address")
    );
    let observed_form = Arc::new(Mutex::new(String::new()));
    let form_capture = observed_form.clone();
    let token_thread = thread::spawn(move || {
        let (mut stream, _) = token_listener
            .accept()
            .expect("token request should arrive");
        let request = read_http_request(&mut stream);
        *form_capture.lock().expect("lock") = request.clone();
        let id_token = jwt(json!({"https://api.openai.com/auth.chatgpt_account_id":"account_123"}));
        let access_token = jwt(json!({"exp":1893456000}));
        write_http_response(
            &mut stream,
            200,
            &format!(
                r#"{{"id_token":"{id_token}","access_token":"{access_token}","refresh_token":"refresh-token"}}"#
            ),
        );
    });
    let published = Arc::new(Mutex::new(Vec::new()));
    let publication = published.clone();
    let options = ChatGptLoginOptions {
        callback_ports: vec![0],
        timeout: Duration::from_secs(1),
        open_browser: Arc::new(move |url| {
            let url = url::Url::parse(url).expect("authorization URL should parse");
            let redirect = url
                .query_pairs()
                .find(|(key, _)| key == "redirect_uri")
                .expect("redirect URI")
                .1
                .into_owned();
            let state = url
                .query_pairs()
                .find(|(key, _)| key == "state")
                .expect("state")
                .1
                .into_owned();
            let authority = redirect
                .trim_start_matches("http://")
                .split('/')
                .next()
                .expect("authority");
            let mut callback = TcpStream::connect(authority)?;
            write!(
                callback,
                "GET /auth/callback?state={state}&code=authorization-code HTTP/1.1\r\nHost: {authority}\r\n\r\n"
            )?;
            Ok(())
        }),
        publish_url: Arc::new(move |url| publication.lock().expect("lock").push(url.to_owned())),
        ..ChatGptLoginOptions::for_test("http://127.0.0.1:1/authorize", &token_url)
    };

    let credentials = login(options, LoginCancellation::new()).expect("login should succeed");

    token_thread.join().expect("token server should finish");
    let request = observed_form.lock().expect("lock").clone();
    assert!(
        request
            .to_ascii_lowercase()
            .contains("content-type: application/x-www-form-urlencoded")
    );
    assert!(request.contains("grant_type=authorization_code"));
    assert!(request.contains("code=authorization-code"));
    assert!(request.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
    assert!(request.contains("code_verifier="));
    assert_eq!(credentials.account_id, "account_123");
    assert_eq!(credentials.refresh_token, "refresh-token");
    assert_eq!(credentials.expires_at, "2030-01-01T00:00:00Z");
    assert_eq!(published.lock().expect("lock").len(), 1);
}

#[test]
fn login_rejects_callbacks_and_tokens_without_exposing_secret_values() {
    let cancelled = LoginCancellation::new();
    cancelled.cancel();
    let options = ChatGptLoginOptions {
        callback_ports: vec![0],
        timeout: Duration::from_secs(1),
        ..ChatGptLoginOptions::for_test("http://127.0.0.1:1/authorize", "http://127.0.0.1:1/token")
    };

    assert_eq!(login(options, cancelled), Err(LoginError::Cancelled));
}

#[test]
fn callback_state_error_and_missing_code_are_sanitized_authentication_failures() {
    for callback in [
        "state=wrong-state&code=secret-authorization-code",
        "state={state}&error=access_denied",
        "state={state}",
    ] {
        let callback = callback.to_owned();
        let options = ChatGptLoginOptions {
            callback_ports: vec![0],
            timeout: Duration::from_secs(1),
            open_browser: Arc::new(move |url| {
                let url = url::Url::parse(url).expect("authorization URL should parse");
                let redirect = url
                    .query_pairs()
                    .find(|(key, _)| key == "redirect_uri")
                    .expect("redirect URI")
                    .1
                    .into_owned();
                let state = url
                    .query_pairs()
                    .find(|(key, _)| key == "state")
                    .expect("state")
                    .1
                    .into_owned();
                let authority = redirect
                    .trim_start_matches("http://")
                    .split('/')
                    .next()
                    .expect("authority");
                let query = callback.replace("{state}", &state);
                let mut stream = TcpStream::connect(authority)?;
                write!(
                    stream,
                    "GET /auth/callback?{query} HTTP/1.1\r\nHost: {authority}\r\n\r\n"
                )?;
                Ok(())
            }),
            ..ChatGptLoginOptions::for_test(
                "http://127.0.0.1:1/authorize",
                "http://127.0.0.1:1/token",
            )
        };

        let rendered = login(options, LoginCancellation::new())
            .expect_err("invalid callback should fail")
            .to_string();
        assert!(rendered.starts_with("ChatGPT authentication required:"));
        assert!(!rendered.contains("secret-authorization-code"));
        assert!(!rendered.contains("wrong-state"));
    }
}

#[test]
fn login_times_out_without_a_callback() {
    let options = ChatGptLoginOptions {
        callback_ports: vec![0],
        timeout: Duration::from_millis(10),
        ..ChatGptLoginOptions::for_test("http://127.0.0.1:1/authorize", "http://127.0.0.1:1/token")
    };

    assert_eq!(
        login(options, LoginCancellation::new()),
        Err(LoginError::TimedOut)
    );
}

#[test]
fn callback_rejects_duplicate_parameters_malformed_encoding_and_untrusted_hosts() {
    for query_and_host in [
        (
            "state={state}&state=duplicate&code=authorization-code",
            "localhost",
        ),
        (
            "state={state}&code=authorization-code&code=duplicate",
            "localhost",
        ),
        (
            "state={state}&error=access_denied&error_description=duplicate",
            "localhost",
        ),
        ("state=%FF&code=authorization-code", "localhost"),
        ("state={state}&code=authorization-code", "attacker.example"),
    ] {
        let (query, host) = query_and_host;
        let (status_send, status_receive) = mpsc::channel();
        let options = ChatGptLoginOptions {
            callback_ports: vec![0],
            timeout: Duration::from_secs(1),
            open_browser: Arc::new(move |url| {
                let url = url::Url::parse(url).expect("authorization URL should parse");
                let redirect = url
                    .query_pairs()
                    .find(|(key, _)| key == "redirect_uri")
                    .expect("redirect URI")
                    .1
                    .into_owned();
                let state = url
                    .query_pairs()
                    .find(|(key, _)| key == "state")
                    .expect("state")
                    .1
                    .into_owned();
                let authority = redirect
                    .trim_start_matches("http://")
                    .split('/')
                    .next()
                    .expect("authority");
                let mut callback = TcpStream::connect(authority)?;
                let query = query.replace("{state}", &state);
                let host = if host == "localhost" { authority } else { host };
                write!(
                    callback,
                    "GET /auth/callback?{query} HTTP/1.1\r\nHost: {host}\r\n\r\n"
                )?;
                let status_send = status_send.clone();
                thread::spawn(move || {
                    let _ = callback.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut response = [0_u8; 128];
                    let mut status = String::new();
                    while !status.contains("\r\n") {
                        let Ok(read) = callback.read(&mut response) else {
                            break;
                        };
                        if read == 0 {
                            break;
                        }
                        status.push_str(&String::from_utf8_lossy(&response[..read]));
                    }
                    let _ = status_send.send(status);
                });
                Ok(())
            }),
            ..ChatGptLoginOptions::for_test(
                "http://127.0.0.1:1/authorize",
                "http://127.0.0.1:1/token",
            )
        };

        let error = login(options, LoginCancellation::new()).expect_err("callback must fail");

        assert!(matches!(error, LoginError::Authentication(_)));
        let status = status_receive
            .recv_timeout(Duration::from_secs(1))
            .expect("callback response");
        assert!(status.starts_with("HTTP/1.1 400"), "response: {status:?}");
    }
}

#[cfg(unix)]
#[test]
fn upsert_creates_private_credentials_and_preserves_unrelated_fields_without_storing_id_tokens() {
    use std::os::unix::fs::PermissionsExt;

    let directory = temporary_directory("upsert");
    let path = directory.join("nested/auth.json");
    fs::create_dir_all(path.parent().expect("parent")).expect("parent should be created");
    fs::write(&path, r#"{"other":{"api_key":"preserve"},"openai-chatgpt":{"custom":"keep","id_token":"remove"}}"#).expect("credentials should be written");
    let credentials = ChatGptCredentials {
        access_token: "access-token".to_owned(),
        refresh_token: "refresh-token".to_owned(),
        account_id: "account_123".to_owned(),
        expires_at: "2030-01-01T00:00:00Z".to_owned(),
    };

    upsert_chatgpt_credentials(&path, &credentials).expect("upsert should succeed");

    let persisted: Value =
        serde_json::from_slice(&fs::read(&path).expect("credentials should be readable"))
            .expect("credentials should remain JSON");
    assert_eq!(persisted["other"]["api_key"], "preserve");
    assert_eq!(persisted["openai-chatgpt"]["custom"], "keep");
    assert_eq!(persisted["openai-chatgpt"]["access_token"], "access-token");
    assert!(persisted["openai-chatgpt"].get("id_token").is_none());
    assert_eq!(
        fs::metadata(path.parent().expect("parent"))
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o077,
        0
    );
    assert_eq!(
        fs::metadata(&path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o077,
        0
    );
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[cfg(unix)]
#[test]
fn upsert_creates_missing_auth_json_and_private_parent_directory() {
    use std::os::unix::fs::PermissionsExt;

    let directory = temporary_directory("create");
    let path = directory.join("missing/auth.json");
    let credentials = ChatGptCredentials {
        access_token: "access-token".to_owned(),
        refresh_token: "refresh-token".to_owned(),
        account_id: "account_123".to_owned(),
        expires_at: "2030-01-01T00:00:00Z".to_owned(),
    };

    upsert_chatgpt_credentials(&path, &credentials).expect("upsert should create credentials");

    let persisted: Value =
        serde_json::from_slice(&fs::read(&path).expect("credentials should be readable"))
            .expect("credentials should remain JSON");
    assert_eq!(persisted["openai-chatgpt"]["account_id"], "account_123");
    assert_eq!(
        fs::metadata(path.parent().expect("parent"))
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o077,
        0
    );
    assert_eq!(
        fs::metadata(&path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o077,
        0
    );
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[cfg(unix)]
#[test]
fn upsert_fails_closed_for_symlinked_or_hardlinked_auth_files() {
    use std::os::unix::fs::symlink;

    let directory = temporary_directory("unsafe-auth-path");
    let target = directory.join("target.json");
    let symlinked = directory.join("symlinked.json");
    let hardlinked = directory.join("hardlinked.json");
    fs::write(&target, r#"{"other":{"preserve":true}}"#).expect("target should be written");
    symlink(&target, &symlinked).expect("symlink should be created");
    fs::hard_link(&target, &hardlinked).expect("hardlink should be created");
    let credentials = ChatGptCredentials {
        access_token: "access-token".to_owned(),
        refresh_token: "refresh-token".to_owned(),
        account_id: "account_123".to_owned(),
        expires_at: "2030-01-01T00:00:00Z".to_owned(),
    };

    for path in [&symlinked, &hardlinked] {
        let error =
            upsert_chatgpt_credentials(path, &credentials).expect_err("unsafe path must fail");
        assert!(matches!(error, LoginError::Authentication(_)));
    }
    assert_eq!(
        fs::read_to_string(&target).expect("target remains readable"),
        r#"{"other":{"preserve":true}}"#
    );
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

fn jwt(payload: Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    format!(
        "{header}.{}.signature",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("payload should encode"))
    )
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("request timeout should be set");
    let mut buffer = [0_u8; 4096];
    let read = stream
        .read(&mut buffer)
        .expect("request should be readable");
    String::from_utf8(buffer[..read].to_vec()).expect("request should be UTF-8")
}

fn write_http_response(stream: &mut TcpStream, status: u16, body: &str) {
    write!(stream, "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("response should be written");
    stream.flush().expect("response should flush");
}

fn temporary_directory(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "agens-providers-chatgpt-login-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("temporary directory should be created");
    directory
}

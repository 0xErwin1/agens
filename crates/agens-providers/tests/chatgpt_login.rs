use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use agens_providers::chatgpt_login::{
    ChatGptCredentials, ChatGptDeviceCodeLoginOptions, ChatGptLoginOptions, LoginCancellation,
    LoginError, authorization_url, device_code_login, device_code_login_with_progress,
    generate_pkce, generate_state, login, upsert_chatgpt_credentials, upsert_provider_entry,
    upsert_provider_entry_with_deadline,
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fs4::fs_std::FileExt;
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
fn device_code_login_uses_exact_json_requests_and_validates_the_server_pkce_pair() {
    let device_listener = TcpListener::bind("127.0.0.1:0").expect("device listener should bind");
    let device_endpoint = format!(
        "http://{}/api/accounts/deviceauth/usercode",
        device_listener.local_addr().expect("device address")
    );
    let poll_listener = TcpListener::bind("127.0.0.1:0").expect("poll listener should bind");
    let poll_endpoint = format!(
        "http://{}/api/accounts/deviceauth/token",
        poll_listener.local_addr().expect("poll address")
    );
    let oauth_listener = TcpListener::bind("127.0.0.1:0").expect("OAuth listener should bind");
    let oauth_endpoint = format!(
        "http://{}/oauth/token",
        oauth_listener.local_addr().expect("OAuth address")
    );

    let pkce = generate_pkce(&|length| Ok(vec![42; length])).expect("PKCE should generate");
    let verifier = pkce.verifier;
    let challenge = pkce.challenge;

    let device_server = thread::spawn(move || {
        let (mut stream, _) = device_listener
            .accept()
            .expect("device request should arrive");
        let request = read_http_request(&mut stream);
        assert!(request.starts_with("POST /api/accounts/deviceauth/usercode HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert!(request.ends_with(r#"{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}"#));
        write_http_response(
            &mut stream,
            200,
            r#"{"device_auth_id":"private-device-id","usercode":"ABCD-EFGH","interval":"0.001"}"#,
        );
    });
    let poll_server = thread::spawn(move || {
        for status in [403, 200] {
            let (mut stream, _) = poll_listener.accept().expect("poll request should arrive");
            let request = read_http_request(&mut stream);
            assert!(request.starts_with("POST /api/accounts/deviceauth/token HTTP/1.1"));
            assert!(
                request
                    .ends_with(r#"{"device_auth_id":"private-device-id","user_code":"ABCD-EFGH"}"#)
            );
            if status == 403 {
                write_http_response(&mut stream, status, "{}");
            } else {
                write_http_response(
                    &mut stream,
                    status,
                    &format!(
                        r#"{{"authorization_code":"authorization-code","code_challenge":"{challenge}","code_verifier":"{verifier}"}}"#
                    ),
                );
            }
        }
    });
    let oauth_server = thread::spawn(move || {
        let (mut stream, _) = oauth_listener
            .accept()
            .expect("OAuth request should arrive");
        let request = read_http_request(&mut stream);
        assert!(request.contains("grant_type=authorization_code"));
        assert!(request.contains("code=authorization-code"));
        assert!(
            request.contains("redirect_uri=https%3A%2F%2Fauth.openai.com%2Fdeviceauth%2Fcallback")
        );
        assert!(request.contains("code_verifier="));
        assert!(!request.contains("code_challenge"));
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

    let published_code = Arc::new(Mutex::new(None));
    let result = device_code_login_with_progress(
        ChatGptDeviceCodeLoginOptions::for_test(&device_endpoint, &poll_endpoint, &oauth_endpoint),
        LoginCancellation::new(),
        {
            let published_code = Arc::clone(&published_code);
            move |verification_url, user_code| {
                *published_code
                    .lock()
                    .expect("published device code lock should be available") =
                    Some((verification_url.to_owned(), user_code.to_owned()));
            }
        },
    )
    .expect("device login should succeed");

    assert_eq!(
        result.verification_url,
        "https://auth.openai.com/codex/device"
    );
    assert_eq!(result.user_code, "ABCD-EFGH");
    assert_eq!(result.credentials.account_id, "account_123");
    assert_eq!(
        *published_code
            .lock()
            .expect("published device code lock should be available"),
        Some((
            "https://auth.openai.com/codex/device".to_owned(),
            "ABCD-EFGH".to_owned()
        ))
    );
    device_server.join().expect("device server should finish");
    poll_server.join().expect("poll server should finish");
    oauth_server.join().expect("OAuth server should finish");
}

#[test]
fn device_code_login_rejects_mismatched_or_malformed_server_pkce_without_token_exchange() {
    for (challenge, verifier) in [
        ("mismatched-challenge".to_owned(), "verifier".to_owned()),
        ("not+base64url".to_owned(), "verifier".to_owned()),
        ("challenge".to_owned(), "bad=padding".to_owned()),
    ] {
        let user_listener = TcpListener::bind("127.0.0.1:0").expect("user listener should bind");
        let user_endpoint = format!(
            "http://{}/usercode",
            user_listener.local_addr().expect("address")
        );
        let poll_listener = TcpListener::bind("127.0.0.1:0").expect("poll listener should bind");
        let poll_endpoint = format!(
            "http://{}/token",
            poll_listener.local_addr().expect("address")
        );
        let oauth_listener = TcpListener::bind("127.0.0.1:0").expect("OAuth listener should bind");
        let oauth_endpoint = format!(
            "http://{}/token",
            oauth_listener.local_addr().expect("address")
        );

        let user_server = thread::spawn(move || {
            let (mut stream, _) = user_listener.accept().expect("user request should arrive");
            let _ = read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                200,
                r#"{"device_auth_id":"private-device-id","user_code":"code","interval":"0.001"}"#,
            );
        });
        let response_challenge = challenge.clone();
        let response_verifier = verifier.clone();
        let poll_server = thread::spawn(move || {
            let (mut stream, _) = poll_listener.accept().expect("poll request should arrive");
            let _ = read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                200,
                &format!(
                    r#"{{"authorization_code":"secret-code","code_challenge":"{response_challenge}","code_verifier":"{response_verifier}"}}"#
                ),
            );
        });

        let rendered = device_code_login(
            ChatGptDeviceCodeLoginOptions::for_test(
                &user_endpoint,
                &poll_endpoint,
                &oauth_endpoint,
            ),
            LoginCancellation::new(),
        )
        .expect_err("invalid server PKCE must fail before exchanging tokens")
        .to_string();

        assert!(rendered.starts_with("ChatGPT authentication required:"));
        assert!(!rendered.contains("secret-code"));
        assert!(!rendered.contains(&challenge));
        assert!(!rendered.contains(&verifier));
        oauth_listener
            .set_nonblocking(true)
            .expect("OAuth listener should become nonblocking");
        thread::sleep(Duration::from_millis(20));
        assert!(matches!(
            oauth_listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        user_server.join().expect("user server should finish");
        poll_server.join().expect("poll server should finish");
    }
}

#[test]
fn device_code_login_retries_many_pending_responses_before_success_with_exact_request_count() {
    let user_listener = TcpListener::bind("127.0.0.1:0").expect("user listener should bind");
    let user_endpoint = format!(
        "http://{}/usercode",
        user_listener.local_addr().expect("address")
    );
    let poll_listener = TcpListener::bind("127.0.0.1:0").expect("poll listener should bind");
    let poll_endpoint = format!(
        "http://{}/token",
        poll_listener.local_addr().expect("address")
    );
    let oauth_listener = TcpListener::bind("127.0.0.1:0").expect("OAuth listener should bind");
    let oauth_endpoint = format!(
        "http://{}/token",
        oauth_listener.local_addr().expect("address")
    );
    let pkce = generate_pkce(&|length| Ok(vec![7; length])).expect("PKCE should generate");

    let user_server = thread::spawn(move || {
        let (mut stream, _) = user_listener.accept().expect("user request should arrive");
        let _ = read_http_request(&mut stream);
        write_http_response(
            &mut stream,
            200,
            r#"{"device_auth_id":"device","user_code":"code","interval":"0.001"}"#,
        );
    });
    let poll_server = thread::spawn(move || {
        for status in [403, 404, 403, 404, 403, 404, 200] {
            let (mut stream, _) = poll_listener.accept().expect("poll request should arrive");
            let request = read_http_request(&mut stream);
            assert!(request.ends_with(r#"{"device_auth_id":"device","user_code":"code"}"#));
            let body = if status == 200 {
                format!(
                    r#"{{"authorization_code":"code","code_challenge":"{}","code_verifier":"{}"}}"#,
                    pkce.challenge, pkce.verifier
                )
            } else {
                "{}".to_owned()
            };
            write_http_response(&mut stream, status, &body);
        }
    });
    let oauth_server = thread::spawn(move || {
        let (mut stream, _) = oauth_listener
            .accept()
            .expect("OAuth request should arrive");
        let _ = read_http_request(&mut stream);
        let id_token = jwt(json!({"https://api.openai.com/auth.chatgpt_account_id":"account"}));
        let access_token = jwt(json!({"exp":1893456000}));
        write_http_response(
            &mut stream,
            200,
            &format!(
                r#"{{"id_token":"{id_token}","access_token":"{access_token}","refresh_token":"refresh"}}"#
            ),
        );
    });

    let result = device_code_login(
        ChatGptDeviceCodeLoginOptions::for_test(&user_endpoint, &poll_endpoint, &oauth_endpoint),
        LoginCancellation::new(),
    )
    .expect("seventh poll should succeed");

    assert_eq!(result.credentials.account_id, "account");
    user_server.join().expect("user server should finish");
    poll_server.join().expect("poll server should finish");
    oauth_server.join().expect("OAuth server should finish");
}

#[test]
fn device_code_login_cancels_while_a_poll_request_is_blocked() {
    let user_listener = TcpListener::bind("127.0.0.1:0").expect("user listener should bind");
    let user_endpoint = format!(
        "http://{}/usercode",
        user_listener.local_addr().expect("address")
    );
    let poll_listener = TcpListener::bind("127.0.0.1:0").expect("poll listener should bind");
    let poll_endpoint = format!(
        "http://{}/token",
        poll_listener.local_addr().expect("address")
    );
    let user_server = thread::spawn(move || {
        let (mut stream, _) = user_listener.accept().expect("user request should arrive");
        let _ = read_http_request(&mut stream);
        write_http_response(
            &mut stream,
            200,
            r#"{"device_auth_id":"device","user_code":"code","interval":"1"}"#,
        );
    });
    let poll_server = thread::spawn(move || {
        let (mut stream, _) = poll_listener.accept().expect("poll request should arrive");
        let _ = read_http_request(&mut stream);
        thread::sleep(Duration::from_millis(150));
        write_http_response(&mut stream, 403, "{}");
    });
    let cancellation = LoginCancellation::new();
    let canceller = cancellation.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(15));
        canceller.cancel();
    });

    let started = Instant::now();
    assert_eq!(
        device_code_login(
            ChatGptDeviceCodeLoginOptions::for_test(
                &user_endpoint,
                &poll_endpoint,
                "http://127.0.0.1:1/token"
            ),
            cancellation,
        ),
        Err(LoginError::Cancelled)
    );
    assert!(started.elapsed() < Duration::from_millis(150));
    user_server.join().expect("user server should finish");
    poll_server.join().expect("poll server should finish");
}

#[test]
fn device_code_login_uses_one_absolute_deadline_across_user_code_polls_and_exchange() {
    let user_listener = TcpListener::bind("127.0.0.1:0").expect("user listener should bind");
    let user_endpoint = format!(
        "http://{}/usercode",
        user_listener.local_addr().expect("address")
    );
    let poll_listener = TcpListener::bind("127.0.0.1:0").expect("poll listener should bind");
    let poll_endpoint = format!(
        "http://{}/token",
        poll_listener.local_addr().expect("address")
    );
    let oauth_listener = TcpListener::bind("127.0.0.1:0").expect("OAuth listener should bind");
    let oauth_endpoint = format!(
        "http://{}/token",
        oauth_listener.local_addr().expect("address")
    );
    let pkce = generate_pkce(&|length| Ok(vec![3; length])).expect("PKCE should generate");

    let user_server = thread::spawn(move || {
        let (mut stream, _) = user_listener.accept().expect("user request should arrive");
        let _ = read_http_request(&mut stream);
        thread::sleep(Duration::from_millis(4));
        write_http_response(
            &mut stream,
            200,
            r#"{"device_auth_id":"device","user_code":"code","interval":"0.001"}"#,
        );
    });
    let poll_server = thread::spawn(move || {
        let (mut pending, _) = poll_listener.accept().expect("pending poll should arrive");
        let _ = read_http_request(&mut pending);
        thread::sleep(Duration::from_millis(4));
        write_http_response(&mut pending, 403, "{}");
        let (mut ready, _) = poll_listener.accept().expect("ready poll should arrive");
        let _ = read_http_request(&mut ready);
        thread::sleep(Duration::from_millis(4));
        write_http_response(
            &mut ready,
            200,
            &format!(
                r#"{{"authorization_code":"code","code_challenge":"{}","code_verifier":"{}"}}"#,
                pkce.challenge, pkce.verifier
            ),
        );
    });
    let oauth_server = thread::spawn(move || {
        let (mut stream, _) = oauth_listener.accept().expect("exchange should arrive");
        let _ = read_http_request(&mut stream);
        thread::sleep(Duration::from_millis(150));
        write_http_response(&mut stream, 500, "{}");
    });
    let mut options =
        ChatGptDeviceCodeLoginOptions::for_test(&user_endpoint, &poll_endpoint, &oauth_endpoint);
    options.timeout = Duration::from_millis(100);

    assert_eq!(
        device_code_login(options, LoginCancellation::new()),
        Err(LoginError::TimedOut)
    );
    user_server.join().expect("user server should finish");
    poll_server.join().expect("poll server should finish");
    oauth_server.join().expect("OAuth server should finish");
}

#[test]
fn device_code_login_rejects_unavailable_and_invalid_user_code_responses_without_secrets() {
    for body in [
        r#"{"device_auth_id":"private-device-id","user_code":"code","interval":"0"}"#,
        r#"{"device_auth_id":"private-device-id","user_code":"code","interval":"999999"}"#,
        r#"{"device_auth_id":"private-device-id","user_code":"code","interval":"not-a-number"}"#,
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
        let endpoint = format!(
            "http://{}/usercode",
            listener.local_addr().expect("address")
        );
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("request should arrive");
            let _ = read_http_request(&mut stream);
            write_http_response(&mut stream, 200, body);
        });
        let error = device_code_login(
            ChatGptDeviceCodeLoginOptions::for_test(
                &endpoint,
                "http://127.0.0.1:1/poll",
                "http://127.0.0.1:1/token",
            ),
            LoginCancellation::new(),
        )
        .expect_err("invalid interval must fail")
        .to_string();
        assert!(error.contains("device authorization response is invalid"));
        assert!(!error.contains("private-device-id"));
        server.join().expect("server should finish");
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    let endpoint = format!(
        "http://{}/usercode",
        listener.local_addr().expect("address")
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("request should arrive");
        let _ = read_http_request(&mut stream);
        write_http_response(
            &mut stream,
            404,
            r#"{"device_auth_id":"private-device-id"}"#,
        );
    });
    assert_eq!(
        device_code_login(
            ChatGptDeviceCodeLoginOptions::for_test(
                &endpoint,
                "http://127.0.0.1:1/poll",
                "http://127.0.0.1:1/token"
            ),
            LoginCancellation::new(),
        ),
        Err(LoginError::Authentication("Authentication unavailable"))
    );
    server.join().expect("server should finish");
}

#[test]
fn device_code_login_handles_pending_timeout_and_cancellation_during_sleep_and_requests() {
    let user_listener = TcpListener::bind("127.0.0.1:0").expect("user listener should bind");
    let user_endpoint = format!(
        "http://{}/usercode",
        user_listener.local_addr().expect("address")
    );
    let poll_listener = TcpListener::bind("127.0.0.1:0").expect("poll listener should bind");
    let poll_endpoint = format!(
        "http://{}/token",
        poll_listener.local_addr().expect("address")
    );
    let user_server = thread::spawn(move || {
        let (mut stream, _) = user_listener.accept().expect("user request should arrive");
        let _ = read_http_request(&mut stream);
        write_http_response(
            &mut stream,
            200,
            r#"{"device_auth_id":"private-device-id","user_code":"code","interval":"10"}"#,
        );
    });
    let poll_server = thread::spawn(move || {
        let (mut stream, _) = poll_listener.accept().expect("poll request should arrive");
        let _ = read_http_request(&mut stream);
        write_http_response(&mut stream, 403, "{}");
    });
    let cancellation = LoginCancellation::new();
    let canceller = cancellation.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(15));
        canceller.cancel();
    });
    let started = Instant::now();
    assert_eq!(
        device_code_login(
            ChatGptDeviceCodeLoginOptions::for_test(
                &user_endpoint,
                &poll_endpoint,
                "http://127.0.0.1:1/token"
            ),
            cancellation,
        ),
        Err(LoginError::Cancelled)
    );
    assert!(started.elapsed() < Duration::from_millis(150));
    user_server.join().expect("user server should finish");
    poll_server.join().expect("poll server should finish");

    let user_listener = TcpListener::bind("127.0.0.1:0").expect("user listener should bind");
    let user_endpoint = format!(
        "http://{}/usercode",
        user_listener.local_addr().expect("address")
    );
    let poll_listener = TcpListener::bind("127.0.0.1:0").expect("poll listener should bind");
    let poll_endpoint = format!(
        "http://{}/token",
        poll_listener.local_addr().expect("address")
    );
    let user_server = thread::spawn(move || {
        let (mut stream, _) = user_listener.accept().expect("user request should arrive");
        let _ = read_http_request(&mut stream);
        write_http_response(
            &mut stream,
            200,
            r#"{"device_auth_id":"private-device-id","user_code":"code","interval":"1"}"#,
        );
    });
    let poll_server = thread::spawn(move || {
        let (mut stream, _) = poll_listener.accept().expect("poll request should arrive");
        let _ = read_http_request(&mut stream);
        write_http_response(&mut stream, 404, "{}");
    });
    let mut options = ChatGptDeviceCodeLoginOptions::for_test(
        &user_endpoint,
        &poll_endpoint,
        "http://127.0.0.1:1/token",
    );
    options.timeout = Duration::from_millis(15);
    assert_eq!(
        device_code_login(options, LoginCancellation::new()),
        Err(LoginError::TimedOut)
    );
    user_server.join().expect("user server should finish");
    poll_server.join().expect("poll server should finish");

    let listener = TcpListener::bind("127.0.0.1:0").expect("listener should bind");
    let endpoint = format!(
        "http://{}/usercode",
        listener.local_addr().expect("address")
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("request should arrive");
        let _ = read_http_request(&mut stream);
        thread::sleep(Duration::from_millis(100));
        write_http_response(&mut stream, 200, "{}");
    });
    let cancellation = LoginCancellation::new();
    let canceller = cancellation.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(15));
        canceller.cancel();
    });
    assert_eq!(
        device_code_login(
            ChatGptDeviceCodeLoginOptions::for_test(
                &endpoint,
                "http://127.0.0.1:1/poll",
                "http://127.0.0.1:1/token"
            ),
            cancellation,
        ),
        Err(LoginError::Cancelled)
    );
    server.join().expect("server should finish");
}

#[test]
fn device_code_login_rejects_nonpending_and_malformed_success_and_cancels_token_exchange() {
    for (status, body) in [
        (500, "{}"),
        (
            200,
            r#"{"authorization_code":"secret-code","code_challenge":"challenge"}"#,
        ),
    ] {
        let user_listener = TcpListener::bind("127.0.0.1:0").expect("user listener should bind");
        let user_endpoint = format!(
            "http://{}/usercode",
            user_listener.local_addr().expect("address")
        );
        let poll_listener = TcpListener::bind("127.0.0.1:0").expect("poll listener should bind");
        let poll_endpoint = format!(
            "http://{}/token",
            poll_listener.local_addr().expect("address")
        );
        let user_server = thread::spawn(move || {
            let (mut stream, _) = user_listener.accept().expect("user request should arrive");
            let _ = read_http_request(&mut stream);
            write_http_response(
                &mut stream,
                200,
                r#"{"device_auth_id":"private-device-id","user_code":"code","interval":"0.001"}"#,
            );
        });
        let poll_server = thread::spawn(move || {
            let (mut stream, _) = poll_listener.accept().expect("poll request should arrive");
            let _ = read_http_request(&mut stream);
            write_http_response(&mut stream, status, body);
        });
        let rendered = device_code_login(
            ChatGptDeviceCodeLoginOptions::for_test(
                &user_endpoint,
                &poll_endpoint,
                "http://127.0.0.1:1/token",
            ),
            LoginCancellation::new(),
        )
        .expect_err("invalid poll result must fail")
        .to_string();
        assert!(rendered.contains("device authorization"));
        assert!(!rendered.contains("private-device-id"));
        assert!(!rendered.contains("secret-code"));
        user_server.join().expect("user server should finish");
        poll_server.join().expect("poll server should finish");
    }

    let user_listener = TcpListener::bind("127.0.0.1:0").expect("user listener should bind");
    let user_endpoint = format!(
        "http://{}/usercode",
        user_listener.local_addr().expect("address")
    );
    let poll_listener = TcpListener::bind("127.0.0.1:0").expect("poll listener should bind");
    let poll_endpoint = format!(
        "http://{}/token",
        poll_listener.local_addr().expect("address")
    );
    let oauth_listener = TcpListener::bind("127.0.0.1:0").expect("OAuth listener should bind");
    let oauth_endpoint = format!(
        "http://{}/token",
        oauth_listener.local_addr().expect("address")
    );
    let pkce = generate_pkce(&|length| Ok(vec![9; length])).expect("PKCE should generate");
    let user_server = thread::spawn(move || {
        let (mut stream, _) = user_listener.accept().expect("user request should arrive");
        let _ = read_http_request(&mut stream);
        write_http_response(
            &mut stream,
            200,
            r#"{"device_auth_id":"private-device-id","user_code":"code","interval":"0.001"}"#,
        );
    });
    let poll_server = thread::spawn(move || {
        let (mut stream, _) = poll_listener.accept().expect("poll request should arrive");
        let _ = read_http_request(&mut stream);
        write_http_response(
            &mut stream,
            200,
            &format!(
                r#"{{"authorization_code":"authorization-code","code_challenge":"{}","code_verifier":"{}"}}"#,
                pkce.challenge, pkce.verifier
            ),
        );
    });
    let oauth_server = thread::spawn(move || {
        let (mut stream, _) = oauth_listener
            .accept()
            .expect("OAuth request should arrive");
        let _ = read_http_request(&mut stream);
        thread::sleep(Duration::from_millis(100));
        write_http_response(&mut stream, 500, "{}");
    });
    let cancellation = LoginCancellation::new();
    let canceller = cancellation.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(15));
        canceller.cancel();
    });
    assert_eq!(
        device_code_login(
            ChatGptDeviceCodeLoginOptions::for_test(
                &user_endpoint,
                &poll_endpoint,
                &oauth_endpoint
            ),
            cancellation,
        ),
        Err(LoginError::Cancelled)
    );
    user_server.join().expect("user server should finish");
    poll_server.join().expect("poll server should finish");
    oauth_server.join().expect("OAuth server should finish");
}

#[test]
fn provider_entry_upsert_preserves_existing_provider_entries() {
    let directory = temporary_directory("provider-entry-merge");
    let path = directory.join("auth.json");
    fs::write(&path, r#"{"other":{"api_key":"preserve"}}"#).expect("credentials should be written");

    upsert_provider_entry(&path, "second-provider", json!({"api_key":"second"}))
        .expect("provider entry should be persisted");

    let persisted: Value =
        serde_json::from_slice(&fs::read(&path).expect("credentials should be readable"))
            .expect("credentials should remain JSON");
    assert_eq!(persisted["other"]["api_key"], "preserve");
    assert_eq!(persisted["second-provider"]["api_key"], "second");

    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn provider_entry_upsert_cancels_or_times_out_while_the_credentials_lock_is_held() {
    let directory = temporary_directory("lock-cancellation");
    let path = directory.join("auth.json");
    upsert_provider_entry(&path, "seed", json!({"value":"seed"})).expect("seed should persist");
    let lock_path = directory.join(".auth.json.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("lock should open");
    assert!(lock.try_lock_exclusive().expect("lock should be available"));

    for cancellation_first in [true, false, true, false] {
        let cancellation = LoginCancellation::new();
        if cancellation_first {
            cancellation.cancel();
        }
        let started = Instant::now();
        let result = upsert_provider_entry_with_deadline(
            &path,
            "blocked",
            json!({"value":"must-not-persist"}),
            &cancellation,
            Instant::now() + Duration::from_millis(30),
        );
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "lock wait did not stop promptly"
        );
        assert_eq!(
            result,
            Err(if cancellation_first {
                LoginError::Cancelled
            } else {
                LoginError::TimedOut
            })
        );
        let persisted: Value =
            serde_json::from_slice(&fs::read(&path).expect("credentials should be readable"))
                .expect("credentials should remain JSON");
        assert!(persisted.get("blocked").is_none());
        assert!(
            fs::read_dir(&directory)
                .expect("directory should be readable")
                .all(|entry| !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".auth-login-"))
        );
    }
    lock.unlock().expect("lock should unlock");
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[cfg(unix)]
#[test]
fn provider_entry_upsert_refuses_symlinked_parent_without_touching_the_target() {
    use std::os::unix::fs::symlink;

    let directory = temporary_directory("symlinked-parent");
    let outside = temporary_directory("symlinked-parent-outside");
    let parent = directory.join("parent");
    symlink(&outside, &parent).expect("parent symlink should be created");
    let path = parent.join("auth.json");

    let result = upsert_provider_entry(&path, "provider", json!({"api_key":"must-not-write"}));

    assert!(matches!(result, Err(LoginError::Authentication(_))));
    assert!(!outside.join("auth.json").exists());
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
    fs::remove_dir_all(outside).expect("temporary directory should be removed");
}

#[test]
fn provider_entry_upsert_merges_concurrent_processes_using_canonical_path_aliases() {
    let directory = temporary_directory("process-merge");
    let path = directory.join("auth.json");
    let alias = directory.join(".").join("auth.json");
    let children = [
        spawn_upsert_child(&path, "first-provider", json!({"api_key":"first"})),
        spawn_upsert_child(&alias, "second-provider", json!({"api_key":"second"})),
        spawn_upsert_child(&path, "openai-chatgpt", json!({"access_token":"access"})),
        spawn_upsert_child(&alias, "openai-chatgpt", json!({"refresh_token":"refresh"})),
    ];
    for mut child in children {
        assert!(
            child.wait().expect("child should wait").success(),
            "child upsert should succeed"
        );
    }

    let persisted: Value =
        serde_json::from_slice(&fs::read(&path).expect("credentials should be readable"))
            .expect("credentials should remain JSON");
    assert_eq!(persisted["first-provider"]["api_key"], "first");
    assert_eq!(persisted["second-provider"]["api_key"], "second");
    assert_eq!(persisted["openai-chatgpt"]["access_token"], "access");
    assert_eq!(persisted["openai-chatgpt"]["refresh_token"], "refresh");
    #[cfg(unix)]
    assert_eq!(fs::metadata(&path).expect("metadata").mode() & 0o077, 0);

    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn provider_entry_upsert_reacquires_the_os_lock_after_a_holder_crashes() {
    let directory = temporary_directory("holder-crash");
    let path = directory.join("auth.json");
    upsert_provider_entry(&path, "seed", json!({"value":"seed"})).expect("seed should persist");
    let ready = directory.join("holder-ready");
    let mut holder = Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg("provider_entry_upsert_child_process")
        .env("AGENS_LOGIN_CHILD_ACTION", "hold-lock")
        .env("AGENS_LOGIN_CHILD_PATH", &path)
        .env("AGENS_LOGIN_CHILD_READY", &ready)
        .spawn()
        .expect("holder should start");
    for _ in 0..100 {
        if ready.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(ready.exists(), "holder never acquired the lock");
    holder.kill().expect("holder should be killable");
    holder.wait().expect("holder should exit");

    upsert_provider_entry(&path, "after-crash", json!({"value":"persisted"}))
        .expect("OS must release the killed holder's lock");
    let persisted: Value =
        serde_json::from_slice(&fs::read(&path).expect("credentials should be readable"))
            .expect("credentials should remain JSON");
    assert_eq!(persisted["after-crash"]["value"], "persisted");
    fs::remove_dir_all(directory).expect("temporary directory should be removed");
}

#[test]
fn provider_entry_upsert_child_process() {
    let action = match std::env::var("AGENS_LOGIN_CHILD_ACTION") {
        Ok(action) => action,
        Err(_) => return,
    };
    let path = PathBuf::from(std::env::var("AGENS_LOGIN_CHILD_PATH").expect("child path"));
    if action == "upsert" {
        let provider = std::env::var("AGENS_LOGIN_CHILD_PROVIDER").expect("child provider");
        let entry: Value =
            serde_json::from_str(&std::env::var("AGENS_LOGIN_CHILD_ENTRY").expect("child entry"))
                .expect("entry JSON");
        upsert_provider_entry(&path, &provider, entry).expect("child upsert should succeed");
        return;
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.parent().expect("parent").join(".auth.json.lock"))
        .expect("lock should open");
    assert!(lock.try_lock_exclusive().expect("lock should be available"));
    fs::write(
        std::env::var("AGENS_LOGIN_CHILD_READY").expect("ready path"),
        "ready",
    )
    .expect("ready marker");
    thread::sleep(Duration::from_secs(10));
}

fn spawn_upsert_child(path: &std::path::Path, provider: &str, entry: Value) -> std::process::Child {
    Command::new(std::env::current_exe().expect("test executable"))
        .arg("--exact")
        .arg("provider_entry_upsert_child_process")
        .env("AGENS_LOGIN_CHILD_ACTION", "upsert")
        .env("AGENS_LOGIN_CHILD_PATH", path)
        .env("AGENS_LOGIN_CHILD_PROVIDER", provider)
        .env(
            "AGENS_LOGIN_CHILD_ENTRY",
            serde_json::to_string(&entry).expect("entry should encode"),
        )
        .spawn()
        .expect("child should start")
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

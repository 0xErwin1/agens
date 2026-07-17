use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use agens_core::Error;
use agens_providers::{
    ProviderCancellation, decode_openai_response_stream, persist_chatgpt_refresh,
};

const ACCESS_TOKEN: &str = "synthetic-access-token-should-not-escape";
const REFRESH_TOKEN: &str = "synthetic-refresh-token-should-not-escape";
const AUTHORIZATION_HEADER: &str = "Authorization: Bearer synthetic-authorization-header";
const CREDENTIAL_BODY: &str = "synthetic-credential-body-should-not-escape";

#[test]
fn cancelling_a_pending_stream_returns_the_typed_cancelled_outcome_promptly() {
    let (_sender, receiver) = mpsc::channel();
    let cancellation = ProviderCancellation::new();
    let canceller = cancellation.clone();

    thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        canceller.cancel();
    });

    let started_at = Instant::now();
    let result = decode_openai_response_stream(receiver, &cancellation);

    assert_eq!(result, Err(Error::Cancelled));
    assert!(started_at.elapsed() < Duration::from_millis(250));
}

#[test]
fn cancellation_does_not_commit_queued_stream_parts_after_it_is_requested() {
    let (sender, receiver) = mpsc::channel();
    let cancellation = ProviderCancellation::new();

    sender
        .send(r#"{"type":"response.output_text.delta","delta":"before cancellation"}"#.to_owned())
        .expect("protocol double should queue the first event");
    cancellation.cancel();
    sender
        .send(r#"{"type":"response.output_text.delta","delta":"after cancellation"}"#.to_owned())
        .expect("protocol double should queue the later event");

    assert_eq!(
        decode_openai_response_stream(receiver, &cancellation),
        Err(Error::Cancelled)
    );
}

#[test]
fn provider_and_authentication_failures_are_not_cancellation_or_credential_diagnostics() {
    let (sender, receiver) = mpsc::channel();
    let cancellation = ProviderCancellation::new();
    let provider_body =
        format!("{ACCESS_TOKEN}; {REFRESH_TOKEN}; {AUTHORIZATION_HEADER}; {CREDENTIAL_BODY}");

    sender
        .send(format!(r#"{{"type":"error","message":"{provider_body}"}}"#))
        .expect("protocol double should queue the provider failure");

    let (failed_sender, failed_receiver) = mpsc::channel();
    failed_sender
        .send(format!(
            r#"{{"type":"response.failed","response":{{"error":{{"message":"{provider_body}"}}}}}}"#
        ))
        .expect("protocol double should queue the failed response");

    let provider_error = decode_openai_response_stream(receiver, &cancellation)
        .expect_err("provider failure should remain distinct from cancellation");
    let failed_response_error = decode_openai_response_stream(failed_receiver, &cancellation)
        .expect_err("failed response should remain distinct from cancellation");
    let auth_error = persist_chatgpt_refresh(
        std::path::Path::new("/not-used-after-validation.json"),
        ACCESS_TOKEN,
        Some(REFRESH_TOKEN),
        CREDENTIAL_BODY,
    )
    .expect_err("unavailable credentials should return authentication failure");

    assert!(matches!(provider_error, Error::Provider(_)));
    assert!(matches!(failed_response_error, Error::Provider(_)));
    assert!(matches!(auth_error, Error::Auth(_)));

    for diagnostic in [
        provider_error.to_string(),
        format!("{provider_error:?}"),
        failed_response_error.to_string(),
        format!("{failed_response_error:?}"),
        auth_error.to_string(),
        format!("{auth_error:?}"),
    ] {
        assert!(!diagnostic.contains(ACCESS_TOKEN));
        assert!(!diagnostic.contains(REFRESH_TOKEN));
        assert!(!diagnostic.contains(AUTHORIZATION_HEADER));
        assert!(!diagnostic.contains(CREDENTIAL_BODY));
    }
}

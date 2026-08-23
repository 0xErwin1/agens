use std::collections::VecDeque;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

mod support;

use support::{
    RequestArrival, SSE_HEADERS, accept_until_stopped, bind_pollable_listener,
    hold_address_until_stopped, read_request, read_request_head, wait_for_client_close,
    write_sse_headers, write_to_possibly_gone_client,
};

use agens_core::{
    HeadlessTurnCancellation, HeadlessTurnPortError, MessagePart, RequestConfig, TurnEvent,
    TurnProvider,
};
use agens_providers::{
    OpenAiFunctionTool, OpenAiResponsesProvider, ProviderDiagnosticClass, ProviderDiagnosticEvent,
    ProviderDiagnosticKind, ProviderDiagnosticScope, ProviderDiagnostics, ProviderFailureDetail,
    RetryPolicy,
};
use serde_json::json;

const SECRET_BODY_SENTINEL: &str = "SENTINEL_REMOTE_ERROR_BODY";
const SECRET_HEADER_SENTINEL: &str = "SENTINEL_REMOTE_ERROR_HEADER";

/// How long a cancelled request may still take to unwind, measured from the `cancel` call
/// rather than from the start of the request so that connect and request-observation
/// latency stay out of it.
///
/// This must remain well under the one-second operation timeout those tests give the
/// provider: cancellation that is merely converted into a timeout would otherwise satisfy
/// the bound and the test would stop proving that cancellation is what ended the request.
const CANCELLATION_RESPONSE_BUDGET: Duration = Duration::from_millis(500);

#[test]
fn persistent_connect_failures_stop_at_the_provider_operation_deadline() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let started = Instant::now();

    let result = run_provider_with_operation_timeout(
        format!("http://{address}"),
        HeadlessTurnCancellation::new(),
        Duration::from_millis(25),
    );

    assert_eq!(result, Err(HeadlessTurnPortError::TimedOut));
    assert!(started.elapsed() < Duration::from_millis(250));
}

#[test]
fn shorter_parent_deadline_caps_the_provider_operation_deadline() {
    let server = LocalResponsesServer::start(ServerMode::DelayedHeaders);
    let started = Instant::now();

    let result = run_provider_with_operation_timeout(
        server.base_url(),
        HeadlessTurnCancellation::with_deadline(Duration::from_millis(25)),
        Duration::from_secs(1),
    );

    assert_eq!(result, Err(HeadlessTurnPortError::TimedOut));
    assert!(started.elapsed() < Duration::from_millis(250));
    server.join();
}

#[test]
fn cancellation_interrupts_connect_headers_stalled_body_and_late_events() {
    for mode in [
        ServerMode::StalledConnect,
        ServerMode::DelayedHeaders,
        ServerMode::StalledBody,
        ServerMode::LateEvent,
    ] {
        let mut server = LocalResponsesServer::start(mode);
        let cancellation = HeadlessTurnCancellation::new();
        let canceller = cancellation.clone();
        let observed_request =
            (!matches!(mode, ServerMode::StalledConnect)).then(|| server.take_observed_request());

        let canceller_thread = thread::spawn(move || {
            if let Some(observed_request) = observed_request {
                observed_request
                    .recv_timeout(Duration::from_secs(1))
                    .expect("server should observe the request before cancellation");
            } else {
                thread::sleep(Duration::from_millis(10));
            }
            canceller.cancel();

            Instant::now()
        });

        let result = run_provider(server.base_url(), cancellation, Duration::from_secs(1));
        let finished_at = Instant::now();

        assert_eq!(result, Err(HeadlessTurnPortError::Cancelled));
        let cancelled_at = canceller_thread
            .join()
            .expect("canceller thread should finish");
        assert!(
            finished_at.saturating_duration_since(cancelled_at) < CANCELLATION_RESPONSE_BUDGET,
            "{mode:?} took {:?} to stop after cancellation",
            finished_at.saturating_duration_since(cancelled_at)
        );
        server.join();
    }
}

/// Task and file-descriptor counts are process-wide, so this is the only assertion in the
/// suite that a *sibling* test can move: libtest runs the rest of this binary on parallel
/// threads of this same process, and each of them transiently owns threads and sockets of
/// its own. Re-running the workload in a private child process is what makes the counts
/// belong to it alone, so the bound below stays the strict one it was written as instead of
/// being traded against how noisy the rest of the binary happens to be.
#[test]
fn one_hundred_same_process_cancellations_and_timeouts_have_bounded_resources() {
    if std::env::var_os(RESOURCE_ISOLATION_ENV).is_some() {
        assert_the_whole_workload_leaks_nothing();
        return;
    }

    let child = Command::new(std::env::current_exe().expect("test executable should be locatable"))
        .arg("--exact")
        .arg("one_hundred_same_process_cancellations_and_timeouts_have_bounded_resources")
        .env(RESOURCE_ISOLATION_ENV, "1")
        .output()
        .expect("isolated child should start");
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );

    assert!(child.status.success(), "{report}");
    assert!(
        report.contains("1 passed"),
        "the isolated child must have run the workload: {report}"
    );
}

const RESOURCE_ISOLATION_ENV: &str = "AGENS_OPENAI_HTTP_RESOURCE_CHILD";

fn assert_the_whole_workload_leaks_nothing() {
    let baseline = ResourceSnapshot::capture();

    for _ in 0..100 {
        let server = LocalResponsesServer::start(ServerMode::DelayedHeaders);
        let cancellation = HeadlessTurnCancellation::with_deadline(Duration::from_millis(25));

        let result = run_provider(server.base_url(), cancellation, Duration::from_secs(1));

        assert_eq!(result, Err(HeadlessTurnPortError::TimedOut));
        server.join();
    }

    for _ in 0..100 {
        let mut server = LocalResponsesServer::start(ServerMode::DelayedHeaders);
        let cancellation = HeadlessTurnCancellation::new();
        let observed_request = server.take_observed_request();
        let canceller = cancellation.clone();
        let cancellation_thread = thread::spawn(move || {
            observed_request
                .recv_timeout(Duration::from_secs(1))
                .expect("server should observe the request before cancellation");
            canceller.cancel();
        });

        let result = run_provider(server.base_url(), cancellation, Duration::from_secs(1));

        assert_eq!(result, Err(HeadlessTurnPortError::Cancelled));
        cancellation_thread
            .join()
            .expect("cancellation thread should finish");
        server.join();
    }

    // Nothing else runs in this process, so what is left to settle is the workload's own
    // asynchronous teardown: a closed socket lingers until the kernel retires it. A real
    // leak is permanent and per-iteration, so it never settles however long this waits.
    let settle_deadline = Instant::now() + Duration::from_secs(10);
    let mut after = ResourceSnapshot::capture();
    while after.tasks > baseline.tasks + 2 || after.file_descriptors > baseline.file_descriptors + 2
    {
        if Instant::now() >= settle_deadline {
            break;
        }
        thread::sleep(Duration::from_millis(25));
        after = ResourceSnapshot::capture();
    }
    assert!(
        after.tasks <= baseline.tasks + 2,
        "task count grew from {} to {}",
        baseline.tasks,
        after.tasks
    );
    assert!(
        after.file_descriptors <= baseline.file_descriptors + 2,
        "file descriptor count grew from {} to {}",
        baseline.file_descriptors,
        after.file_descriptors
    );
}

#[test]
fn cancellation_wins_when_a_remote_error_completes_after_cancellation() {
    let mut server = LocalResponsesServer::start(ServerMode::CancelledError);
    let cancellation = HeadlessTurnCancellation::new();
    let observed_request = server.take_observed_request();
    let canceller = cancellation.clone();
    let cancellation_thread = thread::spawn(move || {
        observed_request
            .recv_timeout(Duration::from_secs(1))
            .expect("server should observe the request");
        canceller.cancel();
    });

    let result = run_provider(server.base_url(), cancellation, Duration::from_secs(1));

    assert_eq!(result, Err(HeadlessTurnPortError::Cancelled));
    cancellation_thread
        .join()
        .expect("cancellation thread should finish");
    server.join();
}

/// A response event echoes the whole request back, so a session carrying many
/// MCP tool definitions produces `response.created` / `response.completed`
/// frames far larger than any model output. Such a frame must still decode.
#[test]
fn a_response_event_echoing_a_large_tool_set_is_decoded_rather_than_rejected() {
    let server = LocalResponsesServer::start(ServerMode::LargeToolEchoFrame);
    let result = run_provider(
        server.base_url(),
        HeadlessTurnCancellation::with_deadline(Duration::from_secs(5)),
        Duration::from_secs(5),
    );

    assert_eq!(result, Ok(()));
    server.join();
}

#[test]
fn malformed_unterminated_or_oversized_frames_and_remote_errors_are_sanitized_provider_failures() {
    for (mode, expected) in [
        (
            ServerMode::MalformedFrame,
            HeadlessTurnPortError::ProviderProtocol,
        ),
        (
            ServerMode::UnterminatedOversizedFrame,
            HeadlessTurnPortError::ProviderProtocol,
        ),
        (
            ServerMode::OversizedFrame,
            HeadlessTurnPortError::ProviderProtocol,
        ),
        (ServerMode::ErrorBody, HeadlessTurnPortError::ProviderServer),
    ] {
        let server = LocalResponsesServer::start(mode);
        let result = run_provider(
            server.base_url(),
            HeadlessTurnCancellation::with_deadline(Duration::from_secs(3)),
            Duration::from_secs(3),
        );

        assert_eq!(result, Err(expected));
        server.join();
    }
}

#[test]
fn openai_transport_uses_frozen_failure_precedence() {
    for (status, body, expected) in [
        (
            401,
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            HeadlessTurnPortError::Authentication,
        ),
        (
            403,
            r#"{"error":{"type":"context_length_exceeded"}}"#,
            HeadlessTurnPortError::Authentication,
        ),
        (
            429,
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            HeadlessTurnPortError::ProviderRateLimited {
                reset_after_seconds: None,
            },
        ),
        (
            500,
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            HeadlessTurnPortError::ProviderServer,
        ),
        (
            400,
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            HeadlessTurnPortError::ProviderContext,
        ),
        (
            400,
            r#"{"error":{"type":"context_length_exceeded"}}"#,
            HeadlessTurnPortError::ProviderContext,
        ),
        (
            400,
            r#"{"error":{"code":"invalid_request"}}"#,
            HeadlessTurnPortError::ProviderRejected,
        ),
        (
            418,
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            HeadlessTurnPortError::ProviderContext,
        ),
    ] {
        let server = LocalResponsesServer::start_error_response(status, body);

        assert_eq!(
            run_provider(
                server.base_url(),
                HeadlessTurnCancellation::new(),
                Duration::from_secs(1),
            ),
            Err(expected),
        );
        server.join();
    }
}

/// A provider that names the overflow in prose rather than in a code is still
/// reporting an overflow: without this it is classified as a plain rejection,
/// and nothing downstream can tell an exhausted context apart from a malformed
/// request.
#[test]
fn context_overflow_is_recognised_across_provider_error_shapes() {
    for (body, expected) in [
        (
            r#"{"error":{"code":"request_too_large","message":"too big"}}"#,
            HeadlessTurnPortError::ProviderContext,
        ),
        (
            r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
            HeadlessTurnPortError::ProviderContext,
        ),
        (
            r#"{"error":{"type":"ValidationException","message":"Input token count exceeds the maximum number of input tokens"}}"#,
            HeadlessTurnPortError::ProviderContext,
        ),
        (
            r#"{"error":{"message":"This model's maximum context length is 128000 tokens"}}"#,
            HeadlessTurnPortError::ProviderContext,
        ),
        (
            r#"{"error":{"message":"input is too long for the model"}}"#,
            HeadlessTurnPortError::ProviderContext,
        ),
        (
            r#"{"error":{"code":"context_length_exceeded_extra","message":"something else"}}"#,
            HeadlessTurnPortError::ProviderRejected,
        ),
        (
            r#"{"error":{"code":"invalid_request","message":"the model does not exist"}}"#,
            HeadlessTurnPortError::ProviderRejected,
        ),
    ] {
        let server = LocalResponsesServer::start_error_response(400, body);

        assert_eq!(
            run_provider(
                server.base_url(),
                HeadlessTurnCancellation::new(),
                Duration::from_secs(1),
            ),
            Err(expected),
            "misclassified {body}",
        );
        server.join();
    }
}

#[test]
fn rejected_status_records_body_status_and_model_for_a_user_visible_sink() {
    let server = LocalResponsesServer::start_error_response(
        400,
        r#"{"error":{"code":"model_not_found","message":"The model `gpt-9-missing` does not exist"}}"#,
    );
    let failure_detail = ProviderFailureDetail::new();
    let mut provider = OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&server.base_url()),
        "gpt-9-missing".into(),
        "test prompt".into(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured")
    .with_failure_detail(failure_detail.clone());

    assert_eq!(
        provider_runtime()
            .block_on(provider.next_parts(&[], &HeadlessTurnCancellation::new()))
            .map(|_| ()),
        Err(HeadlessTurnPortError::ProviderRejected)
    );

    let detail = failure_detail
        .take()
        .expect("a rejected request should record failure detail");
    assert!(detail.contains("400"), "{detail}");
    assert!(detail.contains("gpt-9-missing"), "{detail}");
    assert!(
        detail.contains("The model `gpt-9-missing` does not exist"),
        "{detail}"
    );

    server.join();
}

#[test]
fn no_failure_detail_handle_means_nothing_is_recorded() {
    let server = LocalResponsesServer::start_error_response(
        400,
        r#"{"error":{"code":"model_not_found","message":"The model `gpt-9-missing` does not exist"}}"#,
    );

    assert_eq!(
        run_provider(
            server.base_url(),
            HeadlessTurnCancellation::new(),
            Duration::from_secs(1),
        ),
        Err(HeadlessTurnPortError::ProviderRejected)
    );

    server.join();
}

#[test]
fn mid_stream_response_failed_event_records_its_payload_for_a_user_visible_sink() {
    let server = LocalResponsesServer::start_scripted(vec![concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"the model is temporarily overloaded\"}}}\n\n"
    )
    .to_owned()]);
    let failure_detail = ProviderFailureDetail::new();
    let mut provider = OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&server.base_url()),
        "test-model".into(),
        "test prompt".into(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured")
    .with_failure_detail(failure_detail.clone());

    assert_eq!(
        provider_runtime()
            .block_on(provider.next_parts(&[], &HeadlessTurnCancellation::new()))
            .map(|_| ()),
        Err(HeadlessTurnPortError::ProviderProtocol)
    );

    assert_eq!(
        failure_detail.take(),
        Some("the model is temporarily overloaded".to_owned())
    );

    server.join();
}

/// Regression pin for the SSE frame-drain fix (WU-3): with `finish_on_terminal=false`, a
/// discarded mid-stream decode error must not carry the response's whole output loss with it.
/// Before that fix, `process_sse_frame` left its buffer undrained on the error path, so the
/// very next `\n` reprocessed the SAME undrained bytes and this stream reported
/// `Err(ProviderProtocol)` even though a real, later `output_text.delta` had already arrived —
/// the whole response was silently lost. Nothing else in this suite pins the recovered
/// behavior, so a future refactor could reintroduce the loss with every other test green.
#[test]
fn a_recovered_mid_stream_error_event_does_not_lose_the_completed_response() {
    let server = LocalResponsesServer::start_scripted(vec![
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "data: {\"type\":\"error\",\"message\":\"transient hiccup\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"the answer\"}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n"
        )
        .to_owned(),
    ]);
    let mut provider = OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&server.base_url()),
        "test-model".into(),
        "test prompt".into(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured");

    let parts = provider_runtime()
        .block_on(provider.next_parts(&[], &HeadlessTurnCancellation::new()))
        .expect("a recovered mid-stream error must not lose the completed response");

    assert_eq!(parts, vec![MessagePart::Text("the answer".to_owned())]);

    server.join();
}

/// A recovered mid-stream `error` event still records its text into the failure-detail handle
/// (`upstream_error`) even though the round it happened in ultimately succeeds. `agens-headless`
/// only drains the handle once per whole attempt, so without a drain at the top of every
/// `next_parts` call, that recovered incident's text would still be sitting in the handle when a
/// later, unrelated round in the SAME turn fails — making the stale incident look like the cause
/// of a failure it had nothing to do with.
#[test]
fn a_recovered_mid_stream_error_does_not_leak_into_a_later_same_turn_failure() {
    let server = LocalResponsesServer::start_scripted(vec![
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "data: {\"type\":\"error\",\"message\":\"transient hiccup\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"the answer\"}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n"
        )
        .to_owned(),
    ]);
    let failure_detail = ProviderFailureDetail::new();
    let mut provider = OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&server.base_url()),
        "test-model".into(),
        "test prompt".into(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured")
    .with_failure_detail(failure_detail.clone());
    let runtime = provider_runtime();

    let first_round = runtime.block_on(provider.next_parts(&[], &HeadlessTurnCancellation::new()));
    assert_eq!(
        first_round,
        Ok(vec![MessagePart::Text("the answer".to_owned())])
    );

    // The provider is now `Completed`, so this round fails without recording any new detail —
    // exactly the shape a later, unrelated same-turn failure would take.
    let second_round = runtime.block_on(provider.next_parts(&[], &HeadlessTurnCancellation::new()));
    assert_eq!(
        second_round.map(|_| ()),
        Err(HeadlessTurnPortError::Provider)
    );

    assert_eq!(failure_detail.take(), None);

    server.join();
}

#[test]
fn openai_retries_transient_statuses_twice_then_succeeds() {
    let server = RetryResponsesServer::start(vec![
        RetryResponse::Status(500),
        RetryResponse::Status(429),
        RetryResponse::Sse(completed_text_response("retried", "done")),
    ]);

    assert_eq!(
        run_provider(
            server.base_url(),
            HeadlessTurnCancellation::new(),
            Duration::from_secs(1),
        ),
        Ok(())
    );
    assert_eq!(server.join(), 3);
}

#[test]
fn openai_keeps_retrying_transient_failures_until_success_or_cancellation() {
    let server = RetryResponsesServer::start(vec![
        RetryResponse::Disconnect,
        RetryResponse::Disconnect,
        RetryResponse::Disconnect,
        RetryResponse::Sse(completed_text_response("retried", "done")),
    ]);

    assert_eq!(
        run_provider(
            server.base_url(),
            HeadlessTurnCancellation::new(),
            Duration::from_secs(1),
        ),
        Ok(())
    );
    assert_eq!(server.join(), 4);
}

#[test]
fn openai_honors_numeric_retry_after_before_the_next_attempt() {
    let server = RetryResponsesServer::start(vec![
        RetryResponse::StatusWithRetryAfter(429, "1"),
        RetryResponse::Sse(completed_text_response("retried", "done")),
    ]);
    let started_at = Instant::now();

    assert_eq!(
        run_provider(
            server.base_url(),
            HeadlessTurnCancellation::new(),
            Duration::from_secs(2),
        ),
        Ok(())
    );
    assert!(started_at.elapsed() >= Duration::from_millis(900));
    assert_eq!(server.join(), 2);
}

#[test]
fn openai_emits_allowlisted_retry_diagnostics_with_one_reference() {
    let server = RetryResponsesServer::start(vec![
        RetryResponse::Status(500),
        RetryResponse::Sse(completed_text_response("retried", "done")),
    ]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let diagnostics = ProviderDiagnostics::new(
        "abc12345",
        ProviderDiagnosticScope::Parent,
        Arc::new(move |event| captured.lock().expect("event lock").push(event)),
    )
    .expect("diagnostics should be configured");
    let mut provider = OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&server.base_url()),
        "test-model".into(),
        "test prompt".into(),
        Duration::from_secs(2),
    )
    .expect("provider should be configured")
    .with_diagnostics(diagnostics);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime should build");

    assert!(
        runtime
            .block_on(provider.next_parts(&[], &HeadlessTurnCancellation::new()))
            .is_ok()
    );
    let events = events.lock().expect("event lock");
    assert_eq!(events.len(), 4);
    assert_eq!(events[0].event, ProviderDiagnosticKind::Attempt);
    assert_eq!(events[1].event, ProviderDiagnosticKind::RetryScheduled);
    assert_eq!(events[1].class, Some(ProviderDiagnosticClass::Server));
    assert!(matches!(events[1].delay_ms, Some(1_000..=1_250)));
    assert_eq!(
        events[1].max_attempts,
        u8::try_from(RetryPolicy::default().max_attempts()).expect("the budget fits in a byte")
    );
    assert_eq!(events[2].event, ProviderDiagnosticKind::Attempt);
    assert_eq!(events[3].event, ProviderDiagnosticKind::Terminal);
    assert_eq!(events[3].class, None);
    assert!(
        events
            .iter()
            .all(|event| event.reference.as_str() == "abc12345")
    );
    drop(events);
    assert_eq!(server.join(), 2);
}

/// The truncation this covers is the one the provider actually produces: a
/// response that opens, sends its lifecycle preamble, and then loses its
/// connection. Before, that failed the turn as a protocol error and the user
/// re-sent the whole prompt.
#[test]
fn openai_retries_a_stream_cut_before_it_produced_output() {
    let server = RetryResponsesServer::start(vec![
        RetryResponse::Sse(created_only_response("cut")),
        RetryResponse::Sse(completed_text_response("retried", "done")),
    ]);

    assert_eq!(
        run_provider_with_retry_policy(
            server.base_url(),
            &HeadlessTurnCancellation::new(),
            Duration::from_secs(2),
            brisk_retry_policy(4),
            None,
        ),
        Ok(())
    );
    assert_eq!(server.join(), 2);
}

/// Retrying is only honest while nothing has reached the user. The decoded
/// deltas are handed to the progress sink as they arrive, so a second attempt
/// after the first has streamed text would show that text twice.
#[test]
fn openai_does_not_retry_a_stream_cut_after_it_produced_output() {
    let server = RetryResponsesServer::start(vec![
        RetryResponse::Sse(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"half an answer\"}\n\n"
                .to_owned(),
        ),
        RetryResponse::Sse(completed_text_response("retried", "done")),
    ]);

    assert_eq!(
        run_provider_with_retry_policy(
            server.base_url(),
            &HeadlessTurnCancellation::new(),
            Duration::from_secs(2),
            brisk_retry_policy(4),
            None,
        ),
        Err(HeadlessTurnPortError::ProviderNetwork)
    );
    assert_eq!(server.join(), 1);
}

/// A stream that is cut every time still ends, and it ends against the attempt
/// budget rather than against the user's patience.
#[test]
fn openai_stops_retrying_a_stream_cut_at_the_attempt_budget() {
    let server = RetryResponsesServer::start(vec![
        RetryResponse::Sse(created_only_response("cut")),
        RetryResponse::Sse(created_only_response("cut")),
        RetryResponse::Sse(created_only_response("cut")),
        RetryResponse::Sse(created_only_response("cut")),
    ]);

    assert_eq!(
        run_provider_with_retry_policy(
            server.base_url(),
            &HeadlessTurnCancellation::new(),
            Duration::from_secs(2),
            brisk_retry_policy(3),
            None,
        ),
        Err(HeadlessTurnPortError::ProviderNetwork)
    );
    assert_eq!(server.join(), 3);
}

/// A named `Retry-After` replaces the exponential schedule instead of being
/// bounded by it, so the attempt budget alone said nothing about how long a
/// request could be held: eight attempts against a provider that names the
/// maximum delay every time is minutes, not the schedule's ninety seconds.
///
/// The total wait is what ends this one, well before the attempts run out.
#[test]
fn openai_stops_retrying_when_the_total_wait_budget_is_spent() {
    let server = RetryResponsesServer::start(vec![
        RetryResponse::StatusWithRetryAfter(429, "0.04"),
        RetryResponse::StatusWithRetryAfter(429, "0.04"),
        RetryResponse::StatusWithRetryAfter(429, "0.04"),
        RetryResponse::StatusWithRetryAfter(429, "0.04"),
        RetryResponse::StatusWithRetryAfter(429, "0.04"),
        RetryResponse::StatusWithRetryAfter(429, "0.04"),
        RetryResponse::StatusWithRetryAfter(429, "0.04"),
        RetryResponse::StatusWithRetryAfter(429, "0.04"),
    ]);
    let policy = RetryPolicy::new(
        8,
        Duration::from_millis(10),
        Duration::from_millis(40),
        Duration::from_millis(40),
        Duration::from_millis(100),
    );

    assert_eq!(
        run_provider_with_retry_policy(
            server.base_url(),
            &HeadlessTurnCancellation::new(),
            Duration::from_secs(2),
            policy,
            None,
        ),
        Err(HeadlessTurnPortError::ProviderRateLimited {
            // The scripted `Retry-After` names a delay this request could
            // afford, so the refusal carries the provider's own reset.
            reset_after_seconds: Some(0),
        })
    );
    // Two waits of the capped 40ms fit the 100ms budget and a third does not,
    // so the eight-attempt budget is never reached.
    assert_eq!(server.join(), 3);
}

/// A quota wall is not a burst, and the two are told apart by the delay the
/// provider named: one this request cannot honour ends the retries there and
/// then, and the refusal carries the reset uncapped so a caller can park on it
/// instead of asking again on a schedule of its own.
#[test]
fn openai_stops_at_a_named_delay_it_cannot_honour_and_reports_the_reset() {
    let server = RetryResponsesServer::start(vec![RetryResponse::StatusWithRetryAfter(
        429, "3600",
    )]);

    assert_eq!(
        run_provider_with_retry_policy(
            server.base_url(),
            &HeadlessTurnCancellation::new(),
            Duration::from_secs(2),
            brisk_retry_policy(4),
            None,
        ),
        Err(HeadlessTurnPortError::ProviderRateLimited {
            reset_after_seconds: Some(3_600),
        })
    );
    assert_eq!(server.join(), 1);
}

/// Connection failures used to be exempt from the attempt budget and retried
/// once a second forever. An interactive turn carries no deadline of its own,
/// so that was a spinner with no end.
#[test]
fn openai_bounds_connection_retries_by_the_attempt_budget() {
    // Binding an address and dropping it leaves it free for anything else on
    // the machine to take between the attempts this counts, which would turn a
    // failed connection into an answered request. Holding it and closing every
    // connection keeps the address this test's own and fails each attempt the
    // same way.
    let (listener, address) = bind_pollable_listener();
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = thread::spawn(move || hold_address_until_stopped(&listener, &worker_stop));
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let diagnostics = ProviderDiagnostics::new(
        "abc12345",
        ProviderDiagnosticScope::Parent,
        Arc::new(move |event| captured.lock().expect("event lock").push(event)),
    )
    .expect("diagnostics should be configured");

    assert_eq!(
        run_provider_with_retry_policy(
            format!("http://{address}"),
            &HeadlessTurnCancellation::new(),
            Duration::from_secs(2),
            brisk_retry_policy(3),
            Some(diagnostics),
        ),
        Err(HeadlessTurnPortError::ProviderNetwork)
    );
    stop.store(true, Ordering::Release);
    worker.join().expect("the address holder should finish");

    // Each scheduled retry now names the budget it counts against, which is
    // what the status line renders as `Retrying (n/m)`. While connection
    // retries were unbounded there was no `m` to report, so the one failure a
    // user is most likely to hit was also the one they could see least of.
    let events = events.lock().expect("event lock");
    let scheduled = events
        .iter()
        .filter(|event| event.event == ProviderDiagnosticKind::RetryScheduled)
        .collect::<Vec<_>>();
    assert_eq!(scheduled.len(), 2);
    for event in scheduled {
        assert_eq!(event.class, Some(ProviderDiagnosticClass::Network));
        assert_eq!(event.max_attempts, 3);
        assert!(event.delay_ms.is_some());
    }
}

/// The read timeout is the only thing bounding a response that opened and then
/// stopped emitting. Without it the body read waited forever and the session
/// stayed "running" until somebody cancelled it by hand.
#[test]
fn openai_read_timeout_ends_a_stream_that_stops_emitting() {
    let server = LocalResponsesServer::start(ServerMode::StalledBody);

    assert_eq!(
        run_provider_with_retry_policy(
            server.base_url(),
            &HeadlessTurnCancellation::new(),
            Duration::from_millis(200),
            brisk_retry_policy(1),
            None,
        ),
        Err(HeadlessTurnPortError::ProviderNetwork)
    );

    server.join();
}

#[test]
fn openai_does_not_retry_permanent_or_partial_stream_failures() {
    let permanent = RetryResponsesServer::start(vec![RetryResponse::Status(400)]);
    assert_eq!(
        run_provider(
            permanent.base_url(),
            HeadlessTurnCancellation::new(),
            Duration::from_secs(1),
        ),
        Err(HeadlessTurnPortError::ProviderRejected)
    );
    assert_eq!(permanent.join(), 1);

    let partial = RetryResponsesServer::start(vec![RetryResponse::Sse(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\ndata: {not-json}\n\n"
            .to_owned(),
    )]);
    assert_eq!(
        run_provider(
            partial.base_url(),
            HeadlessTurnCancellation::new(),
            Duration::from_secs(1),
        ),
        Err(HeadlessTurnPortError::ProviderProtocol)
    );
    assert_eq!(partial.join(), 1);
}

/// Both halves prove the same property: the retry backoff was interrupted rather than
/// slept through. That makes each elapsed bound a ceiling chosen against the backoff it
/// must undercut, not a latency budget — it only has to stay clearly below the delay the
/// provider would otherwise wait, and every millisecond under that is headroom for a
/// loaded machine.
///
/// The cancelled half gets no `Retry-After`, so its shortest possible backoff is
/// `HTTP_RETRY_FIRST_DELAY` (250 ms, jitter only adds); 200 ms is the honest ceiling
/// there. Its stop signal is a real event — a thread that cancels once the server has
/// observed the request — so only wall time can order it.
///
/// The deadline half needs no such bet. Its deadline runs on a manual clock that stands
/// still until the retry diagnostic fires, which is emitted inside `wait_for_http_retry`
/// after the backoff has been scheduled and before it is waited on. The deadline
/// therefore expires exactly there, on every machine and under any load: the request can
/// never be cut short before it is sent (so the single observed request is guaranteed,
/// not likely), and the interrupted wait returns with no sleeping at all. What is left of
/// the elapsed bound is a backstop against sleeping through the 5 s backoff
/// (`Retry-After: 5`, capped at `HTTP_RETRY_AFTER_CAP`), not a latency budget.
///
/// Do not tighten these back toward the request latency; they are deliberately loose.
#[test]
fn openai_cancellation_and_deadline_interrupt_retry_backoff() {
    let mut cancelled_server = RetryResponsesServer::start(vec![RetryResponse::Status(500)]);
    let observed = cancelled_server.take_observed_request();
    let cancellation = HeadlessTurnCancellation::new();
    let canceller = cancellation.clone();
    let cancellation_thread = thread::spawn(move || {
        observed
            .recv_timeout(Duration::from_secs(1))
            .expect("first request should be observed");
        canceller.cancel();
    });
    let started_at = Instant::now();
    assert_eq!(
        run_provider(
            cancelled_server.base_url(),
            cancellation,
            Duration::from_secs(1),
        ),
        Err(HeadlessTurnPortError::Cancelled)
    );
    assert!(started_at.elapsed() < Duration::from_millis(200));
    cancellation_thread.join().expect("canceller should finish");
    assert_eq!(cancelled_server.join(), 1);

    let deadline_server =
        RetryResponsesServer::start(vec![RetryResponse::StatusWithRetryAfter(429, "5")]);
    let deadline_timeout = Duration::from_millis(150);
    let (cancellation, clock) =
        HeadlessTurnCancellation::with_manual_deadline_for_test(deadline_timeout);
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let diagnostics = ProviderDiagnostics::new(
        "abc12345",
        ProviderDiagnosticScope::Parent,
        Arc::new(move |event: ProviderDiagnosticEvent| {
            if event.event == ProviderDiagnosticKind::RetryScheduled {
                clock.advance(deadline_timeout);
            }
            captured.lock().expect("event lock").push(event);
        }),
    )
    .expect("diagnostics should be configured");
    let started_at = Instant::now();

    assert_eq!(
        run_provider_with_diagnostics(
            deadline_server.base_url(),
            &cancellation,
            Duration::from_secs(2),
            diagnostics,
        ),
        Err(HeadlessTurnPortError::TimedOut)
    );
    assert!(started_at.elapsed() < Duration::from_secs(2));

    let events = events.lock().expect("event lock");
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].event, ProviderDiagnosticKind::Attempt);
    assert_eq!(events[1].event, ProviderDiagnosticKind::RetryScheduled);
    assert_eq!(events[1].class, Some(ProviderDiagnosticClass::RateLimited));
    assert_eq!(events[1].delay_ms, Some(5000));
    assert_eq!(events[2].event, ProviderDiagnosticKind::Terminal);
    assert_eq!(events[2].class, Some(ProviderDiagnosticClass::Deadline));
    drop(events);
    assert_eq!(deadline_server.join(), 1);
}

#[test]
fn tool_enabled_initial_request_uses_flat_function_tool_json() {
    let mut server = LocalResponsesServer::start_scripted(vec![
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_initial\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_initial\"}}\n\n"
        )
        .to_owned(),
    ]);
    let observed_body = server.take_observed_body();
    let tool = OpenAiFunctionTool::new(
        "lookup_weather",
        "Looks up current weather.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
    )
    .expect("tool should be valid");
    let mut provider = OpenAiResponsesProvider::from_api_key_with_tools_and_timeout(
        "test-api-key".into(),
        Some(&server.base_url()),
        "test-model".into(),
        "test prompt".into(),
        vec![tool],
        Duration::from_secs(1),
    )
    .expect("provider should be configured");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should build");

    runtime
        .block_on(provider.next_parts(&[], &HeadlessTurnCancellation::new()))
        .expect("initial response should complete");

    assert_eq!(
        observed_body
            .recv_timeout(Duration::from_secs(1))
            .expect("server should capture initial request"),
        json!({
            "model": "test-model",
            "input": [{"role": "user", "content": "test prompt"}],
            "tools": [{
                "type": "function",
                "name": "lookup_weather",
                "description": "Looks up current weather.",
                "parameters": {"type": "object", "properties": {}, "additionalProperties": false},
                "strict": true,
            }],
            "parallel_tool_calls": true,
            "stream": true,
        })
    );
    server.join();
}

#[test]
fn reasoning_effort_is_sent_only_when_configured() {
    for (config, expected) in [
        (RequestConfig::default(), None),
        (
            RequestConfig::with_reasoning_effort("max").expect("effort should be valid"),
            Some(json!({"effort": "max"})),
        ),
    ] {
        let mut server =
            LocalResponsesServer::start_scripted(vec![completed_text_response("resp", "done")]);
        let observed_body = server.take_observed_body();
        let mut provider = OpenAiResponsesProvider::from_api_key_with_timeout(
            "test-api-key".into(),
            Some(&server.base_url()),
            "test-model".into(),
            "test prompt".into(),
            Duration::from_secs(1),
        )
        .expect("provider should be configured")
        .with_request_config(config);

        provider_runtime()
            .block_on(provider.next_parts(&[], &HeadlessTurnCancellation::new()))
            .expect("response should complete");

        assert_eq!(
            observed_body
                .recv_timeout(Duration::from_secs(1))
                .expect("request should be observed")
                .get("reasoning"),
            expected.as_ref()
        );
        server.join();
    }
}

#[test]
fn sends_ordered_tool_outputs_in_a_second_responses_request() {
    let mut server = LocalResponsesServer::start_scripted(vec![
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_initial\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_first\",\"call_id\":\"call_first\",\"name\":\"first\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_second\",\"call_id\":\"call_second\",\"name\":\"second\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_second\",\"arguments\":\"{}\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_first\",\"arguments\":\"{}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_initial\"}}\n\n"
        )
        .to_owned(),
        concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_second\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_second\"}}\n\n"
        )
        .to_owned(),
    ]);
    let observed_body = server.take_observed_body();
    let tool = OpenAiFunctionTool::new(
        "lookup_weather",
        "Looks up current weather.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
    )
    .expect("tool should be valid");
    let mut provider = OpenAiResponsesProvider::from_api_key_with_tools_and_timeout(
        "test-api-key".into(),
        Some(&server.base_url()),
        "test-model".into(),
        "test prompt".into(),
        vec![tool],
        Duration::from_secs(1),
    )
    .expect("provider should be configured")
    .with_parallel_tool_calls(false);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should build");
    let cancellation = HeadlessTurnCancellation::new();

    runtime
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("initial tool-call response should complete");
    let parts = runtime
        .block_on(provider.next_parts(
            &[
                TurnEvent::ToolResult(agens_core::MessagePart::ToolResult {
                    tool_call_id: "call_second".to_owned(),
                    content: "second result".to_owned(),
                    is_error: false,
                }),
                TurnEvent::ToolResult(agens_core::MessagePart::ToolResult {
                    tool_call_id: "call_first".to_owned(),
                    content: "first result".to_owned(),
                    is_error: false,
                }),
            ],
            &cancellation,
        ))
        .expect("continuation should complete");

    assert_eq!(
        parts,
        vec![agens_core::MessagePart::Text("done".to_owned())]
    );
    assert_eq!(
        observed_body
            .recv_timeout(Duration::from_secs(1))
            .expect("server should capture initial request")["parallel_tool_calls"],
        false
    );
    assert_eq!(
        observed_body
            .recv_timeout(Duration::from_secs(1))
            .expect("server should capture continuation request"),
        json!({
            "model": "test-model",
            "previous_response_id": "resp_initial",
            "input": [
                {"type": "function_call_output", "call_id": "call_first", "output": "first result"},
                {"type": "function_call_output", "call_id": "call_second", "output": "second result"},
            ],
            "tools": [{
                "type": "function",
                "name": "lookup_weather",
                "description": "Looks up current weather.",
                "parameters": {"type": "object", "properties": {}, "additionalProperties": false},
                "strict": true,
            }],
            "parallel_tool_calls": false,
            "stream": true,
        })
    );
    server.join();
}

#[test]
fn configured_reasoning_effort_is_sent_on_continuation_request() {
    let mut server = LocalResponsesServer::start_scripted(vec![
        tool_call_response("resp_initial", "fc_first", "call_first"),
        completed_text_response("resp_second", "done"),
    ]);
    let observed_body = server.take_observed_body();
    let mut provider = OpenAiResponsesProvider::from_api_key_with_tools_and_timeout(
        "test-api-key".into(),
        Some(&server.base_url()),
        "test-model".into(),
        "test prompt".into(),
        vec![
            OpenAiFunctionTool::new(
                "lookup_weather",
                "Looks up current weather.",
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
            )
            .expect("tool should be valid"),
        ],
        Duration::from_secs(1),
    )
    .expect("provider should be configured")
    .with_request_config(
        RequestConfig::with_reasoning_effort("high").expect("effort should be valid"),
    );
    let runtime = provider_runtime();
    let cancellation = HeadlessTurnCancellation::new();

    runtime
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("initial response should produce a tool call");
    runtime
        .block_on(provider.next_parts(
            &[tool_result("call_first", "first result", false)],
            &cancellation,
        ))
        .expect("continuation should complete");

    let _initial = observed_body
        .recv_timeout(Duration::from_secs(1))
        .expect("server should capture initial request");
    assert_eq!(
        observed_body
            .recv_timeout(Duration::from_secs(1))
            .expect("server should capture continuation request"),
        json!({
            "model": "test-model",
            "previous_response_id": "resp_initial",
            "input": [{"type": "function_call_output", "call_id": "call_first", "output": "first result"}],
            "tools": [{
                "type": "function",
                "name": "lookup_weather",
                "description": "Looks up current weather.",
                "parameters": {"type": "object", "properties": {}, "additionalProperties": false},
                "strict": true,
            }],
            "parallel_tool_calls": true,
            "reasoning": {"effort": "high"},
            "stream": true,
        })
    );
    server.join();
}

/// A failing tool's own output is what the model needs in order to recover, and the dispatcher
/// has already redacted credentials and withheld host paths from it. The continuation therefore
/// carries the sanitized content the turn recorded, not a generic placeholder.
#[test]
fn continues_through_two_tool_rounds_and_forwards_the_sanitized_error_output() {
    let mut server = LocalResponsesServer::start_scripted(vec![
        tool_call_response("resp_first", "fc_first", "call_first"),
        tool_call_response("resp_second", "fc_second", "call_second"),
        completed_text_response("resp_third", "complete"),
    ]);
    let observed_body = server.take_observed_body();
    let mut provider = scripted_provider(server.base_url());
    let runtime = provider_runtime();
    let cancellation = HeadlessTurnCancellation::new();
    let sanitized_failure = "bash: internal failure: secret=[redacted: 6 characters]";
    let first_events = [tool_result("call_first", sanitized_failure, true)];
    let second_events = [
        tool_result("call_first", sanitized_failure, true),
        tool_result("call_second", "second result", false),
    ];

    runtime
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("first tool-call response should complete");
    runtime
        .block_on(provider.next_parts(&first_events, &cancellation))
        .expect("second tool-call response should complete");
    assert_eq!(
        runtime
            .block_on(provider.next_parts(&second_events, &cancellation))
            .expect("third response should complete"),
        vec![agens_core::MessagePart::Text("complete".to_owned())]
    );

    let _initial = observed_body
        .recv_timeout(Duration::from_secs(1))
        .expect("initial body");
    assert_eq!(
        observed_body
            .recv_timeout(Duration::from_secs(1))
            .expect("second body"),
        json!({
            "model": "test-model",
            "previous_response_id": "resp_first",
            "input": [{"type": "function_call_output", "call_id": "call_first", "output": sanitized_failure}],
            "parallel_tool_calls": true,
            "stream": true,
        })
    );
    assert_eq!(
        observed_body
            .recv_timeout(Duration::from_secs(1))
            .expect("third body"),
        json!({
            "model": "test-model",
            "previous_response_id": "resp_second",
            "input": [{"type": "function_call_output", "call_id": "call_second", "output": "second result"}],
            "parallel_tool_calls": true,
            "stream": true,
        })
    );
    server.join();
}

#[test]
fn rejects_missing_duplicate_and_foreign_tool_results_before_a_continuation_request() {
    for events in [
        Vec::new(),
        vec![
            tool_result("call_first", "first", false),
            tool_result("call_first", "again", false),
        ],
        vec![tool_result("foreign", "foreign", false)],
    ] {
        let mut server = LocalResponsesServer::start_scripted(vec![tool_call_response(
            "resp_first",
            "fc_first",
            "call_first",
        )]);
        let observed_body = server.take_observed_body();
        let mut provider = scripted_provider(server.base_url());
        let runtime = provider_runtime();
        let cancellation = HeadlessTurnCancellation::new();

        runtime
            .block_on(provider.next_parts(&[], &cancellation))
            .expect("initial tool-call response should complete");
        assert_eq!(
            runtime.block_on(provider.next_parts(&events, &cancellation)),
            Err(HeadlessTurnPortError::Provider)
        );
        assert!(
            observed_body
                .recv_timeout(Duration::from_secs(1))
                .expect("initial request should be observed")
                .get("input")
                .is_some()
        );
        assert!(
            observed_body
                .recv_timeout(Duration::from_millis(25))
                .is_err()
        );
        server.join();
    }
}

#[test]
fn rejects_reused_response_ids_and_truncated_event_history_before_another_request() {
    let mut duplicate_server = LocalResponsesServer::start_scripted(vec![
        tool_call_response("resp_duplicate", "fc_first", "call_first"),
        tool_call_response("resp_duplicate", "fc_second", "call_second"),
    ]);
    let duplicate_bodies = duplicate_server.take_observed_body();
    let mut duplicate_provider = scripted_provider(duplicate_server.base_url());
    let runtime = provider_runtime();
    let cancellation = HeadlessTurnCancellation::new();

    runtime
        .block_on(duplicate_provider.next_parts(&[], &cancellation))
        .expect("first response should produce a tool call");
    assert_eq!(
        runtime.block_on(duplicate_provider.next_parts(
            &[tool_result("call_first", "first result", false)],
            &cancellation,
        )),
        Err(HeadlessTurnPortError::Provider)
    );
    assert!(
        duplicate_bodies
            .recv_timeout(Duration::from_secs(1))
            .expect("initial request should be observed")
            .get("input")
            .is_some()
    );
    assert!(
        duplicate_bodies
            .recv_timeout(Duration::from_secs(1))
            .expect("continuation request should be observed")
            .get("previous_response_id")
            .is_some()
    );
    assert!(
        duplicate_bodies
            .recv_timeout(Duration::from_millis(25))
            .is_err()
    );
    duplicate_server.join();

    let mut cursor_server = LocalResponsesServer::start_scripted(vec![tool_call_response(
        "resp_cursor",
        "fc_cursor",
        "call_cursor",
    )]);
    let cursor_bodies = cursor_server.take_observed_body();
    let mut cursor_provider = scripted_provider(cursor_server.base_url());

    runtime
        .block_on(cursor_provider.next_parts(
            &[tool_result("previous", "previous result", false)],
            &cancellation,
        ))
        .expect("first response should produce a tool call");
    assert_eq!(
        runtime.block_on(cursor_provider.next_parts(&[], &cancellation)),
        Err(HeadlessTurnPortError::Provider)
    );
    assert!(
        cursor_bodies
            .recv_timeout(Duration::from_secs(1))
            .expect("initial request should be observed")
            .get("input")
            .is_some()
    );
    assert!(
        cursor_bodies
            .recv_timeout(Duration::from_millis(25))
            .is_err()
    );
    cursor_server.join();
}

#[test]
fn continuation_rounds_cancel_or_timeout_during_headers_bodies_and_late_sse_without_replay() {
    for (round, mode, stop) in [
        (2, ContinuationStall::DelayedHeaders, Stop::Cancellation),
        (2, ContinuationStall::StalledBody, Stop::Deadline),
        (2, ContinuationStall::LateEvent, Stop::Cancellation),
        (3, ContinuationStall::DelayedHeaders, Stop::Deadline),
        (3, ContinuationStall::StalledBody, Stop::Cancellation),
        (3, ContinuationStall::LateEvent, Stop::Deadline),
    ] {
        let immediate_responses = match round {
            2 => vec![tool_call_response("resp_first", "fc_first", "call_first")],
            3 => vec![
                tool_call_response("resp_first", "fc_first", "call_first"),
                tool_call_response("resp_second", "fc_second", "call_second"),
            ],
            _ => unreachable!("only second and third rounds are tested"),
        };
        let mut server = LocalResponsesServer::start_scripted_with_stall(immediate_responses, mode);
        let observed_bodies = Arc::new(Mutex::new(server.take_observed_body()));
        let mut provider = scripted_provider(server.base_url());
        let runtime = provider_runtime();
        let setup_cancellation = HeadlessTurnCancellation::new();

        runtime
            .block_on(provider.next_parts(&[], &setup_cancellation))
            .expect("first response should produce a tool call");
        observed_bodies
            .lock()
            .expect("request receiver should remain available")
            .recv_timeout(Duration::from_secs(1))
            .expect("initial request should be observed");

        if round == 3 {
            runtime
                .block_on(provider.next_parts(
                    &[tool_result("call_first", "first result", false)],
                    &setup_cancellation,
                ))
                .expect("second response should produce a tool call");
            observed_bodies
                .lock()
                .expect("request receiver should remain available")
                .recv_timeout(Duration::from_secs(1))
                .expect("second request should be observed");
        }

        let events = if round == 2 {
            vec![tool_result("call_first", "first result", false)]
        } else {
            vec![
                tool_result("call_first", "first result", false),
                tool_result("call_second", "second result", false),
            ]
        };
        let expected_error = match stop {
            Stop::Cancellation => HeadlessTurnPortError::Cancelled,
            Stop::Deadline => HeadlessTurnPortError::TimedOut,
        };
        let cancellation = match stop {
            Stop::Cancellation => HeadlessTurnCancellation::new(),
            Stop::Deadline => HeadlessTurnCancellation::with_deadline(Duration::from_millis(250)),
        };
        let cancellation_thread = matches!(stop, Stop::Cancellation).then(|| {
            let canceller = cancellation.clone();
            let observed_bodies = Arc::clone(&observed_bodies);
            thread::spawn(move || {
                observed_bodies
                    .lock()
                    .expect("request receiver should remain available")
                    .recv_timeout(Duration::from_secs(1))
                    .expect("continuation request should be observed");
                canceller.cancel();
            })
        });

        assert_eq!(
            runtime.block_on(provider.next_parts(&events, &cancellation)),
            Err(expected_error)
        );
        assert_eq!(
            runtime.block_on(provider.next_parts(&events, &HeadlessTurnCancellation::new())),
            Err(HeadlessTurnPortError::Provider)
        );

        if let Some(cancellation_thread) = cancellation_thread {
            cancellation_thread
                .join()
                .expect("cancellation thread should finish");
        } else {
            observed_bodies
                .lock()
                .expect("request receiver should remain available")
                .recv_timeout(Duration::from_secs(1))
                .expect("timed-out continuation request should be observed");
        }
        server.join();
    }
}

fn tool_call_response(response_id: &str, item_id: &str, call_id: &str) -> String {
    format!(
        "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{response_id}\"}}}}\n\n\
data: {{\"type\":\"response.output_item.added\",\"item\":{{\"type\":\"function_call\",\"id\":\"{item_id}\",\"call_id\":\"{call_id}\",\"name\":\"lookup\",\"arguments\":\"\"}}}}\n\n\
data: {{\"type\":\"response.function_call_arguments.done\",\"item_id\":\"{item_id}\",\"arguments\":\"{{}}\"}}\n\n\
data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{response_id}\"}}}}\n\n"
    )
}

/// A stream that opens and is then cut: the lifecycle preamble arrives, no
/// output part is decoded, and no terminal event ever comes.
fn created_only_response(response_id: &str) -> String {
    format!("data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{response_id}\"}}}}\n\n")
}

fn completed_text_response(response_id: &str, text: &str) -> String {
    format!(
        "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{response_id}\"}}}}\n\n\
data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\n\
data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{response_id}\"}}}}\n\n"
    )
}

fn tool_result(call_id: &str, content: &str, is_error: bool) -> TurnEvent {
    TurnEvent::ToolResult(agens_core::MessagePart::ToolResult {
        tool_call_id: call_id.to_owned(),
        content: content.to_owned(),
        is_error,
    })
}

fn scripted_provider(base_url: String) -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&base_url),
        "test-model".into(),
        "test prompt".into(),
        Duration::from_secs(1),
    )
    .expect("provider should be configured")
}

fn provider_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should build")
}

fn run_provider(
    base_url: String,
    cancellation: HeadlessTurnCancellation,
    timeout: Duration,
) -> Result<(), HeadlessTurnPortError> {
    let mut provider = OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&base_url),
        "test-model".into(),
        "test prompt".into(),
        timeout,
    )
    .expect("provider should be configured");
    run_provider_instance(&mut provider, &cancellation)
}

fn run_provider_with_diagnostics(
    base_url: String,
    cancellation: &HeadlessTurnCancellation,
    timeout: Duration,
    diagnostics: ProviderDiagnostics,
) -> Result<(), HeadlessTurnPortError> {
    let mut provider = OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&base_url),
        "test-model".into(),
        "test prompt".into(),
        timeout,
    )
    .expect("provider should be configured")
    .with_diagnostics(diagnostics);
    run_provider_instance(&mut provider, cancellation)
}

fn run_provider_with_retry_policy(
    base_url: String,
    cancellation: &HeadlessTurnCancellation,
    timeout: Duration,
    retry_policy: RetryPolicy,
    diagnostics: Option<ProviderDiagnostics>,
) -> Result<(), HeadlessTurnPortError> {
    let mut provider = OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&base_url),
        "test-model".into(),
        "test prompt".into(),
        timeout,
    )
    .expect("provider should be configured")
    .with_retry_policy(retry_policy);
    if let Some(diagnostics) = diagnostics {
        provider = provider.with_diagnostics(diagnostics);
    }
    run_provider_instance(&mut provider, cancellation)
}

/// A schedule short enough to sleep through in a test, with the same shape as
/// the production one.
fn brisk_retry_policy(max_attempts: usize) -> RetryPolicy {
    RetryPolicy::new(
        max_attempts,
        Duration::from_millis(10),
        Duration::from_millis(40),
        Duration::from_millis(40),
        Duration::from_secs(60),
    )
}

fn run_provider_with_operation_timeout(
    base_url: String,
    cancellation: HeadlessTurnCancellation,
    timeout: Duration,
) -> Result<(), HeadlessTurnPortError> {
    let mut provider = OpenAiResponsesProvider::from_api_key_with_timeout(
        "test-api-key".into(),
        Some(&base_url),
        "test-model".into(),
        "test prompt".into(),
        timeout,
    )
    .expect("provider should be configured")
    .with_operation_timeout(timeout);
    run_provider_instance(&mut provider, &cancellation)
}

fn run_provider_instance(
    provider: &mut OpenAiResponsesProvider,
    cancellation: &HeadlessTurnCancellation,
) -> Result<(), HeadlessTurnPortError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("runtime should build");

    runtime
        .block_on(provider.next_parts(&[], cancellation))
        .map(|_| ())
}

#[derive(Clone, Copy, Debug)]
enum ServerMode {
    StalledConnect,
    DelayedHeaders,
    StalledBody,
    LateEvent,
    MalformedFrame,
    LargeToolEchoFrame,
    OversizedFrame,
    UnterminatedOversizedFrame,
    ErrorBody,
    CancelledError,
}

#[derive(Clone, Copy)]
enum ContinuationStall {
    DelayedHeaders,
    StalledBody,
    LateEvent,
}

#[derive(Clone, Copy)]
enum Stop {
    Cancellation,
    Deadline,
}

struct LocalResponsesServer {
    address: std::net::SocketAddr,
    observed_request: Option<mpsc::Receiver<()>>,
    observed_body: Option<mpsc::Receiver<serde_json::Value>>,
    stop: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

enum RetryResponse {
    Disconnect,
    Status(u16),
    StatusWithRetryAfter(u16, &'static str),
    Sse(String),
}

struct RetryResponsesServer {
    address: std::net::SocketAddr,
    observed_request: Option<mpsc::Receiver<usize>>,
    request_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

impl RetryResponsesServer {
    fn start(responses: Vec<RetryResponse>) -> Self {
        let (listener, address) = bind_pollable_listener();
        let request_count = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_count = Arc::clone(&request_count);
        let worker_stop = Arc::clone(&stop);
        let (observed_sender, observed_request) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut responses = VecDeque::from(responses);
            while !responses.is_empty() {
                let Some(mut stream) = accept_until_stopped(&listener, &worker_stop) else {
                    return;
                };

                if read_request_head(&stream, "/responses") == RequestArrival::ClientCut {
                    continue;
                }

                let request_number = worker_count.fetch_add(1, Ordering::AcqRel) + 1;
                observed_sender
                    .send(request_number)
                    .expect("test should receive request observation");
                match responses.pop_front().expect("response should be available") {
                    RetryResponse::Disconnect => {}
                    RetryResponse::Status(status) => write_to_possibly_gone_client(
                        &mut stream,
                        format!(
                            "HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    ),
                    RetryResponse::StatusWithRetryAfter(status, retry_after) => {
                        write_to_possibly_gone_client(
                            &mut stream,
                            format!(
                                "HTTP/1.1 {status} Test\r\nRetry-After: {retry_after}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                            .as_bytes(),
                        )
                    }
                    RetryResponse::Sse(events) => {
                        write_to_possibly_gone_client(&mut stream, SSE_HEADERS);
                        write_to_possibly_gone_client(&mut stream, events.as_bytes());
                    }
                }
            }
        });

        Self {
            address,
            observed_request: Some(observed_request),
            request_count,
            stop,
            worker,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn take_observed_request(&mut self) -> mpsc::Receiver<usize> {
        self.observed_request
            .take()
            .expect("request observation should only be taken once")
    }

    fn join(self) -> usize {
        self.stop.store(true, Ordering::Release);
        self.worker.join().expect("retry server should finish");
        self.request_count.load(Ordering::Acquire)
    }
}

impl LocalResponsesServer {
    fn start_error_response(status: u16, body: &'static str) -> Self {
        let (listener, address) = bind_pollable_listener();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (observed_sender, observed_request) = mpsc::channel();
        let worker = thread::spawn(move || {
            let Some(mut stream) = accept_until_stopped(&listener, &worker_stop) else {
                return;
            };

            if read_request_head(&stream, "/responses") == RequestArrival::ClientCut {
                return;
            }

            observed_sender
                .send(())
                .expect("test should receive request observation");
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .expect("error response should be written");
        });

        Self {
            address,
            observed_request: Some(observed_request),
            observed_body: None,
            stop,
            worker,
        }
    }

    fn start(mode: ServerMode) -> Self {
        let (listener, address) = bind_pollable_listener();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (observed_sender, observed_request) = mpsc::channel();
        let worker = thread::spawn(move || {
            if matches!(mode, ServerMode::StalledConnect) {
                let mut backlog_fillers = Vec::new();
                let mut backlog_full = false;
                for _ in 0..512 {
                    match TcpStream::connect_timeout(&address, Duration::from_millis(5)) {
                        Ok(stream) => backlog_fillers.push(stream),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                            ) =>
                        {
                            backlog_full = true;
                            break;
                        }
                        Err(error) => panic!("backlog fill should only stop when full: {error}"),
                    }
                }
                assert!(
                    !backlog_fillers.is_empty(),
                    "the local listener should accept at least one queued connect"
                );
                assert!(
                    backlog_full,
                    "the local connect backlog should fill before the request starts"
                );
                thread::sleep(Duration::from_millis(250));
                return;
            }

            let Some(mut stream) = accept_until_stopped(&listener, &worker_stop) else {
                return;
            };

            if read_request_head(&stream, "/responses") == RequestArrival::ClientCut {
                return;
            }

            let observe_request = || {
                observed_sender
                    .send(())
                    .expect("test should receive request observation")
            };

            // `StalledBody` and `LateEvent` are both defined by bytes that reached the client
            // before cancellation, and the test cancels the moment the observation lands.
            // Observing after those bytes are written is what keeps that ordering true;
            // otherwise each write races the cancel and the mode decays into the weaker one
            // above it (`LateEvent` into `StalledBody`, `StalledBody` into `DelayedHeaders`).
            if !matches!(mode, ServerMode::StalledBody | ServerMode::LateEvent) {
                observe_request();
            }

            match mode {
                ServerMode::StalledConnect => {
                    unreachable!("stalled connect returns before handling")
                }
                ServerMode::DelayedHeaders => wait_for_client_close(&stream),
                ServerMode::StalledBody => {
                    write_sse_headers(&mut stream);

                    observe_request();

                    wait_for_client_close(&stream);
                }
                ServerMode::LateEvent => {
                    write_sse_headers(&mut stream);
                    stream
                        .write_all(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"early\"}\n\n")
                        .expect("early event should be written");

                    observe_request();

                    wait_for_client_close(&stream);
                    let _ = stream.write_all(b"data: {\"type\":\"response.completed\"}\n\n");
                }
                ServerMode::MalformedFrame => {
                    write_sse_headers(&mut stream);
                    stream
                        .write_all(b"data: {not-json}\n\n")
                        .expect("malformed frame should be written");
                }
                ServerMode::LargeToolEchoFrame => {
                    write_sse_headers(&mut stream);
                    let frame = format!(
                        "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_1\",\"instructions\":\"{}\"}}}}\n\n",
                        "x".repeat(400 * 1024)
                    );
                    stream
                        .write_all(frame.as_bytes())
                        .expect("large tool echo frame should be written");
                    let _ = stream.write_all(
                        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
                    );
                    let _ = stream.write_all(b"data: {\"type\":\"response.completed\"}\n\n");
                }
                ServerMode::OversizedFrame => {
                    write_sse_headers(&mut stream);
                    let frame = format!(
                        "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}\n\n",
                        "x".repeat(2 * 1024 * 1024)
                    );
                    write_to_possibly_gone_client(&mut stream, frame.as_bytes());
                }
                ServerMode::UnterminatedOversizedFrame => {
                    write_sse_headers(&mut stream);
                    write_to_possibly_gone_client(
                        &mut stream,
                        format!(
                            "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}",
                            "x".repeat(2 * 1024 * 1024)
                        )
                        .as_bytes(),
                    );
                }
                ServerMode::ErrorBody => {
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 500 Internal Server Error\r\nX-Remote-Secret: {SECRET_HEADER_SENTINEL}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{SECRET_BODY_SENTINEL}",
                                SECRET_BODY_SENTINEL.len()
                            )
                            .as_bytes(),
                        )
                        .expect("error response should be written");
                }
                ServerMode::CancelledError => {
                    thread::sleep(Duration::from_millis(25));
                    write_to_possibly_gone_client(
                        &mut stream,
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        });

        Self {
            address,
            observed_request: Some(observed_request),
            observed_body: None,
            stop,
            worker,
        }
    }

    fn start_scripted(responses: Vec<String>) -> Self {
        let (listener, address) = bind_pollable_listener();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (body_sender, observed_body) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut responses = VecDeque::from(responses);
            while !responses.is_empty() {
                let Some(mut stream) = accept_until_stopped(&listener, &worker_stop) else {
                    return;
                };
                let Some(body) = read_responses_request_body(&stream) else {
                    continue;
                };

                body_sender
                    .send(body)
                    .expect("test should receive the request body");

                let response = responses.pop_front().expect("response should be available");
                write_sse_headers(&mut stream);
                stream
                    .write_all(response.as_bytes())
                    .expect("scripted response should be written");
            }
        });

        Self {
            address,
            observed_request: None,
            observed_body: Some(observed_body),
            stop,
            worker,
        }
    }

    fn start_scripted_with_stall(responses: Vec<String>, stall: ContinuationStall) -> Self {
        let (listener, address) = bind_pollable_listener();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (body_sender, observed_body) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut responses = VecDeque::from(responses);
            while !responses.is_empty() {
                let Some(mut stream) = accept_until_stopped(&listener, &worker_stop) else {
                    return;
                };
                let Some(body) = read_responses_request_body(&stream) else {
                    continue;
                };

                body_sender
                    .send(body)
                    .expect("test should receive the request body");

                let response = responses.pop_front().expect("response should be available");
                write_sse_headers(&mut stream);
                stream
                    .write_all(response.as_bytes())
                    .expect("scripted response should be written");
            }

            let Some(mut stream) = accept_until_stopped(&listener, &worker_stop) else {
                return;
            };
            let Some(body) = read_responses_request_body(&stream) else {
                return;
            };

            let observe_continuation = move || {
                body_sender
                    .send(body)
                    .expect("test should receive the continuation request body")
            };

            // The cancelling half of this test cancels the moment the continuation body is
            // observed, so each stall writes the bytes that define it first and only then
            // reports the body; otherwise the write races the cancel and the stall decays
            // into a weaker one. The deadline half stops on its own clock instead, and can
            // therefore be gone before any write lands, so these writes also tolerate a
            // client that already left.
            match stall {
                ContinuationStall::DelayedHeaders => {
                    observe_continuation();

                    wait_for_client_close(&stream);
                }
                ContinuationStall::StalledBody => {
                    write_to_possibly_gone_client(&mut stream, SSE_HEADERS);

                    observe_continuation();

                    wait_for_client_close(&stream);
                }
                ContinuationStall::LateEvent => {
                    write_to_possibly_gone_client(&mut stream, SSE_HEADERS);
                    write_to_possibly_gone_client(
                        &mut stream,
                        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"early\"}\n\n",
                    );

                    observe_continuation();

                    wait_for_client_close(&stream);
                    let _ = stream.write_all(b"data: {\"type\":\"response.completed\"}\n\n");
                }
            }
        });

        Self {
            address,
            observed_request: None,
            observed_body: Some(observed_body),
            stop,
            worker,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn take_observed_request(&mut self) -> mpsc::Receiver<()> {
        self.observed_request
            .take()
            .expect("request observation should only be taken once")
    }

    fn take_observed_body(&mut self) -> mpsc::Receiver<serde_json::Value> {
        self.observed_body
            .take()
            .expect("request body observation should only be taken once")
    }

    fn join(self) {
        self.stop.store(true, Ordering::Release);
        self.worker.join().expect("server worker should finish");
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct ResourceSnapshot {
    tasks: usize,
    file_descriptors: usize,
}

#[cfg(target_os = "linux")]
impl ResourceSnapshot {
    fn capture() -> Self {
        Self {
            tasks: std::fs::read_dir("/proc/self/task")
                .expect("task directory should be readable")
                .count(),
            file_descriptors: std::fs::read_dir("/proc/self/fd")
                .expect("file descriptor directory should be readable")
                .count(),
        }
    }
}

/// The JSON body of a `/responses` request, or `None` when the client cut the
/// connection before the request was complete.
fn read_responses_request_body(stream: &TcpStream) -> Option<serde_json::Value> {
    read_request(stream).map(|request| {
        assert_eq!(request.path, "/responses");
        request.body
    })
}

//! Moonshot provider behaviour against a local HTTP server.
//!
//! The endpoint is the suite's shared support server, scripted with chat
//! completion streams: the two providers agree on framing and disagree on every
//! byte inside a frame, so what is shared is accepting, reading and shutting
//! down the socket, and what stays here is the framing itself.

use std::cell::Cell;
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

mod support;

use support::{ScriptedResponse, ScriptedServer};

use agens_core::{
    HeadlessTurnCancellation, HeadlessTurnPortError, Message, MessagePart, ReasoningEffort,
    RequestConfig, Role, TurnEvent, TurnProvider,
};
use agens_providers::{MoonshotProvider, OpenAiFunctionTool, ProviderFailureDetail, RetryPolicy};
use serde_json::{Value, json};

const SECRET_BODY_SENTINEL: &str = "SENTINEL_REMOTE_ERROR_BODY";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime should build")
}

fn provider(address: &str, tools: Vec<OpenAiFunctionTool>) -> MoonshotProvider {
    MoonshotProvider::from_api_key_with_tools_and_timeout(
        "test-key".to_owned(),
        Some(&format!("http://{address}/v1")),
        "kimi-k3".to_owned(),
        "hello".to_owned(),
        tools,
        Duration::from_secs(5),
    )
    .expect("provider should build")
}

fn weather_tool() -> OpenAiFunctionTool {
    OpenAiFunctionTool::new(
        "get_weather",
        "Get the weather",
        json!({"type": "object", "properties": {"city": {"type": "string"}}}),
    )
    .expect("tool should build")
}

#[test]
fn a_streamed_turn_yields_reasoning_then_text_and_reports_usage() {
    let server = SseServer::start(vec![sse(&[
        json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"reasoning_content": "thinking"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop",
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}}),
    ])]);

    let mut provider = provider(&server.address().to_string(), Vec::new());
    let cancellation = HeadlessTurnCancellation::new();

    let parts = runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("turn should complete");

    assert_eq!(
        parts,
        vec![
            MessagePart::Reasoning("thinking".to_owned()),
            MessagePart::Text("hi".to_owned()),
        ]
    );

    let body = server.take_body();
    assert_eq!(body["model"], json!("kimi-k3"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"]["include_usage"], json!(true));
}

#[test]
fn a_tool_call_turn_replays_its_results_in_the_next_request() {
    let server = SseServer::start(vec![
        sse(&[
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_0",
                "type": "function", "function": {"name": "get_weather", "arguments": ""}}]},
                "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0,
                "function": {"arguments": "{\"city\":\"Paris\"}"}}]}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]),
        sse(&[
            json!({"choices": [{"index": 0, "delta": {"content": "sunny"}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        ]),
    ]);

    let mut provider = provider(&server.address().to_string(), vec![weather_tool()]);
    let cancellation = HeadlessTurnCancellation::new();
    let runtime = runtime();

    let parts = runtime
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("first turn should complete");
    assert_eq!(
        parts,
        vec![MessagePart::ToolCall {
            id: "call_0".to_owned(),
            name: "get_weather".to_owned(),
            input: r#"{"city":"Paris"}"#.to_owned(),
        }]
    );

    let events = vec![TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "call_0".to_owned(),
        content: "sunny".to_owned(),
        is_error: false,
    })];
    let parts = runtime
        .block_on(provider.next_parts(&events, &cancellation))
        .expect("continuation should complete");
    assert_eq!(parts, vec![MessagePart::Text("sunny".to_owned())]);

    let first = server.take_body();
    assert_eq!(first["tools"][0]["function"]["name"], json!("get_weather"));
    assert!(
        first["tools"][0]["function"].get("strict").is_none(),
        "strict must not be sent"
    );

    let second = server.take_body();
    let messages = second["messages"].as_array().expect("messages is an array");
    let assistant = messages
        .iter()
        .find(|message| message["role"] == json!("assistant"))
        .expect("the assistant tool call is replayed");
    assert_eq!(assistant["tool_calls"][0]["id"], json!("call_0"));

    let tool = messages
        .iter()
        .find(|message| message["role"] == json!("tool"))
        .expect("the tool result is replayed");
    assert_eq!(tool["tool_call_id"], json!("call_0"));
    assert_eq!(tool["name"], json!("get_weather"));
    assert_eq!(tool["content"], json!("sunny"));
}

#[test]
fn queue_user_messages_during_awaiting_tool_results_place_user_after_tool_results() {
    let server = SseServer::start(vec![
        sse(&[
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_0",
                "type": "function", "function": {"name": "get_weather", "arguments": ""}}]},
                "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0,
                "function": {"arguments": "{\"city\":\"Paris\"}"}}]}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ]),
        sse(&[
            json!({"choices": [{"index": 0, "delta": {"content": "done"}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        ]),
    ]);

    let mut provider = provider(&server.address().to_string(), vec![weather_tool()]);
    let cancellation = HeadlessTurnCancellation::new();
    let runtime = runtime();

    runtime
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("first round should complete");

    provider
        .queue_user_messages(vec![Message {
            role: Role::User,
            parts: vec![MessagePart::Text("coord".to_owned())],
        }])
        .expect("queueing while awaiting tool results is allowed");

    let events = vec![TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "call_0".to_owned(),
        content: "sunny".to_owned(),
        is_error: false,
    })];
    runtime
        .block_on(provider.next_parts(&events, &cancellation))
        .expect("continuation should complete");

    let _first = server.take_body();
    let second = server.take_body();
    let messages = second["messages"].as_array().expect("messages is an array");

    let assistant_index = messages
        .iter()
        .position(|message| {
            message["role"] == json!("assistant") && message.get("tool_calls").is_some()
        })
        .expect("assistant tool_calls message is present");
    let tool_index = messages
        .iter()
        .position(|message| message["role"] == json!("tool"))
        .expect("tool result is present");
    let coord_index = messages
        .iter()
        .position(|message| {
            message["role"] == json!("user") && message["content"] == json!("coord")
        })
        .expect("coordination user message is present");

    assert!(
        assistant_index < tool_index && tool_index < coord_index,
        "expected assistant(tool_calls) → tool → user(coord), got indices assistant={assistant_index} tool={tool_index} coord={coord_index}; messages={messages:?}"
    );
    assert_eq!(messages[tool_index]["tool_call_id"], json!("call_0"));
    assert_eq!(messages[tool_index]["name"], json!("get_weather"));
}

#[test]
fn partial_tool_results_fail_without_a_second_http_request() {
    let server = SseServer::start(vec![sse(&[
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "call_0", "type": "function", "function": {"name": "get_weather", "arguments": "{}"}},
            {"index": 1, "id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{}"}}
        ]}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ])]);

    let mut provider = provider(&server.address().to_string(), vec![weather_tool()]);
    let cancellation = HeadlessTurnCancellation::new();
    let runtime = runtime();

    let parts = runtime
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("first round should complete");
    assert_eq!(parts.len(), 2);

    let _first = server.take_body();

    let events = vec![TurnEvent::ToolResult(MessagePart::ToolResult {
        tool_call_id: "call_0".to_owned(),
        content: "sunny".to_owned(),
        is_error: false,
    })];
    let error = runtime
        .block_on(provider.next_parts(&events, &cancellation))
        .expect_err("partial tool results must fail before HTTP");

    assert_eq!(error, HeadlessTurnPortError::Provider);
    assert!(
        server.try_take_body().is_none(),
        "a second request must not be sent when tool results are incomplete"
    );
}

#[test]
fn reasoning_effort_reaches_the_wire_for_the_model_that_takes_it() {
    let server = SseServer::start(vec![sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ])]);

    let mut provider = provider(&server.address().to_string(), Vec::new()).with_request_config(
        RequestConfig::with_reasoning_effort(ReasoningEffort::Max.as_str())
            .expect("max is a valid effort"),
    );
    let cancellation = HeadlessTurnCancellation::new();

    runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("turn should complete");

    assert_eq!(server.take_body()["reasoning_effort"], json!("max"));
}

/// The failure is a network one rather than a protocol one: nothing the
/// decoder read was wrong, there was simply less of it than the dialect
/// promises. This stream already showed text, so it is not asked for again.
#[test]
fn a_stream_that_ends_without_a_finish_reason_fails_the_turn() {
    let server = SseServer::start(vec![sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": "partial"}, "finish_reason": null}]}),
    ])]);

    let mut provider = provider(&server.address().to_string(), Vec::new());
    let cancellation = HeadlessTurnCancellation::new();

    let error = runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect_err("a truncated stream must not read as a complete turn");

    assert_eq!(error, HeadlessTurnPortError::ProviderNetwork);
}

#[test]
fn a_context_overflow_rejection_is_classified_as_a_context_error() {
    let server = ErrorServer::start(
        400,
        r#"{"error":{"message":"Your request exceeded model token limit: 262144","type":"invalid_request_error"}}"#,
    );

    let mut provider = provider(&server.address().to_string(), Vec::new());
    let cancellation = HeadlessTurnCancellation::new();

    let error = runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect_err("an overflowing request must fail");

    assert_eq!(error, HeadlessTurnPortError::ProviderContext);
}

#[test]
fn an_unauthorized_response_is_classified_as_authentication() {
    let server = ErrorServer::start(401, r#"{"error":{"message":"invalid key"}}"#);

    let mut provider = provider(&server.address().to_string(), Vec::new());
    let cancellation = HeadlessTurnCancellation::new();

    let error = runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect_err("an unauthorized request must fail");

    assert_eq!(error, HeadlessTurnPortError::Authentication);
}

#[test]
fn a_rejected_request_never_leaks_its_body_into_the_error() {
    let server = ErrorServer::start(
        400,
        concat!(
            r#"{"error":{"message":"bad request "#,
            "SENTINEL_REMOTE_ERROR_BODY",
            r#""}}"#
        ),
    );

    let mut provider = provider(&server.address().to_string(), Vec::new());
    let cancellation = HeadlessTurnCancellation::new();

    let error = runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect_err("a rejected request must fail");

    let rendered = format!("{error:?}");
    assert!(
        !rendered.contains(SECRET_BODY_SENTINEL),
        "the remote body must not reach the error: {rendered}"
    );
}

#[test]
fn a_rejected_request_records_body_status_and_model_for_a_user_visible_sink() {
    let server = ErrorServer::start(
        400,
        r#"{"error":{"code":"model_not_found","message":"The model `kimi-missing` does not exist"}}"#,
    );
    let failure_detail = ProviderFailureDetail::new();
    let mut provider = MoonshotProvider::from_api_key_with_tools_and_timeout(
        "test-key".to_owned(),
        Some(&format!("http://{}/v1", server.address())),
        "kimi-missing".to_owned(),
        "hello".to_owned(),
        Vec::new(),
        Duration::from_secs(5),
    )
    .expect("provider should build")
    .with_failure_detail(failure_detail.clone());
    let cancellation = HeadlessTurnCancellation::new();

    let error = runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect_err("a rejected request must fail");

    assert_eq!(error, HeadlessTurnPortError::ProviderRejected);
    let detail = failure_detail
        .take()
        .expect("a rejected request should record failure detail");
    assert!(detail.contains("400"), "{detail}");
    assert!(detail.contains("kimi-missing"), "{detail}");
    assert!(
        detail.contains("The model `kimi-missing` does not exist"),
        "{detail}"
    );
}

/// One `ProviderFailureDetail` handle is shared by every continuation round of one attempt, and
/// `agens-headless` drains it once per whole attempt. A round that records detail for an incident
/// it then recovers from would leave that text in the handle, so a later, unrelated round's
/// failure would be reported with a cause it never had. Every round therefore starts clean.
#[test]
fn each_continuation_round_starts_from_a_clean_failure_detail_handle() {
    let server = SseServer::start(vec![sse(&[
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_0",
            "type": "function", "function": {"name": "get_weather", "arguments": "{}"}}]},
            "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
    ])]);
    let failure_detail = ProviderFailureDetail::new();
    let mut provider = MoonshotProvider::from_api_key_with_tools_and_timeout(
        "test-key".to_owned(),
        Some(&format!("http://{}/v1", server.address())),
        "kimi-k3".to_owned(),
        "hello".to_owned(),
        vec![weather_tool()],
        Duration::from_secs(5),
    )
    .expect("provider should build")
    .with_failure_detail(failure_detail.clone());
    let cancellation = HeadlessTurnCancellation::new();
    let runtime = runtime();

    runtime
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("the first round should complete");

    // Stands in for a recording site inside the round above whose enclosing round still
    // succeeded, which is the only way stale detail can outlive the round that produced it.
    failure_detail.record("recovered mid-round incident");

    let error = runtime
        .block_on(provider.next_parts(&[], &cancellation))
        .expect_err("a continuation without tool results must fail");

    assert_eq!(error, HeadlessTurnPortError::Provider);
    assert_eq!(failure_detail.take(), None);
}

#[test]
fn a_cancelled_turn_reports_cancellation_rather_than_a_provider_failure() {
    let server = SseServer::start(vec![sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ])]);

    let mut provider = provider(&server.address().to_string(), Vec::new());
    let cancellation = HeadlessTurnCancellation::new();
    cancellation.cancel();

    let error = runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect_err("a cancelled turn must not complete");

    assert_eq!(error, HeadlessTurnPortError::Cancelled);
}

#[test]
fn a_response_that_never_sends_a_first_byte_fails_as_a_network_stall() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("server should bind");
    let address = listener.local_addr().expect("address should be available");
    let worker = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("server should accept a request");
        // Hold the connection open without writing a single response byte:
        // dropping it would fail fast for the wrong reason (a closed socket),
        // which is not the stall this test is about.
        let _held = stream;
        thread::sleep(Duration::from_secs(120));
    });

    // The request timeout stands far above the first-byte window so the only
    // clock that can end this turn is the stall window itself — a shorter
    // request timeout would make this test pass for the wrong reason.
    let mut provider = MoonshotProvider::from_api_key_with_tools_and_timeout(
        "test-key".to_owned(),
        Some(&format!("http://{address}/v1")),
        "kimi-k3".to_owned(),
        "hello".to_owned(),
        Vec::new(),
        Duration::from_secs(600),
    )
    .expect("provider should build");
    let cancellation = HeadlessTurnCancellation::new();

    let started = std::time::Instant::now();
    let error = runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect_err("a response that never starts must not read as a slow turn");
    let elapsed = started.elapsed();

    assert_eq!(error, HeadlessTurnPortError::ProviderNetwork);
    assert!(
        elapsed < Duration::from_secs(110),
        "a stalled response must fail near the first-byte window, not the 600s read timeout: {elapsed:?}"
    );
    drop(worker);
}

#[test]
fn queued_user_messages_join_the_replayed_history() {
    let server = SseServer::start(vec![sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    ])]);

    let mut provider = provider(&server.address().to_string(), Vec::new());
    provider
        .queue_user_messages(vec![Message {
            role: Role::User,
            parts: vec![MessagePart::Text("and also this".to_owned())],
        }])
        .expect("queueing before the first request is allowed");

    let cancellation = HeadlessTurnCancellation::new();
    runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("turn should complete");

    let messages = server.take_body();
    let messages = messages["messages"]
        .as_array()
        .expect("messages is an array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["content"], json!("and also this"));
}

/// A stream cut before anything reached the reader is a dropped connection,
/// not a malformed response, and asking for it again costs nothing: this was
/// the one provider that surfaced it as a failed turn instead.
#[test]
fn moonshot_retries_a_stream_cut_before_it_produced_output() {
    let server = SseServer::start(vec![
        cut_sse(&[
            json!({"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]}),
        ]),
        sse(&[
            json!({"choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        ]),
    ]);

    let mut provider = provider(&server.address().to_string(), Vec::new())
        .with_retry_policy(brisk_retry_policy(4));
    let cancellation = HeadlessTurnCancellation::new();

    let parts = runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect("the retried turn should complete");

    assert_eq!(parts, vec![MessagePart::Text("hi".to_owned())]);
    let _cut = server.take_body();
    assert!(
        server.try_take_body().is_some(),
        "the cut stream must be asked for a second time"
    );
}

/// The retry only holds while nothing has been shown: replaying a stream after
/// its text reached the reader would print that text twice.
#[test]
fn moonshot_does_not_retry_a_stream_cut_after_it_produced_output() {
    let server = SseServer::start(vec![
        cut_sse(&[
            json!({"choices": [{"index": 0, "delta": {"content": "half an answer"}, "finish_reason": null}]}),
        ]),
        sse(&[
            json!({"choices": [{"index": 0, "delta": {"content": "whole"}, "finish_reason": "stop"}]}),
        ]),
    ]);

    let mut provider = provider(&server.address().to_string(), Vec::new())
        .with_retry_policy(brisk_retry_policy(4));
    let cancellation = HeadlessTurnCancellation::new();

    let error = runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect_err("a cut that already showed text must not be replayed");

    assert_eq!(error, HeadlessTurnPortError::ProviderNetwork);
    let _cut = server.take_body();
    assert!(
        server.try_take_body().is_none(),
        "a stream that produced output must not be asked for again"
    );
}

/// A stream cut every time still ends, and it ends against the attempt budget
/// rather than against the reader's patience.
#[test]
fn moonshot_stops_retrying_a_stream_cut_at_the_attempt_budget() {
    let cut = || {
        cut_sse(&[
            json!({"choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}]}),
        ])
    };
    let server = SseServer::start(vec![cut(), cut(), cut(), cut()]);

    let mut provider = provider(&server.address().to_string(), Vec::new())
        .with_retry_policy(brisk_retry_policy(3));
    let cancellation = HeadlessTurnCancellation::new();

    let error = runtime()
        .block_on(provider.next_parts(&[], &cancellation))
        .expect_err("a stream cut every time must end as a network failure");

    assert_eq!(error, HeadlessTurnPortError::ProviderNetwork);
    for _ in 0..3 {
        let _attempt = server.take_body();
    }
    assert!(
        server.try_take_body().is_none(),
        "the attempt budget bounds the retries"
    );
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

/// A stream that ends before it says how the turn ended, which is what a
/// connection cut mid-answer looks like from here.
fn cut_sse(chunks: &[Value]) -> String {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    body
}

fn sse(chunks: &[Value]) -> String {
    let mut body = cut_sse(chunks);
    body.push_str("data: [DONE]\n\n");
    body
}

/// A local endpoint scripted with one chat-completion stream per turn.
///
/// The shared support server answers the rounds and captures the requests; this
/// only remembers which of them the test has already looked at.
struct SseServer {
    inner: ScriptedServer,
    taken: Cell<usize>,
}

impl SseServer {
    fn start(responses: Vec<String>) -> Self {
        Self {
            inner: ScriptedServer::start(
                responses.into_iter().map(ScriptedResponse::Sse).collect(),
            ),
            taken: Cell::new(0),
        }
    }

    fn address(&self) -> std::net::SocketAddr {
        self.inner.address()
    }

    fn take_body(&self) -> Value {
        let index = self.taken.get();
        self.taken.set(index + 1);

        self.inner.wait_for_request(index).body
    }

    fn try_take_body(&self) -> Option<Value> {
        let index = self.taken.get();
        self.inner.request(index).inspect(|_| {
            self.taken.set(index + 1);
        })?;

        self.inner.request(index).map(|request| request.body)
    }
}

/// A local endpoint that rejects every turn with the same status and body.
struct ErrorServer {
    inner: ScriptedServer,
}

impl ErrorServer {
    fn start(status: u16, body: &str) -> Self {
        Self {
            inner: ScriptedServer::start(vec![ScriptedResponse::Json(status, body.to_owned())]),
        }
    }

    fn address(&self) -> std::net::SocketAddr {
        self.inner.address()
    }
}

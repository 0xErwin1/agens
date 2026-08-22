//! The scripted provider's own contract, exercised over a real socket.
//!
//! These drive the server with a hand-written HTTP client rather than with a
//! production provider: `agens-fixtures` may not reach `agens-providers`, and
//! the properties under test here — keep-alive, dialect framing, capture, lane
//! routing, script exhaustion — are properties of the server, not of any one
//! client. The end-to-end proof that a real provider understands these bytes
//! is the journey suite in `agens-cli`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use agens_fixtures::{Script, ScriptedDialect, ScriptedProvider, ScriptedTurn};

/// A minimal HTTP client that keeps its connection open across requests, so a
/// test can tell "served two requests" from "accepted twice".
struct KeepAliveClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl KeepAliveClient {
    fn connect(provider: &ScriptedProvider) -> Self {
        let stream = TcpStream::connect(provider.address())
            .expect("client should reach the scripted server");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("client read timeout should be set");

        Self {
            reader: BufReader::new(stream.try_clone().expect("client stream should clone")),
            writer: stream,
        }
    }

    /// Sends one request and asserts the server closed without answering.
    fn post_expecting_no_reply(&mut self, target: &str, body: &str) {
        self.write_request(target, body);

        let mut status_line = String::new();
        assert_eq!(
            self.reader
                .read_line(&mut status_line)
                .expect("the closed connection should read cleanly"),
            0,
            "an unscripted request should get no reply at all, got {status_line:?}"
        );
    }

    fn write_request(&mut self, target: &str, body: &str) {
        self.writer
            .write_all(
                format!(
                    "POST {target} HTTP/1.1\r\nHost: scripted\r\nAuthorization: Bearer fixture\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("client request should be written");
    }

    /// Sends one request and returns the response's status line and body.
    fn post(&mut self, target: &str, body: &str) -> (String, String) {
        self.write_request(target, body);

        let mut status_line = String::new();
        self.reader
            .read_line(&mut status_line)
            .expect("response status line should be readable");

        let mut content_length = None;
        loop {
            let mut header = String::new();
            self.reader
                .read_line(&mut header)
                .expect("response header should be readable");
            if header == "\r\n" {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("content length should be numeric"),
                );
            }
        }

        let payload = match content_length {
            Some(length) => {
                let mut payload = vec![0_u8; length];
                self.reader
                    .read_exact(&mut payload)
                    .expect("response body should be readable");
                payload
            }
            None => {
                let mut payload = Vec::new();
                self.reader
                    .read_to_end(&mut payload)
                    .expect("response body should be readable to close");
                payload
            }
        };

        (
            status_line.trim_end().to_owned(),
            String::from_utf8(payload).expect("response body should be UTF-8"),
        )
    }
}

#[test]
fn one_connection_serves_every_scripted_turn_in_order() {
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([
            ScriptedTurn::tool_call("call-1", "read", r#"{"path":"notes.md"}"#),
            ScriptedTurn::text("done"),
        ]),
    );

    let mut client = KeepAliveClient::connect(&provider);
    let (first_status, first_body) = client.post("/responses", r#"{"input":"first"}"#);
    let (second_status, second_body) = client.post("/responses", r#"{"input":"second"}"#);

    assert_eq!(first_status, "HTTP/1.1 200 OK");
    assert_eq!(second_status, "HTTP/1.1 200 OK");
    assert!(
        first_body.contains(r#""call_id":"call-1""#),
        "first turn should be the scripted tool call: {first_body}"
    );
    assert!(
        second_body.contains(r#""delta":"done""#),
        "second turn should be the scripted text: {second_body}"
    );
    provider.assert_script_consumed();
}

#[test]
fn every_request_is_captured_with_its_target_headers_and_body() {
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([ScriptedTurn::text("ok")]),
    );

    KeepAliveClient::connect(&provider).post("/responses", r#"{"input":"what did I send"}"#);

    let requests = provider.wait_for_requests(1);
    let request = &requests[0];
    assert_eq!(request.method(), "POST");
    assert_eq!(request.target(), "/responses");
    assert_eq!(request.header("authorization"), Some("Bearer fixture"));
    assert_eq!(request.json()["input"], "what did I send");
    assert!(!request.is_child());
    provider.assert_script_consumed();
}

#[test]
fn the_chat_completions_dialect_frames_the_same_script_as_chat_completion_chunks() {
    let provider = ScriptedProvider::start(
        ScriptedDialect::ChatCompletions,
        Script::new([
            ScriptedTurn::tool_call("call_0", "get_weather", r#"{"city":"Paris"}"#),
            ScriptedTurn::text("sunny"),
        ]),
    );

    let mut client = KeepAliveClient::connect(&provider);
    let (_, tool_call) = client.post("/v1/chat/completions", r#"{"messages":[]}"#);
    let (_, text) = client.post("/v1/chat/completions", r#"{"messages":[]}"#);

    assert!(
        tool_call.contains(r#""id":"call_0""#)
            && tool_call.contains(r#""finish_reason":"tool_calls""#),
        "tool call should stream as chat completion chunks: {tool_call}"
    );
    assert!(
        text.contains(r#""content":"sunny""#) && text.contains(r#""finish_reason":"stop""#),
        "text should stream as chat completion chunks: {text}"
    );
    for body in [&tool_call, &text] {
        assert!(
            body.ends_with("data: [DONE]\n\n"),
            "chat completion streams terminate with [DONE]: {body}"
        );
    }
    provider.assert_script_consumed();
}

#[test]
fn a_request_carrying_the_delegation_marker_is_served_the_child_script() {
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([ScriptedTurn::text("parent answer")])
            .with_child("review this", [ScriptedTurn::text("child answer")]),
    );

    let mut client = KeepAliveClient::connect(&provider);
    let (_, child) = client.post("/responses", r#"{"input":"review this file"}"#);
    let (_, parent) = client.post("/responses", r#"{"input":"unrelated"}"#);

    assert!(
        child.contains(r#""delta":"child answer""#),
        "the marked request should get the child script: {child}"
    );
    assert!(
        parent.contains(r#""delta":"parent answer""#),
        "an unmarked request should get the main script: {parent}"
    );
    assert_eq!(provider.child_requests().len(), 1);
    provider.assert_script_consumed();
}

/// The `/responses` dialect drops the history in favour of
/// `previous_response_id`, so a child's second turn no longer carries the
/// delegation prompt. Routing it by the marker alone would hand it back to the
/// main script and silently swap the two conversations.
#[test]
fn a_child_continuation_stays_in_the_child_script_without_repeating_the_marker() {
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([ScriptedTurn::text("parent answer")]).with_child(
            "review this",
            [
                ScriptedTurn::tool_call("child-read", "read", r#"{"path":"notes.md"}"#),
                ScriptedTurn::text("child answer"),
            ],
        ),
    );

    let mut client = KeepAliveClient::connect(&provider);
    client.post("/responses", r#"{"input":"review this file"}"#);
    let (_, continuation) = client.post(
        "/responses",
        r#"{"previous_response_id":"response_child-read","input":[{"call_id":"child-read","output":"read"}]}"#,
    );
    let (_, parent) = client.post("/responses", r#"{"input":"unrelated"}"#);

    assert!(
        continuation.contains(r#""delta":"child answer""#),
        "the continuation should stay in the child script: {continuation}"
    );
    assert!(
        parent.contains(r#""delta":"parent answer""#),
        "the main script should still be waiting for its own turn: {parent}"
    );
    assert_eq!(provider.child_requests().len(), 2);
    provider.assert_script_consumed();
}

#[test]
#[should_panic(expected = "script was not consumed")]
fn a_loop_that_stops_early_leaves_the_script_unconsumed() {
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([
            ScriptedTurn::text("first"),
            ScriptedTurn::text("never reached"),
        ]),
    );

    KeepAliveClient::connect(&provider).post("/responses", r#"{"input":"only one"}"#);
    provider.wait_for_requests(1);

    provider.assert_script_consumed();
}

#[test]
#[should_panic(expected = "script did not cover")]
fn a_loop_that_runs_past_the_script_is_reported_rather_than_answered() {
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([ScriptedTurn::text("only")]),
    );

    let mut client = KeepAliveClient::connect(&provider);
    client.post("/responses", r#"{"input":"first"}"#);
    client.post_expecting_no_reply("/responses", r#"{"input":"one too many"}"#);

    provider.wait_for_requests(2);
    provider.assert_script_consumed();
}

#[test]
fn a_truncated_turn_closes_the_stream_without_its_terminating_event() {
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([ScriptedTurn::truncate()]),
    );

    let (status, body) =
        KeepAliveClient::connect(&provider).post("/responses", r#"{"input":"cut me"}"#);

    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(
        body.contains("response.output_text.delta") && !body.contains("response.completed"),
        "a truncated stream carries partial output and no completion: {body}"
    );
    provider.assert_script_consumed();
}

#[test]
fn an_error_turn_answers_with_its_status_and_body_verbatim() {
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([ScriptedTurn::error(429, r#"{"error":"slow down"}"#)]),
    );

    let (status, body) =
        KeepAliveClient::connect(&provider).post("/responses", r#"{"input":"too fast"}"#);

    assert_eq!(status, "HTTP/1.1 429 Scripted");
    assert_eq!(body, r#"{"error":"slow down"}"#);
    provider.assert_script_consumed();
}

#[test]
fn the_written_configuration_points_at_the_running_server() {
    let temporary = agens_fixtures::session_directory("scripted-provider-configuration");
    let config_home = temporary.join("config");
    let data_directory = temporary.join("data");
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([ScriptedTurn::text("ok")]),
    );

    provider.write_configuration(&config_home, &data_directory, "[permissions]\nallow = []\n");

    let configuration = std::fs::read_to_string(config_home.join("config.toml"))
        .expect("journey configuration should exist");
    assert!(configuration.contains(&format!("base_url = \"{}\"", provider.base_url())));
    assert!(configuration.contains("type = \"openai-api\""));
    assert!(configuration.contains("[permissions]"));
    assert!(
        std::fs::read_to_string(config_home.join("auth.json"))
            .expect("journey credentials should exist")
            .contains("openai-api")
    );

    let _ = std::fs::remove_dir_all(&temporary);
}

#[test]
fn a_stalled_turn_sends_headers_and_then_nothing_until_it_closes() {
    let stall = Duration::from_millis(100);
    let provider = ScriptedProvider::start(
        ScriptedDialect::Responses,
        Script::new([ScriptedTurn::stall(stall)]),
    );

    let started = std::time::Instant::now();
    let (status, body) =
        KeepAliveClient::connect(&provider).post("/responses", r#"{"input":"wait"}"#);

    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(body, "");
    assert!(
        started.elapsed() >= stall,
        "a stalled turn holds the stream open for its whole duration"
    );
    provider.assert_script_consumed();
}

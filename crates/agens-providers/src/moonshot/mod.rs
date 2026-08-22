//! The Moonshot AI provider: OpenAI's chat-completions dialect over
//! `api.moonshot.ai`.
//!
//! It sits beside the responses-API providers rather than on top of them
//! because the two dialects disagree about who holds the conversation. The
//! responses API keeps the thread server-side and continues it by id; here the
//! full history is replayed on every call, so tool results are ordinary
//! messages rather than outputs attached to a previous response.
//!
//! Everything below the dialect — retry and backoff, cancellation, the
//! diagnostics record, and how an HTTP status becomes a turn error — is the
//! crate's existing machinery, reached directly rather than reimplemented.

mod compat;
mod decode;
mod encode;

use std::time::Duration;

use agens_core::{
    HeadlessTurnCancellation, HeadlessTurnPortError, Message, MessagePart, RequestConfig, Role,
    TurnEvent, TurnProgressSink, TurnProvider, Usage,
};
use serde_json::Value;

use crate::{
    Error, MAX_SSE_FRAME_BYTES, MediaBlobs, OpenAiFunctionTool, ProgressAwareProvider,
    ProviderDiagnosticComponent, ProviderDiagnostics, ProviderFailureDetail, RetryLoop,
    RetryPolicy, classify_openai_response_status, diagnostic_class_for_port_error,
    diagnostic_class_for_status, provider_operation_cancellation, stop_before_mapping,
    wait_for_stop,
};

use decode::CompletionsDecoder;
use encode::RequestOptions;

const DEFAULT_MOONSHOT_BASE_URL: &str = "https://api.moonshot.ai/v1";
const DEFAULT_MOONSHOT_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

enum TurnState {
    /// No request has been sent yet.
    Initial,
    /// The model asked for tool calls; the next call replays their results.
    ///
    /// User messages that arrive while tools are outstanding must not enter
    /// `history` yet: chat-completions requires `role=tool` messages to sit
    /// immediately after the assistant `tool_calls` message. They wait in
    /// `pending_user_messages` and are appended only after the tool batch.
    AwaitingToolResults {
        event_cursor: usize,
        /// `(call_id, tool_name)` in the order the assistant emitted them.
        pending_calls: Vec<(String, String)>,
        pending_user_messages: Vec<Message>,
    },
    Completed,
    Failed,
}

pub struct MoonshotProvider {
    api_key: String,
    base_url: String,
    model: String,
    tools: Vec<OpenAiFunctionTool>,
    parallel_tool_calls: bool,
    request_config: RequestConfig,
    history: Vec<Message>,
    state: TurnState,
    client: reqwest::Client,
    operation_timeout: Duration,
    retry_policy: RetryPolicy,
    diagnostics: Option<ProviderDiagnostics>,
    progress: Option<TurnProgressSink>,
    failure_detail: Option<ProviderFailureDetail>,
    media_blobs: MediaBlobs,
}

impl MoonshotProvider {
    pub fn from_api_key(
        api_key: String,
        base_url: Option<&str>,
        model: String,
        prompt: String,
    ) -> Result<Self, Error> {
        Self::from_api_key_with_tools_and_timeout(
            api_key,
            base_url,
            model,
            prompt,
            Vec::new(),
            DEFAULT_MOONSHOT_REQUEST_TIMEOUT,
        )
    }

    pub fn from_api_key_with_tools_and_timeout(
        api_key: String,
        base_url: Option<&str>,
        model: String,
        prompt: String,
        tools: Vec<OpenAiFunctionTool>,
        request_timeout: Duration,
    ) -> Result<Self, Error> {
        Self::from_api_key_with_messages_and_tools_and_timeout(
            api_key,
            base_url,
            model,
            vec![Message {
                role: Role::User,
                parts: vec![MessagePart::Text(prompt)],
            }],
            tools,
            request_timeout,
        )
    }

    pub fn from_api_key_with_messages_and_tools_and_timeout(
        api_key: String,
        base_url: Option<&str>,
        model: String,
        history: Vec<Message>,
        tools: Vec<OpenAiFunctionTool>,
        request_timeout: Duration,
    ) -> Result<Self, Error> {
        if api_key.trim().is_empty() || model.trim().is_empty() || history.is_empty() {
            return Err(Error::Auth(
                "Moonshot AI authentication is unavailable".into(),
            ));
        }

        Ok(Self {
            api_key,
            base_url: base_url
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(DEFAULT_MOONSHOT_BASE_URL)
                .trim_end_matches('/')
                .to_owned(),
            model,
            tools,
            parallel_tool_calls: true,
            request_config: RequestConfig::default(),
            history,
            state: TurnState::Initial,
            client: reqwest::Client::builder()
                .read_timeout(request_timeout)
                .build()
                .map_err(|_| Error::Provider("Moonshot client is unavailable".to_owned()))?,
            operation_timeout: crate::DEFAULT_PROVIDER_REQUEST_TIMEOUT,
            retry_policy: RetryPolicy::default(),
            diagnostics: None,
            progress: None,
            failure_detail: None,
            media_blobs: MediaBlobs::new(),
        })
    }

    #[must_use]
    pub fn with_media_blobs(mut self, media_blobs: MediaBlobs) -> Self {
        self.media_blobs = media_blobs;
        self
    }

    pub fn with_operation_timeout(mut self, operation_timeout: Duration) -> Self {
        self.operation_timeout = operation_timeout;
        self
    }

    #[must_use]
    pub fn with_request_config(mut self, request_config: RequestConfig) -> Self {
        self.request_config = request_config;
        self
    }

    #[must_use]
    pub fn with_parallel_tool_calls(mut self, parallel_tool_calls: bool) -> Self {
        self.parallel_tool_calls = parallel_tool_calls;
        self
    }

    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: ProviderDiagnostics) -> Self {
        self.diagnostics = Some(diagnostics.with_max_attempts(self.retry_policy.max_attempts()));
        self
    }

    #[must_use]
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self.diagnostics = self
            .diagnostics
            .map(|diagnostics| diagnostics.with_max_attempts(retry_policy.max_attempts()));
        self
    }

    #[must_use]
    pub fn with_failure_detail(mut self, failure_detail: ProviderFailureDetail) -> Self {
        self.failure_detail = Some(failure_detail);
        self
    }

    fn payload(&self) -> Result<Value, HeadlessTurnPortError> {
        encode::validate_chat_completions_history(&self.history, &self.media_blobs)
            .map_err(|_| HeadlessTurnPortError::ProviderProtocol)?;

        encode::encode_request(
            &self.history,
            &RequestOptions {
                model: &self.model,
                tools: &self.tools,
                parallel_tool_calls: self.parallel_tool_calls,
                reasoning_effort: self.request_config.reasoning_effort(),
            },
            &self.media_blobs,
        )
        .map_err(|_| HeadlessTurnPortError::Provider)
    }

    /// Sends one request, bounding the wait for its first sign of life.
    ///
    /// `client.execute` resolves as soon as the response headers arrive, so the
    /// request's read timeout — which bounds a stream that is already producing
    /// bytes — never starts on a connection that accepts and then stays silent.
    /// That silence is the stall this bounds: `FIRST_RESPONSE_BYTE_TIMEOUT`
    /// covers headers plus the first body byte, and `read_stream` reuses the
    /// same deadline so the window is not spent twice.
    async fn send(
        &self,
        payload: Value,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<(reqwest::Response, tokio::time::Instant), HeadlessTurnPortError> {
        let first_byte_deadline = tokio::time::Instant::now() + crate::FIRST_RESPONSE_BYTE_TIMEOUT;
        let mut retry = RetryLoop::new(
            self.retry_policy,
            self.diagnostics.as_ref(),
            ProviderDiagnosticComponent::ChatCompletions,
        );

        loop {
            stop_before_mapping(cancellation)?;
            retry.begin_attempt();

            let request = self
                .client
                .post(format!("{}/chat/completions", self.base_url))
                .bearer_auth(&self.api_key)
                .header("Accept", "text/event-stream")
                .json(&payload)
                .build()
                .map_err(|_| HeadlessTurnPortError::ProviderProtocol)?;

            let response = tokio::select! {
                response = self.client.execute(request) => {
                    stop_before_mapping(cancellation)?;
                    response
                }
                stop = wait_for_stop(cancellation) => return Err(stop),
                () = tokio::time::sleep_until(first_byte_deadline) => {
                    crate::emit_first_byte_stall(
                        self.diagnostics.as_ref(),
                        ProviderDiagnosticComponent::ChatCompletions,
                    );
                    return Err(HeadlessTurnPortError::ProviderNetwork);
                }
            };

            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    if retry.retry_transport_error(&error, cancellation).await? {
                        continue;
                    }

                    let last_transient_status = retry.last_transient_status();
                    let error = last_transient_status
                        .map(|status| classify_openai_response_status(status, false))
                        .unwrap_or(HeadlessTurnPortError::ProviderNetwork);
                    retry.emit_terminal(
                        last_transient_status,
                        Some(diagnostic_class_for_port_error(error)),
                    );
                    return Err(error);
                }
            };

            let Some(response) = retry.retry_transient_status(response, cancellation).await? else {
                continue;
            };
            let status = response.status().as_u16();

            if !response.status().is_success() {
                if let Some(failure_detail) = &self.failure_detail {
                    failure_detail
                        .record(&format!("HTTP {status} rejected model \"{}\"", self.model));
                }
                let context_exceeded =
                    read_context_overflow(response, cancellation, self.failure_detail.as_ref())
                        .await?;
                retry.emit_terminal(
                    Some(status),
                    Some(diagnostic_class_for_status(status, context_exceeded)),
                );
                return Err(classify_openai_response_status(status, context_exceeded));
            }

            return Ok((response, first_byte_deadline));
        }
    }

    /// Reads the streamed response into the parts of one assistant turn.
    ///
    /// Frame assembly mirrors the responses-API reader beside it rather than
    /// sharing it: the two agree on SSE framing and disagree on everything
    /// inside a frame, and generalizing the loop would mean editing a provider
    /// this change is not otherwise touching.
    ///
    /// The first chunk is bounded by the `first_byte_deadline` `send` already
    /// started, so the window covers the response headers plus the first body
    /// byte exactly once. Once the stream has started, the request's read
    /// timeout resumes bounding it.
    async fn read_stream(
        &self,
        mut response: reqwest::Response,
        first_byte_deadline: tokio::time::Instant,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<(Vec<MessagePart>, Option<Usage>, bool), HeadlessTurnPortError> {
        let mut decoder = CompletionsDecoder::new();
        let mut frame = Vec::new();
        let mut first_byte_deadline = Some(first_byte_deadline);

        loop {
            let next_chunk = tokio::select! {
                chunk = response.chunk() => {
                    stop_before_mapping(cancellation)?;
                    chunk.map_err(|error| {
                        crate::stream_read_failure(&error, HeadlessTurnPortError::ProviderProtocol)
                    })?
                }
                stop = wait_for_stop(cancellation) => return Err(stop),
                () = async {
                    match first_byte_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    crate::emit_first_byte_stall(
                        self.diagnostics.as_ref(),
                        ProviderDiagnosticComponent::ChatCompletions,
                    );
                    return Err(HeadlessTurnPortError::ProviderNetwork);
                }
            };

            first_byte_deadline = None;

            let Some(chunk) = next_chunk else {
                stop_before_mapping(cancellation)?;
                let usage = decoder.usage();
                let wants_tool_results = decoder.wants_tool_results();
                return Ok((decoder.finish()?, usage, wants_tool_results));
            };

            for byte in chunk {
                if byte == b'\n' {
                    accept_frame(&mut decoder, &frame, self.progress.as_ref())?;
                    frame.clear();
                    stop_before_mapping(cancellation)?;
                    continue;
                }

                if frame.len() == MAX_SSE_FRAME_BYTES {
                    stop_before_mapping(cancellation)?;
                    return Err(HeadlessTurnPortError::ProviderProtocol);
                }
                frame.push(byte);
            }

            stop_before_mapping(cancellation)?;
        }
    }
}

/// Decodes one SSE line into the decoder. Anything that is not a `data:` payload
/// frames the stream rather than describing the turn, and `[DONE]` only closes
/// it.
fn accept_frame(
    decoder: &mut CompletionsDecoder,
    frame: &[u8],
    progress: Option<&TurnProgressSink>,
) -> Result<(), HeadlessTurnPortError> {
    let line = std::str::from_utf8(frame)
        .map_err(|_| HeadlessTurnPortError::ProviderProtocol)?
        .trim();

    let Some(payload) = line.strip_prefix("data:") else {
        return Ok(());
    };
    let payload = payload.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return Ok(());
    }

    let chunk: Value =
        serde_json::from_str(payload).map_err(|_| HeadlessTurnPortError::ProviderProtocol)?;

    if let Some(progress) = progress
        && let Some(text) = chunk
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        && !text.is_empty()
    {
        progress(TurnEvent::ProviderPart(MessagePart::Text(text.to_owned())));
    }

    decoder.accept(&chunk)
}

/// Whether a failed response says the request outran the model's context.
///
/// The body is read through the crate's bounded reader so an error page cannot
/// be streamed into memory. Beyond the context-overflow marker it is
/// otherwise discarded, unless the caller supplies a `ProviderFailureDetail`
/// handle: the raw body then survives there for a user-visible sink. It can
/// echo the request, so it must never reach a model-visible path.
async fn read_context_overflow(
    response: reqwest::Response,
    cancellation: &HeadlessTurnCancellation,
    failure_detail: Option<&ProviderFailureDetail>,
) -> Result<bool, HeadlessTurnPortError> {
    crate::read_safe_context_error(
        response,
        cancellation,
        &[compat::CONTEXT_OVERFLOW_MARKER],
        failure_detail,
    )
    .await
}

/// Correlates ToolResult events with the pending assistant tool calls.
///
/// Every pending `call_id` must appear exactly once; unknown or duplicate ids
/// are rejected before any HTTP request is made. Results are returned in the
/// same order as `pending_calls` so the wire batch matches the assistant turn.
fn correlate_tool_results(
    pending_calls: &[(String, String)],
    events: &[TurnEvent],
) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
    use std::collections::HashMap;

    let mut by_id = HashMap::new();

    for event in events {
        let TurnEvent::ToolResult(part @ MessagePart::ToolResult { tool_call_id, .. }) = event
        else {
            continue;
        };

        if !pending_calls.iter().any(|(id, _)| id == tool_call_id)
            || by_id.contains_key(tool_call_id.as_str())
        {
            return Err(HeadlessTurnPortError::Provider);
        }

        by_id.insert(tool_call_id.as_str(), part.clone());
    }

    if by_id.len() != pending_calls.len() {
        return Err(HeadlessTurnPortError::Provider);
    }

    pending_calls
        .iter()
        .map(|(id, _)| {
            by_id
                .remove(id.as_str())
                .ok_or(HeadlessTurnPortError::Provider)
        })
        .collect()
}

fn pending_calls_from_parts(parts: &[MessagePart]) -> Vec<(String, String)> {
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect()
}

impl TurnProvider for MoonshotProvider {
    fn queue_user_messages(&mut self, messages: Vec<Message>) -> Result<(), HeadlessTurnPortError> {
        match &mut self.state {
            TurnState::Initial => {
                self.history.extend(messages);
                Ok(())
            }
            TurnState::AwaitingToolResults {
                pending_user_messages,
                ..
            } => {
                pending_user_messages.extend(messages);
                Ok(())
            }
            TurnState::Completed | TurnState::Failed => Err(HeadlessTurnPortError::Provider),
        }
    }

    async fn next_parts(
        &mut self,
        events: &[TurnEvent],
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
        let operation_cancellation =
            provider_operation_cancellation(cancellation, self.operation_timeout);
        let cancellation = &operation_cancellation;
        // A prior round in this same turn may have recorded detail for an incident it then
        // recovered from. This handle is shared across every round of one attempt, so draining
        // it here keeps that recovered incident from being mistaken for the cause of a later,
        // unrelated failure in this same turn.
        if let Some(failure_detail) = &self.failure_detail {
            failure_detail.take();
        }
        stop_before_mapping(cancellation)?;

        let state = std::mem::replace(&mut self.state, TurnState::Failed);
        match state {
            TurnState::Initial => {}
            TurnState::AwaitingToolResults {
                event_cursor,
                pending_calls,
                pending_user_messages,
            } => {
                let Some(new_events) = events.get(event_cursor..) else {
                    return Err(HeadlessTurnPortError::Provider);
                };
                let results = correlate_tool_results(&pending_calls, new_events)?;

                self.history.push(Message {
                    role: Role::Tool,
                    parts: results,
                });
                self.history.extend(pending_user_messages);
            }
            TurnState::Completed | TurnState::Failed => {
                return Err(HeadlessTurnPortError::Provider);
            }
        }

        let (response, first_byte_deadline) = self.send(self.payload()?, cancellation).await?;
        let (parts, usage, wants_tool_results) = self
            .read_stream(response, first_byte_deadline, cancellation)
            .await?;

        if let Some(progress) = &self.progress
            && let Some(usage) = usage
        {
            progress(TurnEvent::Usage(usage));
        }

        self.history.push(Message {
            role: Role::Assistant,
            parts: parts.clone(),
        });

        self.state = if wants_tool_results {
            TurnState::AwaitingToolResults {
                event_cursor: events.len(),
                pending_calls: pending_calls_from_parts(&parts),
                pending_user_messages: Vec::new(),
            }
        } else {
            TurnState::Completed
        };

        Ok(parts)
    }
}

impl ProgressAwareProvider for MoonshotProvider {
    fn with_progress_sink(mut self, progress: TurnProgressSink) -> Self {
        self.progress = Some(progress);
        self
    }
}

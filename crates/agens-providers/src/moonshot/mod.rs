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
    Error, MAX_HTTP_REQUEST_ATTEMPTS, MAX_OPENAI_TOOL_CONTINUATION_ROUNDS, MAX_SSE_FRAME_BYTES,
    OpenAiFunctionTool, ProgressAwareProvider, ProviderDiagnosticClass,
    ProviderDiagnosticComponent, ProviderDiagnosticKind, ProviderDiagnostics,
    classify_openai_response_status, diagnostic_class_for_port_error, diagnostic_class_for_status,
    is_transient_http_status, is_transient_transport_error, retry_after_from_headers,
    stop_before_mapping, wait_for_http_retry, wait_for_stop,
};

use decode::CompletionsDecoder;
use encode::RequestOptions;

const DEFAULT_MOONSHOT_BASE_URL: &str = "https://api.moonshot.ai/v1";
const DEFAULT_MOONSHOT_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

enum TurnState {
    /// No request has been sent yet.
    Initial,
    /// The model asked for tool calls; the next call replays their results.
    AwaitingToolResults {
        event_cursor: usize,
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
    continuation_rounds: usize,
    client: reqwest::Client,
    diagnostics: Option<ProviderDiagnostics>,
    progress: Option<TurnProgressSink>,
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
            continuation_rounds: 0,
            client: reqwest::Client::builder()
                .timeout(request_timeout)
                .build()
                .map_err(|_| Error::Provider("Moonshot client is unavailable".to_owned()))?,
            diagnostics: None,
            progress: None,
        })
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
        self.diagnostics = Some(diagnostics);
        self
    }

    fn payload(&self) -> Value {
        encode::encode_request(
            &self.history,
            &RequestOptions {
                model: &self.model,
                tools: &self.tools,
                parallel_tool_calls: self.parallel_tool_calls,
                reasoning_effort: self.request_config.reasoning_effort(),
            },
        )
    }

    fn emit(
        &self,
        kind: ProviderDiagnosticKind,
        attempt: usize,
        status: Option<u16>,
        class: Option<ProviderDiagnosticClass>,
    ) {
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.emit(
                ProviderDiagnosticComponent::ChatCompletions,
                kind,
                attempt,
                None,
                status,
                class,
            );
        }
    }

    async fn send(
        &self,
        payload: Value,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<reqwest::Response, HeadlessTurnPortError> {
        let mut attempt = 0;
        let mut last_transient_status = None;

        loop {
            stop_before_mapping(cancellation)?;
            self.emit(ProviderDiagnosticKind::Attempt, attempt + 1, None, None);

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
            };

            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    if attempt + 1 < MAX_HTTP_REQUEST_ATTEMPTS
                        && is_transient_transport_error(&error)
                    {
                        wait_for_http_retry(
                            cancellation,
                            attempt,
                            None,
                            self.diagnostics.as_ref(),
                            ProviderDiagnosticComponent::ChatCompletions,
                            ProviderDiagnosticClass::Network,
                            None,
                        )
                        .await?;
                        attempt += 1;
                        continue;
                    }

                    let error = last_transient_status
                        .map(|status| classify_openai_response_status(status, false))
                        .unwrap_or(HeadlessTurnPortError::ProviderNetwork);
                    self.emit(
                        ProviderDiagnosticKind::Terminal,
                        attempt + 1,
                        last_transient_status,
                        Some(diagnostic_class_for_port_error(error)),
                    );
                    return Err(error);
                }
            };

            let status = response.status().as_u16();
            if is_transient_http_status(status) && attempt + 1 < MAX_HTTP_REQUEST_ATTEMPTS {
                last_transient_status = Some(status);
                let retry_after = retry_after_from_headers(response.headers());
                drop(response);
                wait_for_http_retry(
                    cancellation,
                    attempt,
                    retry_after,
                    self.diagnostics.as_ref(),
                    ProviderDiagnosticComponent::ChatCompletions,
                    diagnostic_class_for_status(status, false),
                    Some(status),
                )
                .await?;
                attempt += 1;
                continue;
            }

            if !response.status().is_success() {
                let context_exceeded = read_context_overflow(response, cancellation).await?;
                self.emit(
                    ProviderDiagnosticKind::Terminal,
                    attempt + 1,
                    Some(status),
                    Some(diagnostic_class_for_status(status, context_exceeded)),
                );
                return Err(classify_openai_response_status(status, context_exceeded));
            }

            return Ok(response);
        }
    }

    /// Reads the streamed response into the parts of one assistant turn.
    ///
    /// Frame assembly mirrors the responses-API reader beside it rather than
    /// sharing it: the two agree on SSE framing and disagree on everything
    /// inside a frame, and generalizing the loop would mean editing a provider
    /// this change is not otherwise touching.
    async fn read_stream(
        &self,
        mut response: reqwest::Response,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<(Vec<MessagePart>, Option<Usage>, bool), HeadlessTurnPortError> {
        let mut decoder = CompletionsDecoder::new();
        let mut frame = Vec::new();

        loop {
            let next_chunk = tokio::select! {
                chunk = response.chunk() => {
                    stop_before_mapping(cancellation)?;
                    chunk.map_err(|_| HeadlessTurnPortError::ProviderProtocol)?
                }
                stop = wait_for_stop(cancellation) => return Err(stop),
            };

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
/// be streamed into memory, and only the marker survives — the body may echo the
/// request and is never retained.
async fn read_context_overflow(
    response: reqwest::Response,
    cancellation: &HeadlessTurnCancellation,
) -> Result<bool, HeadlessTurnPortError> {
    crate::read_safe_context_error(response, cancellation, &[compat::CONTEXT_OVERFLOW_MARKER]).await
}

/// The tool results recorded since the last request, in turn order.
fn tool_results(events: &[TurnEvent]) -> Vec<MessagePart> {
    events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::ToolResult(part) => Some(part.clone()),
            _ => None,
        })
        .collect()
}

impl TurnProvider for MoonshotProvider {
    fn queue_user_messages(&mut self, messages: Vec<Message>) -> Result<(), HeadlessTurnPortError> {
        if matches!(self.state, TurnState::Completed | TurnState::Failed) {
            return Err(HeadlessTurnPortError::Provider);
        }

        self.history.extend(messages);
        Ok(())
    }

    async fn next_parts(
        &mut self,
        events: &[TurnEvent],
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
        stop_before_mapping(cancellation)?;

        let state = std::mem::replace(&mut self.state, TurnState::Failed);
        match state {
            TurnState::Initial => {}
            TurnState::AwaitingToolResults { event_cursor } => {
                if self.continuation_rounds >= MAX_OPENAI_TOOL_CONTINUATION_ROUNDS {
                    return Err(HeadlessTurnPortError::Provider);
                }

                let Some(new_events) = events.get(event_cursor..) else {
                    return Err(HeadlessTurnPortError::Provider);
                };
                let results = tool_results(new_events);
                if results.is_empty() {
                    return Err(HeadlessTurnPortError::Provider);
                }

                self.history.push(Message {
                    role: Role::Tool,
                    parts: results,
                });
                self.continuation_rounds += 1;
            }
            TurnState::Completed | TurnState::Failed => {
                return Err(HeadlessTurnPortError::Provider);
            }
        }

        let response = self.send(self.payload(), cancellation).await?;
        let (parts, usage, wants_tool_results) = self.read_stream(response, cancellation).await?;

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

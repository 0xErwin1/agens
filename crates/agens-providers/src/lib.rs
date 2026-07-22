pub mod chatgpt_login;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, SystemTime};

use agens_core::{
    Error, HeadlessTurnCancellation, HeadlessTurnPortError, Message, MessagePart, RequestConfig,
    Role, TurnEvent, TurnProgressSink, TurnProvider, Usage,
};
use fs4::fs_std::FileExt;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const CHATGPT_PROVIDER_ID: &str = "openai-chatgpt";
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);
const HTTP_CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(5);
const DEFAULT_OPENAI_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_SSE_FRAME_BYTES: usize = 128 * 1024;
const MAX_CHATGPT_ERROR_BODY_BYTES: usize = 8 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_OPENAI_TOOL_CONTINUATION_ROUNDS: usize = 128;
const MAX_CHATGPT_REPLAY_ITEMS: usize = 512;
const MAX_CHATGPT_REPLAY_ITEM_BYTES: usize = 64 * 1024;
const MAX_CHATGPT_REPLAY_HISTORY_BYTES: usize = 4 * 1024 * 1024;
const PROACTIVE_REFRESH_WINDOW: Duration = Duration::from_secs(5 * 60);
const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_CHATGPT_OAUTH_URL: &str = "https://auth.openai.com/oauth/token";
const CHATGPT_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CHATGPT_ORIGINATOR: &str = "codex_cli_rs";
const AGENS_USER_AGENT: &str = concat!("Agens/", env!("CARGO_PKG_VERSION"));
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
thread_local! {
    static FAIL_BEFORE_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatGptCapabilities {
    pub subscription_access: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatGptAuthState {
    Ready,
    RefreshRequired,
}

#[derive(Clone, Debug, Default)]
pub struct ProviderCancellation {
    cancelled: Arc<AtomicBool>,
}

/// OpenAI Responses API adapter used by the headless CLI composition root.
pub struct OpenAiResponsesProvider {
    api_key: String,
    base_url: String,
    model: String,
    request_config: RequestConfig,
    initial_input: Value,
    tools: Vec<OpenAiFunctionTool>,
    parallel_tool_calls: bool,
    client: reqwest::Client,
    state: ContinuationState,
    seen_response_ids: BTreeSet<String>,
    seen_item_ids: BTreeSet<String>,
    seen_call_ids: BTreeSet<String>,
    continuation_rounds: usize,
    progress: Option<TurnProgressSink>,
}

/// ChatGPT subscription Responses transport using existing auth.json credentials.
pub struct ChatGptResponsesProvider {
    access_token: String,
    account_id: String,
    credentials_path: PathBuf,
    base_url: String,
    oauth_url: String,
    model: String,
    request_config: RequestConfig,
    instructions: String,
    initial_input: Vec<Value>,
    session_id: String,
    client: reqwest::Client,
    tools: Vec<OpenAiFunctionTool>,
    parallel_tool_calls: bool,
    state: ChatGptContinuationState,
    seen_item_ids: BTreeSet<String>,
    seen_call_ids: BTreeSet<String>,
    seen_replay_item_ids: BTreeSet<String>,
    continuation_rounds: usize,
    progress: Option<TurnProgressSink>,
}

enum ChatGptResponseError {
    Authentication(u16),
    Rejected,
    RateLimited,
    Server,
    Protocol,
    Other(HeadlessTurnPortError),
}

#[derive(Clone, Copy)]
enum SafeRemoteError {
    InvalidRequest,
    Authentication,
    Permission,
    RateLimit,
    Server,
}

impl ChatGptResponseError {
    fn into_port_error(self) -> HeadlessTurnPortError {
        match self {
            Self::Authentication(_) => HeadlessTurnPortError::Authentication,
            Self::Rejected => HeadlessTurnPortError::ProviderRejected,
            Self::RateLimited => HeadlessTurnPortError::ProviderRateLimited,
            Self::Server => HeadlessTurnPortError::ProviderServer,
            Self::Protocol => HeadlessTurnPortError::ProviderProtocol,
            Self::Other(error) => error,
        }
    }
}

enum ChatGptContinuationState {
    Initial,
    AwaitingToolOutputs {
        replay_history: Vec<Value>,
        pending_calls: Vec<PendingToolCall>,
        event_cursor: usize,
    },
    Completed,
    Failed,
}

struct RefreshFileLock {
    _file: File,
}

enum ContinuationState {
    Initial,
    AwaitingToolOutputs {
        previous_response_id: String,
        pending_calls: Vec<PendingToolCall>,
        event_cursor: usize,
    },
    Completed,
    Failed,
}

#[derive(Clone)]
struct PendingToolCall {
    item_id: String,
    call_id: String,
}

/// A validated function tool definition for the OpenAI Responses API.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenAiFunctionTool {
    name: String,
    description: String,
    parameters: Value,
}

impl OpenAiFunctionTool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
    ) -> Result<Self, Error> {
        let name = name.into();
        let description = description.into();

        let is_object_root_schema = parameters
            .as_object()
            .and_then(|schema| schema.get("type"))
            .and_then(Value::as_str)
            == Some("object");

        if name.trim().is_empty() || description.trim().is_empty() || !is_object_root_schema {
            return Err(Error::Provider(
                "OpenAI request error: function tools require a name, description, and object parameters".to_owned(),
            ));
        }

        Ok(Self {
            name,
            description,
            parameters,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn parameters(&self) -> &Value {
        &self.parameters
    }

    pub fn to_response_api_json(&self) -> Value {
        serde_json::json!({
            "type": "function",
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
            "strict": true,
        })
    }

    fn to_chatgpt_response_api_json(&self) -> Value {
        let mut tool = self.to_response_api_json();
        tool["strict"] = Value::Bool(false);
        tool
    }
}

impl OpenAiResponsesProvider {
    pub fn from_api_key(
        api_key: String,
        base_url: Option<&str>,
        model: String,
        prompt: String,
    ) -> Result<Self, Error> {
        Self::from_api_key_with_timeout(
            api_key,
            base_url,
            model,
            prompt,
            DEFAULT_OPENAI_REQUEST_TIMEOUT,
        )
    }

    pub fn from_api_key_with_timeout(
        api_key: String,
        base_url: Option<&str>,
        model: String,
        prompt: String,
        request_timeout: Duration,
    ) -> Result<Self, Error> {
        Self::from_api_key_with_tools_and_timeout(
            api_key,
            base_url,
            model,
            prompt,
            Vec::new(),
            request_timeout,
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
        if api_key.trim().is_empty() || model.trim().is_empty() || prompt.trim().is_empty() {
            return Err(Error::Auth(
                "OpenAI API authentication is unavailable".into(),
            ));
        }

        Ok(Self {
            api_key,
            base_url: base_url
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/')
                .to_owned(),
            model,
            request_config: RequestConfig::default(),
            initial_input: serde_json::json!([{
                "role": "user",
                "content": prompt,
            }]),
            tools,
            parallel_tool_calls: true,
            client: reqwest::Client::builder()
                .connect_timeout(request_timeout)
                .build()
                .map_err(|_| Error::Provider("OpenAI HTTP client is unavailable".into()))?,
            state: ContinuationState::Initial,
            seen_response_ids: BTreeSet::new(),
            seen_item_ids: BTreeSet::new(),
            seen_call_ids: BTreeSet::new(),
            continuation_rounds: 0,
            progress: None,
        })
    }

    pub fn from_api_key_with_messages_and_tools_and_timeout(
        api_key: String,
        base_url: Option<&str>,
        model: String,
        messages: Vec<Message>,
        tools: Vec<OpenAiFunctionTool>,
        request_timeout: Duration,
    ) -> Result<Self, Error> {
        let initial_input = resumed_input(&model, &messages, &tools)?;
        let mut provider = Self::from_api_key_with_tools_and_timeout(
            api_key,
            base_url,
            model,
            "resumed session".to_owned(),
            tools,
            request_timeout,
        )?;
        provider.initial_input = Value::Array(initial_input);
        Ok(provider)
    }

    pub fn with_parallel_tool_calls(mut self, parallel_tool_calls: bool) -> Self {
        self.parallel_tool_calls = parallel_tool_calls;
        self
    }

    pub fn with_request_config(mut self, request_config: RequestConfig) -> Self {
        self.request_config = request_config;
        self
    }

    async fn request_response(
        &self,
        payload: Value,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<DecodedResponse, HeadlessTurnPortError> {
        let request = self
            .client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .header("Accept", "text/event-stream")
            .json(&payload)
            .build()
            .map_err(|_| HeadlessTurnPortError::ProviderProtocol)?;
        let response = tokio::select! {
            response = self.client.execute(request) => {
                stop_before_mapping(cancellation)?;
                response.map_err(|_| HeadlessTurnPortError::ProviderNetwork)?
            }
            stop = wait_for_stop(cancellation) => return Err(stop),
        };

        stop_before_mapping(cancellation)?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let context_exceeded = read_safe_openai_context_error(response, cancellation).await?;
            return Err(classify_openai_response_status(status, context_exceeded));
        }

        decode_http_response_stream(
            response,
            cancellation,
            false,
            self.progress.as_ref(),
            HeadlessTurnPortError::ProviderProtocol,
            false,
        )
        .await
    }
}

fn classify_openai_response_status(status: u16, context_exceeded: bool) -> HeadlessTurnPortError {
    match status {
        401 | 403 => HeadlessTurnPortError::Authentication,
        429 => HeadlessTurnPortError::ProviderRateLimited,
        500..=599 => HeadlessTurnPortError::ProviderServer,
        400..=499 if context_exceeded => HeadlessTurnPortError::ProviderContext,
        400..=499 => HeadlessTurnPortError::ProviderRejected,
        _ => HeadlessTurnPortError::ProviderProtocol,
    }
}

async fn read_safe_openai_context_error(
    mut response: reqwest::Response,
    cancellation: &HeadlessTurnCancellation,
) -> Result<bool, HeadlessTurnPortError> {
    let mut body = Vec::new();
    while body.len() < MAX_CHATGPT_ERROR_BODY_BYTES {
        let chunk = tokio::select! {
            chunk = response.chunk() => chunk,
            stop = wait_for_stop(cancellation) => return Err(stop),
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => return Ok(false),
        };
        let Some(chunk) = chunk else {
            break;
        };
        let remaining = MAX_CHATGPT_ERROR_BODY_BYTES - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    stop_before_mapping(cancellation)?;

    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return Ok(false);
    };
    let error = body.get("error").unwrap_or(&body);
    Ok(["code", "type"]
        .into_iter()
        .any(|field| error.get(field).and_then(Value::as_str) == Some("context_length_exceeded")))
}

impl ChatGptResponsesProvider {
    pub fn from_credentials(
        credentials_path: &Path,
        base_url: Option<&str>,
        model: String,
        instructions: String,
        input: String,
    ) -> Result<Self, Error> {
        Self::from_credentials_with_timeout(
            credentials_path,
            base_url,
            model,
            instructions,
            input,
            DEFAULT_OPENAI_REQUEST_TIMEOUT,
        )
    }

    pub fn from_credentials_with_timeout(
        credentials_path: &Path,
        base_url: Option<&str>,
        model: String,
        instructions: String,
        input: String,
        request_timeout: Duration,
    ) -> Result<Self, Error> {
        Self::from_credentials_with_timeout_and_auth_url(
            credentials_path,
            base_url,
            None,
            model,
            instructions,
            input,
            request_timeout,
        )
    }

    pub fn from_credentials_with_timeout_and_auth_url(
        credentials_path: &Path,
        base_url: Option<&str>,
        oauth_url: Option<&str>,
        model: String,
        instructions: String,
        input: String,
        request_timeout: Duration,
    ) -> Result<Self, Error> {
        Self::from_credentials_with_tools_and_timeout_and_auth_url(
            credentials_path,
            base_url,
            oauth_url,
            model,
            instructions,
            input,
            Vec::new(),
            request_timeout,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_credentials_with_tools_and_timeout_and_auth_url(
        credentials_path: &Path,
        base_url: Option<&str>,
        oauth_url: Option<&str>,
        model: String,
        instructions: String,
        input: String,
        tools: Vec<OpenAiFunctionTool>,
        request_timeout: Duration,
    ) -> Result<Self, Error> {
        if model.trim().is_empty() || instructions.trim().is_empty() || input.trim().is_empty() {
            return Err(auth_error("request configuration is incomplete"));
        }

        let credentials = read_credentials(credentials_path)?;
        let entry = chatgpt_entry(&credentials)?;
        let access_token = required_credential_string(entry, "access_token")?.to_owned();
        let account_id = required_credential_string(entry, "account_id")?.to_owned();
        required_credential_string(entry, "refresh_token")?;
        required_credential_string(entry, "expires_at")?;

        Ok(Self {
            access_token,
            account_id,
            credentials_path: credentials_path.to_path_buf(),
            base_url: chatgpt_responses_endpoint(base_url)?,
            oauth_url: oauth_url
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(DEFAULT_CHATGPT_OAUTH_URL)
                .to_owned(),
            model,
            request_config: RequestConfig::default(),
            instructions,
            initial_input: vec![serde_json::json!({
                "role": "user",
                "content": [{"type": "input_text", "text": input}],
            })],
            session_id: format!(
                "agens-{}",
                TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ),
            client: reqwest::Client::builder()
                .connect_timeout(request_timeout)
                .timeout(request_timeout)
                .build()
                .map_err(|_| Error::Provider("ChatGPT HTTP client is unavailable".into()))?,
            tools,
            parallel_tool_calls: true,
            state: ChatGptContinuationState::Initial,
            seen_item_ids: BTreeSet::new(),
            seen_call_ids: BTreeSet::new(),
            seen_replay_item_ids: BTreeSet::new(),
            continuation_rounds: 0,
            progress: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_credentials_with_messages_and_tools_and_timeout_and_auth_url(
        credentials_path: &Path,
        base_url: Option<&str>,
        oauth_url: Option<&str>,
        model: String,
        instructions: String,
        messages: Vec<Message>,
        tools: Vec<OpenAiFunctionTool>,
        request_timeout: Duration,
    ) -> Result<Self, Error> {
        let initial_input = resumed_input(&model, &messages, &tools)?;
        let mut provider = Self::from_credentials_with_tools_and_timeout_and_auth_url(
            credentials_path,
            base_url,
            oauth_url,
            model,
            instructions,
            "resumed session".to_owned(),
            tools,
            request_timeout,
        )?;
        provider.initial_input = initial_input;
        Ok(provider)
    }

    pub fn with_parallel_tool_calls(mut self, parallel_tool_calls: bool) -> Self {
        self.parallel_tool_calls = parallel_tool_calls;
        self
    }

    pub fn with_request_config(mut self, request_config: RequestConfig) -> Self {
        self.request_config = request_config;
        self
    }

    async fn request_response(
        &self,
        payload: Value,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<DecodedResponse, ChatGptResponseError> {
        let request = self
            .client
            .post(&self.base_url)
            .bearer_auth(&self.access_token)
            .header("ChatGPT-Account-ID", &self.account_id)
            .header("Accept", "text/event-stream")
            .header("originator", CHATGPT_ORIGINATOR)
            .header("User-Agent", AGENS_USER_AGENT)
            .header("session-id", &self.session_id)
            .json(&payload)
            .build()
            .map_err(|_| ChatGptResponseError::Other(HeadlessTurnPortError::Provider))?;
        let response = tokio::select! {
            response = self.client.execute(request) => {
                stop_before_mapping(cancellation).map_err(ChatGptResponseError::Other)?;
                response.map_err(|_| ChatGptResponseError::Other(HeadlessTurnPortError::Provider))?
            }
            stop = wait_for_stop(cancellation) => return Err(ChatGptResponseError::Other(stop)),
        };

        stop_before_mapping(cancellation).map_err(ChatGptResponseError::Other)?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let _metadata = read_safe_chatgpt_error(response, cancellation)
                .await
                .map_err(ChatGptResponseError::Other)?;
            return Err(match status {
                401 | 403 => ChatGptResponseError::Authentication(status),
                429 => ChatGptResponseError::RateLimited,
                500..=599 => ChatGptResponseError::Server,
                400..=499 => ChatGptResponseError::Rejected,
                _ => ChatGptResponseError::Protocol,
            });
        }

        decode_http_response_stream(
            response,
            cancellation,
            true,
            self.progress.as_ref(),
            HeadlessTurnPortError::ProviderProtocol,
            true,
        )
        .await
        .map_err(|error| match error {
            HeadlessTurnPortError::Cancelled | HeadlessTurnPortError::TimedOut => {
                ChatGptResponseError::Other(error)
            }
            _ => ChatGptResponseError::Protocol,
        })
    }

    async fn refresh_if_needed(
        &mut self,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<(), HeadlessTurnPortError> {
        stop_before_mapping(cancellation)?;
        if load_chatgpt_auth_state(&self.credentials_path, SystemTime::now())
            .map_err(|_| HeadlessTurnPortError::Authentication)?
            == ChatGptAuthState::RefreshRequired
        {
            self.refresh_or_adopt(cancellation).await?;
        }
        stop_before_mapping(cancellation)
    }

    async fn refresh_or_adopt(
        &mut self,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<(), HeadlessTurnPortError> {
        let _lock = acquire_refresh_file_lock(&self.credentials_path, cancellation).await?;

        stop_before_mapping(cancellation)?;
        let credentials = read_credentials(&self.credentials_path)
            .map_err(|_| HeadlessTurnPortError::Authentication)?;
        let entry =
            chatgpt_entry(&credentials).map_err(|_| HeadlessTurnPortError::Authentication)?;
        let access_token = required_credential_string(entry, "access_token")
            .map_err(|_| HeadlessTurnPortError::Authentication)?;
        let account_id = required_credential_string(entry, "account_id")
            .map_err(|_| HeadlessTurnPortError::Authentication)?;

        if access_token != self.access_token
            && load_chatgpt_auth_state(&self.credentials_path, SystemTime::now())
                .map_err(|_| HeadlessTurnPortError::Authentication)?
                == ChatGptAuthState::Ready
        {
            self.access_token = access_token.to_owned();
            self.account_id = account_id.to_owned();
            return stop_before_mapping(cancellation);
        }

        if account_id != self.account_id {
            return Err(HeadlessTurnPortError::Authentication);
        }

        let refresh_token = required_credential_string(entry, "refresh_token")
            .map_err(|_| HeadlessTurnPortError::Authentication)?;
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", CHATGPT_OAUTH_CLIENT_ID)
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", refresh_token)
            .finish();
        let request = self
            .client
            .post(&self.oauth_url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form)
            .build()
            .map_err(|_| HeadlessTurnPortError::Provider)?;
        let response = tokio::select! {
            response = self.client.execute(request) => {
                stop_before_mapping(cancellation)?;
                response.map_err(|_| HeadlessTurnPortError::Provider)?
            }
            stop = wait_for_stop(cancellation) => return Err(stop),
        };

        stop_before_mapping(cancellation)?;
        if response.status().as_u16() == 401 {
            return Err(HeadlessTurnPortError::Authentication);
        }

        let status = response.status();
        if !status.is_success() {
            if status.is_server_error() {
                return Err(HeadlessTurnPortError::Provider);
            }

            let body = tokio::select! {
                body = response.json::<Value>() => body.ok(),
                stop = wait_for_stop(cancellation) => return Err(stop),
            };
            stop_before_mapping(cancellation)?;
            return Err(if body.as_ref().is_some_and(is_permanent_refresh_failure) {
                HeadlessTurnPortError::Authentication
            } else {
                HeadlessTurnPortError::Provider
            });
        }

        let body = tokio::select! {
            body = response.json::<Value>() => body.map_err(|_| HeadlessTurnPortError::Authentication)?,
            stop = wait_for_stop(cancellation) => return Err(stop),
        };
        stop_before_mapping(cancellation)?;

        let access_token = body
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(HeadlessTurnPortError::Authentication)?;
        let expires_at = jwt_expiry(access_token)
            .or_else(|| {
                body.get("expires_in")
                    .and_then(Value::as_u64)
                    .filter(|seconds| *seconds > 0)
                    .and_then(|seconds| SystemTime::now().checked_add(Duration::from_secs(seconds)))
            })
            .map(timestamp_to_rfc3339)
            .ok_or(HeadlessTurnPortError::Authentication)?;
        let refresh_token = body
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let id_token = match body.get("id_token") {
            Some(Value::String(id_token)) => Some(id_token.as_str()),
            Some(_) => return Err(HeadlessTurnPortError::Authentication),
            None => None,
        };
        let refreshed_account_id = id_token
            .map(chatgpt_login::account_id_from_id_token)
            .transpose()
            .map_err(|_| HeadlessTurnPortError::Authentication)?;

        persist_chatgpt_refresh_with_id_token(
            &self.credentials_path,
            access_token,
            refresh_token,
            id_token,
            &expires_at,
        )
        .map_err(|_| HeadlessTurnPortError::Authentication)?;
        stop_before_mapping(cancellation)?;

        self.access_token = access_token.to_owned();
        if let Some(account_id) = refreshed_account_id {
            self.account_id = account_id;
        }
        Ok(())
    }

    fn request_payload(&self, input: Vec<Value>) -> Value {
        let mut payload = serde_json::json!({
            "model": self.model,
            "instructions": self.instructions,
            "input": input,
            "tool_choice": "auto",
            "parallel_tool_calls": self.parallel_tool_calls,
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "reasoning": {"summary": "auto"},
        });
        payload["tools"] = chatgpt_function_tools_json(&self.tools);
        if let Some(effort) = self.request_config.reasoning_effort() {
            payload["reasoning"]["effort"] = Value::String(effort.as_str().to_owned());
        }
        payload
    }

    fn initial_input(&self) -> Vec<Value> {
        self.initial_input.clone()
    }
}

impl TurnProvider for ChatGptResponsesProvider {
    async fn next_parts(
        &mut self,
        events: &[TurnEvent],
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
        if cancellation.is_cancelled() {
            return Err(HeadlessTurnPortError::Cancelled);
        }
        if cancellation.is_expired() {
            return Err(HeadlessTurnPortError::TimedOut);
        }
        let state = std::mem::replace(&mut self.state, ChatGptContinuationState::Failed);
        let (payload, replay_history) = match state {
            ChatGptContinuationState::Initial => {
                let replay_history = self.initial_input();
                (self.request_payload(replay_history.clone()), replay_history)
            }
            ChatGptContinuationState::AwaitingToolOutputs {
                mut replay_history,
                pending_calls,
                event_cursor,
            } => {
                if self.continuation_rounds >= MAX_OPENAI_TOOL_CONTINUATION_ROUNDS {
                    return Err(HeadlessTurnPortError::Provider);
                }
                let Some(new_events) = events.get(event_cursor..) else {
                    return Err(HeadlessTurnPortError::Provider);
                };
                let outputs = correlated_tool_outputs(&pending_calls, new_events)
                    .map_err(|()| HeadlessTurnPortError::Provider)?;
                replay_history.extend(outputs);
                validate_chatgpt_replay_history(&replay_history)
                    .map_err(|()| HeadlessTurnPortError::Provider)?;
                (self.request_payload(replay_history.clone()), replay_history)
            }
            ChatGptContinuationState::Completed | ChatGptContinuationState::Failed => {
                return Err(HeadlessTurnPortError::Provider);
            }
        };

        self.refresh_if_needed(cancellation).await?;
        let response = match self.request_response(payload, cancellation).await {
            Ok(response) => response,
            Err(ChatGptResponseError::Authentication(403)) => {
                self.state = ChatGptContinuationState::Failed;
                return Err(HeadlessTurnPortError::Authentication);
            }
            Err(ChatGptResponseError::Authentication(401)) => {
                self.refresh_or_adopt(cancellation).await?;
                match self
                    .request_response(self.request_payload(replay_history.clone()), cancellation)
                    .await
                {
                    Ok(response) => response,
                    Err(ChatGptResponseError::Authentication(_)) => {
                        self.state = ChatGptContinuationState::Failed;
                        return Err(HeadlessTurnPortError::Authentication);
                    }
                    Err(ChatGptResponseError::Other(error)) => {
                        self.state = ChatGptContinuationState::Failed;
                        return Err(error);
                    }
                    Err(error) => {
                        self.state = ChatGptContinuationState::Failed;
                        return Err(error.into_port_error());
                    }
                }
            }
            Err(ChatGptResponseError::Authentication(_)) => {
                self.state = ChatGptContinuationState::Failed;
                return Err(HeadlessTurnPortError::Authentication);
            }
            Err(ChatGptResponseError::Other(error)) => {
                self.state = ChatGptContinuationState::Failed;
                return Err(error);
            }
            Err(error) => {
                self.state = ChatGptContinuationState::Failed;
                return Err(error.into_port_error());
            }
        };
        if response.pending_calls.iter().any(|call| {
            !self.seen_item_ids.insert(call.item_id.clone())
                || !self.seen_call_ids.insert(call.call_id.clone())
        }) || response.replay_items.iter().any(|item| {
            item.get("id")
                .and_then(Value::as_str)
                .is_none_or(|id| !self.seen_replay_item_ids.insert(id.to_owned()))
        }) || replay_history.len() + response.replay_items.len() > MAX_CHATGPT_REPLAY_ITEMS
        {
            self.state = ChatGptContinuationState::Failed;
            return Err(HeadlessTurnPortError::Provider);
        }

        let mut replay_history = replay_history;
        replay_history.extend(response.replay_items);
        if validate_chatgpt_replay_history(&replay_history).is_err() {
            self.state = ChatGptContinuationState::Failed;
            return Err(HeadlessTurnPortError::Provider);
        }
        if response.pending_calls.is_empty() {
            self.state = ChatGptContinuationState::Completed;
        } else {
            self.continuation_rounds += 1;
            self.state = ChatGptContinuationState::AwaitingToolOutputs {
                replay_history,
                pending_calls: response.pending_calls,
                event_cursor: events.len(),
            };
        }
        Ok(response.parts)
    }
}

/// Allows the CLI to observe decoded stream deltas without changing final results.
pub trait ProgressAwareProvider: TurnProvider {
    fn with_progress_sink(self, progress: TurnProgressSink) -> Self;
}

impl ProgressAwareProvider for OpenAiResponsesProvider {
    fn with_progress_sink(mut self, progress: TurnProgressSink) -> Self {
        self.progress = Some(progress);
        self
    }
}

impl ProgressAwareProvider for ChatGptResponsesProvider {
    fn with_progress_sink(mut self, progress: TurnProgressSink) -> Self {
        self.progress = Some(progress);
        self
    }
}

impl TurnProvider for OpenAiResponsesProvider {
    async fn next_parts(
        &mut self,
        _events: &[TurnEvent],
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
        if cancellation.is_cancelled() {
            return Err(HeadlessTurnPortError::Cancelled);
        }
        if cancellation.is_expired() {
            return Err(HeadlessTurnPortError::TimedOut);
        }
        let state = std::mem::replace(&mut self.state, ContinuationState::Failed);
        let payload = match state {
            ContinuationState::Initial => self.initial_payload(),
            ContinuationState::AwaitingToolOutputs {
                previous_response_id,
                pending_calls,
                event_cursor,
            } => {
                let Some(events) = _events.get(event_cursor..) else {
                    return Err(HeadlessTurnPortError::Provider);
                };

                match continuation_payload(
                    &self.model,
                    &self.request_config,
                    &self.tools,
                    self.parallel_tool_calls,
                    &previous_response_id,
                    &pending_calls,
                    events,
                ) {
                    Ok(payload) => payload,
                    Err(()) => return Err(HeadlessTurnPortError::Provider),
                }
            }
            ContinuationState::Completed | ContinuationState::Failed => {
                return Err(HeadlessTurnPortError::Provider);
            }
        };

        let response = match self.request_response(payload, cancellation).await {
            Ok(response) => response,
            Err(error) => {
                self.state = ContinuationState::Failed;
                return Err(error);
            }
        };

        if response
            .response_id
            .as_ref()
            .is_some_and(|response_id| !self.seen_response_ids.insert(response_id.clone()))
            || response.pending_calls.iter().any(|call| {
                !self.seen_item_ids.insert(call.item_id.clone())
                    || !self.seen_call_ids.insert(call.call_id.clone())
            })
        {
            self.state = ContinuationState::Failed;
            return Err(HeadlessTurnPortError::Provider);
        }

        if response.pending_calls.is_empty() {
            self.state = ContinuationState::Completed;
        } else {
            if self.continuation_rounds == MAX_OPENAI_TOOL_CONTINUATION_ROUNDS {
                self.state = ContinuationState::Failed;
                return Err(HeadlessTurnPortError::Provider);
            }
            let Some(previous_response_id) = response.response_id else {
                self.state = ContinuationState::Failed;
                return Err(HeadlessTurnPortError::Provider);
            };
            self.continuation_rounds += 1;
            self.state = ContinuationState::AwaitingToolOutputs {
                previous_response_id,
                pending_calls: response.pending_calls,
                event_cursor: _events.len(),
            };
        }

        Ok(response.parts)
    }
}

impl OpenAiResponsesProvider {
    fn initial_payload(&self) -> Value {
        let mut payload = serde_json::json!({
            "model": self.model,
            "input": self.initial_input,
            "parallel_tool_calls": self.parallel_tool_calls,
            "stream": true,
        });

        if !self.tools.is_empty() {
            payload["tools"] = function_tools_json(&self.tools);
        }

        if let Some(effort) = self.request_config.reasoning_effort() {
            payload["reasoning"] = serde_json::json!({"effort": effort.as_str()});
        }

        payload
    }
}

fn continuation_payload(
    model: &str,
    request_config: &RequestConfig,
    tools: &[OpenAiFunctionTool],
    parallel_tool_calls: bool,
    previous_response_id: &str,
    pending_calls: &[PendingToolCall],
    events: &[TurnEvent],
) -> Result<Value, ()> {
    let mut outputs = BTreeMap::new();

    for event in events {
        let TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id,
            content,
            is_error,
        }) = event
        else {
            continue;
        };

        if !pending_calls
            .iter()
            .any(|call| call.call_id == *tool_call_id)
            || outputs.contains_key(tool_call_id)
        {
            return Err(());
        }

        let output = if *is_error {
            "Tool execution failed".to_owned()
        } else {
            bounded_tool_output(content)
        };
        outputs.insert(tool_call_id, output);
    }

    if outputs.len() != pending_calls.len() {
        return Err(());
    }

    let input = pending_calls
        .iter()
        .map(|call| {
            let output = outputs.remove(&call.call_id).ok_or(())?;
            Ok(serde_json::json!({
                "type": "function_call_output",
                "call_id": call.call_id,
                "output": output,
            }))
        })
        .collect::<Result<Vec<_>, ()>>()?;
    let mut payload = serde_json::json!({
        "model": model,
        "previous_response_id": previous_response_id,
        "input": input,
        "parallel_tool_calls": parallel_tool_calls,
        "stream": true,
    });

    if !tools.is_empty() {
        payload["tools"] = function_tools_json(tools);
    }

    if let Some(effort) = request_config.reasoning_effort() {
        payload["reasoning"] = serde_json::json!({"effort": effort.as_str()});
    }

    Ok(payload)
}

fn correlated_tool_outputs(
    pending_calls: &[PendingToolCall],
    events: &[TurnEvent],
) -> Result<Vec<Value>, ()> {
    let mut outputs = BTreeMap::new();

    for event in events {
        let TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id,
            content,
            is_error,
        }) = event
        else {
            continue;
        };

        if !pending_calls
            .iter()
            .any(|call| call.call_id == *tool_call_id)
            || outputs.contains_key(tool_call_id)
        {
            return Err(());
        }

        let output = if *is_error {
            "Tool execution failed".to_owned()
        } else {
            bounded_tool_output(content)
        };
        outputs.insert(tool_call_id, output);
    }

    if outputs.len() != pending_calls.len() {
        return Err(());
    }

    pending_calls
        .iter()
        .map(|call| {
            let output = outputs.remove(&call.call_id).ok_or(())?;
            Ok(serde_json::json!({
                "type": "function_call_output",
                "call_id": call.call_id,
                "output": output,
            }))
        })
        .collect()
}

fn validate_chatgpt_replay_history(history: &[Value]) -> Result<(), ()> {
    if history.len() > MAX_CHATGPT_REPLAY_ITEMS {
        return Err(());
    }

    let mut bytes = 0_usize;
    for item in history {
        let item_bytes = serde_json::to_vec(item).map_err(|_| ())?;
        if item_bytes.len() > MAX_CHATGPT_REPLAY_ITEM_BYTES {
            return Err(());
        }
        bytes = bytes.checked_add(item_bytes.len()).ok_or(())?;
        if bytes > MAX_CHATGPT_REPLAY_HISTORY_BYTES {
            return Err(());
        }
    }

    Ok(())
}

fn bounded_tool_output(content: &str) -> String {
    if content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return content.to_owned();
    }

    let mut end = MAX_TOOL_OUTPUT_BYTES;
    while !content.is_char_boundary(end) {
        end -= 1;
    }

    content[..end].to_owned()
}

async fn decode_http_response_stream(
    mut response: reqwest::Response,
    cancellation: &HeadlessTurnCancellation,
    require_encrypted_reasoning: bool,
    progress: Option<&TurnProgressSink>,
    protocol_failure: HeadlessTurnPortError,
    finish_on_terminal: bool,
) -> Result<DecodedResponse, HeadlessTurnPortError> {
    let mut decoder = OpenAiResponseDecoder::new(require_encrypted_reasoning);
    let mut frame = Vec::new();

    loop {
        let next_chunk = tokio::select! {
            chunk = response.chunk() => {
                stop_before_mapping(cancellation)?;
                chunk.map_err(|_| protocol_failure)?
            }
            stop = wait_for_stop(cancellation) => return Err(stop),
        };
        let Some(chunk) = next_chunk else {
            let completed = decoder.finish();
            stop_before_mapping(cancellation)?;
            return completed.map_err(|_| protocol_failure);
        };

        for byte in chunk {
            if byte == b'\n' {
                let processed = process_sse_frame(&mut decoder, &mut frame, progress);
                stop_before_mapping(cancellation)?;
                if finish_on_terminal && processed.map_err(|_| protocol_failure)? {
                    return decoder.finish().map_err(|_| protocol_failure);
                }
                continue;
            }

            if frame.len() == MAX_SSE_FRAME_BYTES {
                stop_before_mapping(cancellation)?;
                return Err(protocol_failure);
            }
            frame.push(byte);
        }

        stop_before_mapping(cancellation)?;
    }
}

fn process_sse_frame(
    decoder: &mut OpenAiResponseDecoder,
    frame: &mut Vec<u8>,
    progress: Option<&TurnProgressSink>,
) -> Result<bool, ()> {
    if frame.last() == Some(&b'\r') {
        frame.pop();
    }

    let data = frame
        .strip_prefix(b"data:")
        .map(|value| value.strip_prefix(b" ").unwrap_or(value));
    if let Some(data) = data.filter(|data| !data.is_empty()) {
        if data == b"[DONE]" {
            frame.clear();
            return Ok(false);
        }
        let event = std::str::from_utf8(data).map_err(|_| ())?;
        let start = decoder.parts.len();
        decoder.process(event).map_err(|_| ())?;
        if let Some(progress) = progress {
            for part in &decoder.parts[start..] {
                progress(agens_core::TurnEvent::ProviderPart(part.clone()));
            }
            if let Some(usage) = decoder.completed_usage() {
                progress(TurnEvent::Usage(usage));
            }
        }
    }
    let completed = decoder.completed;
    frame.clear();
    Ok(completed)
}

async fn read_safe_chatgpt_error(
    mut response: reqwest::Response,
    cancellation: &HeadlessTurnCancellation,
) -> Result<Option<SafeRemoteError>, HeadlessTurnPortError> {
    let mut body = Vec::new();
    while body.len() < MAX_CHATGPT_ERROR_BODY_BYTES {
        let chunk = tokio::select! {
            chunk = response.chunk() => chunk,
            stop = wait_for_stop(cancellation) => return Err(stop),
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => return Ok(None),
        };
        let Some(chunk) = chunk else {
            break;
        };
        let remaining = MAX_CHATGPT_ERROR_BODY_BYTES - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    stop_before_mapping(cancellation)?;

    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return Ok(None);
    };
    let error = body.get("error").unwrap_or(&body);
    Ok(["code", "type"]
        .into_iter()
        .filter_map(|field| error.get(field).and_then(Value::as_str))
        .find_map(safe_remote_error))
}

fn safe_remote_error(value: &str) -> Option<SafeRemoteError> {
    match value {
        "invalid_request" | "invalid_request_error" | "context_length_exceeded" => {
            Some(SafeRemoteError::InvalidRequest)
        }
        "authentication_error" | "invalid_api_key" => Some(SafeRemoteError::Authentication),
        "permission_error" => Some(SafeRemoteError::Permission),
        "rate_limit_error" | "rate_limit_exceeded" | "insufficient_quota" => {
            Some(SafeRemoteError::RateLimit)
        }
        "server_error" => Some(SafeRemoteError::Server),
        _ => None,
    }
}

fn stop_before_mapping(
    cancellation: &HeadlessTurnCancellation,
) -> Result<(), HeadlessTurnPortError> {
    if cancellation.is_cancelled() {
        return Err(HeadlessTurnPortError::Cancelled);
    }
    if cancellation.is_expired() {
        return Err(HeadlessTurnPortError::TimedOut);
    }
    Ok(())
}

async fn wait_for_stop(cancellation: &HeadlessTurnCancellation) -> HeadlessTurnPortError {
    loop {
        if cancellation.is_cancelled() {
            return HeadlessTurnPortError::Cancelled;
        }
        if cancellation.is_expired() {
            return HeadlessTurnPortError::TimedOut;
        }

        tokio::time::sleep(HTTP_CANCELLATION_POLL_INTERVAL).await;
    }
}

impl ProviderCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

pub const fn chatgpt_capabilities() -> ChatGptCapabilities {
    ChatGptCapabilities {
        subscription_access: true,
    }
}

pub fn load_chatgpt_auth_state(
    credentials_path: &Path,
    now: SystemTime,
) -> Result<ChatGptAuthState, Error> {
    let credentials = read_credentials(credentials_path)?;
    let entry = chatgpt_entry(&credentials)?;
    let expires_at = required_credential_string(entry, "expires_at")?;
    let expires_at =
        parse_rfc3339_timestamp(expires_at).ok_or_else(|| auth_error("credentials are invalid"))?;

    required_credential_string(entry, "refresh_token")?;
    required_credential_string(entry, "account_id")?;

    let access_token = required_credential_string(entry, "access_token")?;
    let expiry = jwt_expiry(access_token).unwrap_or(expires_at);

    if now
        .checked_add(PROACTIVE_REFRESH_WINDOW)
        .is_none_or(|refresh_at| refresh_at >= expiry)
    {
        return Ok(ChatGptAuthState::RefreshRequired);
    }

    Ok(ChatGptAuthState::Ready)
}

pub fn persist_chatgpt_refresh(
    credentials_path: &Path,
    access_token: &str,
    refresh_token: Option<&str>,
    expires_at: &str,
) -> Result<(), Error> {
    persist_chatgpt_refresh_with_id_token(
        credentials_path,
        access_token,
        refresh_token,
        None,
        expires_at,
    )
}

pub fn persist_chatgpt_refresh_with_id_token(
    credentials_path: &Path,
    access_token: &str,
    refresh_token: Option<&str>,
    id_token: Option<&str>,
    expires_at: &str,
) -> Result<(), Error> {
    if access_token.is_empty() || refresh_token.is_some_and(str::is_empty) {
        return Err(auth_error("refreshed credentials are incomplete"));
    }

    if parse_rfc3339_timestamp(expires_at).is_none() {
        return Err(auth_error("refreshed credentials are invalid"));
    }

    let refreshed_account_id = id_token
        .map(chatgpt_login::account_id_from_id_token)
        .transpose()
        .map_err(|_| auth_error("refreshed credentials are invalid"))?;

    let mut credentials = read_credentials(credentials_path)?;
    let entry = chatgpt_entry_mut(&mut credentials)?;

    required_credential_string(entry, "account_id")?;

    if refresh_token.is_none() {
        required_credential_string(entry, "refresh_token")?;
    }

    let entry = entry
        .as_object_mut()
        .ok_or_else(|| auth_error("credentials are invalid"))?;
    entry.insert(
        "access_token".to_owned(),
        Value::String(access_token.to_owned()),
    );
    entry.insert(
        "expires_at".to_owned(),
        Value::String(expires_at.to_owned()),
    );

    if let Some(refresh_token) = refresh_token {
        entry.insert(
            "refresh_token".to_owned(),
            Value::String(refresh_token.to_owned()),
        );
    }

    if let Some(account_id) = refreshed_account_id {
        entry.insert("account_id".to_owned(), Value::String(account_id));
    }

    let contents = serde_json::to_vec(&credentials)
        .map_err(|_| auth_error("refreshed credentials could not be encoded"))?;
    write_credentials_atomically(credentials_path, &contents)
}

pub fn encode_openai_response_request(model: &str, message: &Message) -> Result<String, Error> {
    encode_openai_response_request_with_tools(model, message, &[])
}

pub fn encode_openai_response_request_with_tools(
    model: &str,
    message: &Message,
    tools: &[OpenAiFunctionTool],
) -> Result<String, Error> {
    let content = match message.parts.as_slice() {
        [MessagePart::Text(content)] => content,
        _ => {
            return Err(Error::Provider(
                "OpenAI request error: only a single text part is supported".to_owned(),
            ));
        }
    };
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        _ => {
            return Err(Error::Provider(
                "OpenAI request error: only system and user messages are supported".to_owned(),
            ));
        }
    };

    let mut request = serde_json::json!({
        "model": model,
        "input": [{ "role": role, "content": content }],
        "stream": true,
    });

    if !tools.is_empty() {
        request["tools"] = function_tools_json(tools);
    }

    Ok(request.to_string())
}

pub fn encode_openai_response_request_with_messages(
    model: &str,
    messages: &[Message],
    tools: &[OpenAiFunctionTool],
) -> Result<String, Error> {
    if model.trim().is_empty() || messages.is_empty() {
        return Err(invalid_resumed_history());
    }

    let mut input = Vec::new();
    let mut calls = BTreeSet::new();
    let mut results = BTreeSet::new();

    for message in messages {
        match message.role {
            Role::System | Role::User => {
                let content = message
                    .parts
                    .iter()
                    .map(|part| match part {
                        MessagePart::Text(text) if !text.is_empty() => {
                            Ok(serde_json::json!({ "type": "input_text", "text": text }))
                        }
                        _ => Err(invalid_resumed_history()),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if content.is_empty() {
                    return Err(invalid_resumed_history());
                }
                input.push(serde_json::json!({
                    "role": match message.role {
                        Role::System => "system",
                        Role::User => "user",
                        _ => unreachable!(),
                    },
                    "content": content,
                }));
            }
            Role::Assistant => {
                if message.parts.is_empty() {
                    return Err(invalid_resumed_history());
                }
                for part in &message.parts {
                    match part {
                        MessagePart::Text(text) if !text.is_empty() => {
                            input.push(serde_json::json!({
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": text }],
                            }));
                        }
                        MessagePart::Reasoning(text) if !text.is_empty() => {
                            input.push(serde_json::json!({
                                "type": "reasoning",
                                "summary": [{ "type": "summary_text", "text": text }],
                            }));
                        }
                        MessagePart::ToolCall {
                            id,
                            name,
                            input: arguments,
                        } if !id.is_empty()
                            && !name.is_empty()
                            && serde_json::from_str::<Value>(arguments).is_ok()
                            && calls.insert(id.clone()) =>
                        {
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": arguments,
                            }));
                        }
                        _ => return Err(invalid_resumed_history()),
                    }
                }
            }
            Role::Tool => {
                if message.parts.is_empty() {
                    return Err(invalid_resumed_history());
                }
                for part in &message.parts {
                    let MessagePart::ToolResult {
                        tool_call_id,
                        content,
                        ..
                    } = part
                    else {
                        return Err(invalid_resumed_history());
                    };
                    if tool_call_id.is_empty()
                        || content.is_empty()
                        || !calls.contains(tool_call_id)
                        || !results.insert(tool_call_id.clone())
                    {
                        return Err(invalid_resumed_history());
                    }
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": tool_call_id,
                        "output": content,
                    }));
                }
            }
        }
    }

    if calls != results {
        return Err(invalid_resumed_history());
    }

    let mut request = serde_json::json!({
        "model": model,
        "input": input,
        "stream": true,
    });
    if !tools.is_empty() {
        request["tools"] = function_tools_json(tools);
    }

    Ok(request.to_string())
}

fn resumed_input(
    model: &str,
    messages: &[Message],
    tools: &[OpenAiFunctionTool],
) -> Result<Vec<Value>, Error> {
    let request = encode_openai_response_request_with_messages(model, messages, tools)?;
    serde_json::from_str::<Value>(&request)
        .ok()
        .and_then(|request| request.get("input").cloned())
        .and_then(|input| input.as_array().cloned())
        .ok_or_else(invalid_resumed_history)
}

fn invalid_resumed_history() -> Error {
    Error::Provider("OpenAI request error: resumed history is invalid".to_owned())
}

pub fn decode_openai_response_events<I, S>(events: I) -> Result<Vec<MessagePart>, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut decoder = OpenAiResponseDecoder::default();

    for event in events {
        decoder.process(event.as_ref())?;
    }

    decoder.finish().map(|response| response.parts)
}

pub fn decode_openai_response_events_with_usage<I, S>(events: I) -> Result<Vec<TurnEvent>, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut decoder = OpenAiResponseDecoder::default();

    for event in events {
        decoder.process(event.as_ref())?;
    }

    decoder.finish().map(|response| {
        let mut events = response
            .parts
            .into_iter()
            .map(TurnEvent::ProviderPart)
            .collect::<Vec<_>>();
        events.push(TurnEvent::Usage(response.usage));
        events
    })
}

pub fn decode_openai_response_stream(
    events: Receiver<String>,
    cancellation: &ProviderCancellation,
) -> Result<Vec<MessagePart>, Error> {
    let mut decoder = OpenAiResponseDecoder::default();

    loop {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }

        match events.recv_timeout(CANCELLATION_POLL_INTERVAL) {
            Ok(event) => {
                if cancellation.is_cancelled() {
                    return Err(Error::Cancelled);
                }

                decoder.process(&event)?;
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                if cancellation.is_cancelled() {
                    return Err(Error::Cancelled);
                }

                return decoder.finish().map(|response| response.parts);
            }
        }
    }
}

#[derive(Default)]
struct OpenAiResponseDecoder {
    parts: Vec<MessagePart>,
    function_calls: BTreeMap<String, FunctionCall>,
    response_id: Option<String>,
    seen_item_ids: BTreeMap<String, ()>,
    seen_call_ids: BTreeMap<String, ()>,
    function_call_order: Vec<String>,
    completed_calls: BTreeMap<String, PendingToolCall>,
    replay_items: Vec<Value>,
    replay_item_positions: BTreeMap<String, usize>,
    completed_function_output_item_ids: BTreeSet<String>,
    require_encrypted_reasoning: bool,
    usage: Option<Usage>,
    completed: bool,
}

struct DecodedResponse {
    parts: Vec<MessagePart>,
    usage: Usage,
    response_id: Option<String>,
    pending_calls: Vec<PendingToolCall>,
    replay_items: Vec<Value>,
}

struct FunctionCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl OpenAiResponseDecoder {
    fn new(require_encrypted_reasoning: bool) -> Self {
        Self {
            require_encrypted_reasoning,
            ..Self::default()
        }
    }

    fn process(&mut self, event_json: &str) -> Result<(), Error> {
        let event: Value =
            serde_json::from_str(event_json).map_err(|_| protocol_error("invalid event JSON"))?;
        let event_type = required_string(&event, "type")?;

        match event_type {
            "response.created" => self.capture_response_id(&event)?,
            "response.output_text.delta" => {
                self.parts.push(MessagePart::Text(
                    required_string(&event, "delta")?.to_owned(),
                ));
            }
            "response.output_item.done" => self.process_output_item(&event)?,
            "response.output_item.added" => self.add_function_call(&event)?,
            "response.function_call_arguments.delta" => self.append_function_arguments(&event)?,
            "response.function_call_arguments.done" => self.finish_function_call(&event)?,
            "error" => return Err(upstream_error(&event)),
            "response.failed" => return Err(response_failed_error(&event)),
            "response.completed" | "response.done" => {
                self.capture_response_id(&event)?;
                self.usage = Some(usage_from_event(&event));
                self.completed = true;
            }
            "response.incomplete" => {
                return Err(protocol_error("upstream response was incomplete"));
            }
            _ => {}
        }

        Ok(())
    }

    fn finish(mut self) -> Result<DecodedResponse, Error> {
        if !self.completed {
            return Err(protocol_error("stream ended before response.completed"));
        }

        if !self.function_calls.is_empty() {
            return Err(protocol_error(
                "stream completed with unfinished function calls",
            ));
        }

        if self
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::ToolCall { .. }))
            && self.response_id.is_none()
            && !self.require_encrypted_reasoning
        {
            return Err(protocol_error("tool calls require a response ID"));
        }

        let pending_calls = self
            .function_call_order
            .iter()
            .map(|item_id| {
                self.completed_calls.remove(item_id).ok_or_else(|| {
                    protocol_error("stream completed with unfinished function calls")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(DecodedResponse {
            parts: self.parts,
            usage: self.usage.unwrap_or_default(),
            response_id: self.response_id,
            pending_calls,
            replay_items: self.replay_items,
        })
    }

    fn completed_usage(&self) -> Option<Usage> {
        self.completed
            .then(|| self.usage.clone().unwrap_or_default())
    }

    fn process_output_item(&mut self, event: &Value) -> Result<(), Error> {
        let item = required_object(event, "item")?;

        match required_string(item, "type")? {
            "reasoning" => self.process_reasoning_item(item)?,
            "message" => {
                required_nonempty_string(item, "id")?;
                required_string(item, "role")?;
                required_array(item, "content")?;
                if self.require_encrypted_reasoning {
                    self.push_replay_item(item.clone())?;
                }
            }
            "function_call" => {
                let id = required_nonempty_string(item, "id")?;
                required_nonempty_string(item, "call_id")?;
                required_string(item, "name")?;
                required_string(item, "arguments")?;
                if self.require_encrypted_reasoning {
                    self.replace_completed_function_call_replay_item(id, item.clone())?;
                }
            }
            _ if self.require_encrypted_reasoning => {
                return Err(protocol_error("unsupported replay output item"));
            }
            _ => {}
        }

        Ok(())
    }

    fn process_reasoning_item(&mut self, item: &Value) -> Result<(), Error> {
        let summaries = required_array(item, "summary")?;

        for summary in summaries {
            if required_string(summary, "type")? == "summary_text" {
                self.parts.push(MessagePart::Reasoning(
                    required_string(summary, "text")?.to_owned(),
                ));
            }
        }

        if self.require_encrypted_reasoning {
            required_nonempty_string(item, "id")?;
            required_nonempty_string(item, "encrypted_content")?;
            self.push_replay_item(item.clone())?;
        }

        Ok(())
    }

    fn add_function_call(&mut self, event: &Value) -> Result<(), Error> {
        let item = required_object(event, "item")?;

        if required_string(item, "type")? != "function_call" {
            return Ok(());
        }

        let id = required_nonempty_string(item, "id")?.to_owned();
        if self.seen_item_ids.insert(id.clone(), ()).is_some() {
            return Err(protocol_error("duplicate function call item"));
        }
        let call_id = required_nonempty_string(item, "call_id")?.to_owned();
        if self.seen_call_ids.insert(call_id.clone(), ()).is_some() {
            return Err(protocol_error("duplicate function call ID"));
        }
        let call = FunctionCall {
            call_id,
            name: required_string(item, "name")?.to_owned(),
            arguments: required_string(item, "arguments")?.to_owned(),
        };

        if self.function_calls.insert(id, call).is_some() {
            return Err(protocol_error("duplicate function call item"));
        }
        self.function_call_order
            .push(required_nonempty_string(item, "id")?.to_owned());

        Ok(())
    }

    fn capture_response_id(&mut self, event: &Value) -> Result<(), Error> {
        let Some(response) = event.get("response") else {
            return Ok(());
        };
        let id = required_nonempty_string(response, "id")?.to_owned();

        if self
            .response_id
            .as_ref()
            .is_some_and(|existing| existing != &id)
        {
            return Err(protocol_error("conflicting response IDs"));
        }

        self.response_id = Some(id);
        Ok(())
    }

    fn append_function_arguments(&mut self, event: &Value) -> Result<(), Error> {
        let id = required_nonempty_string(event, "item_id")?;
        let call = self.function_calls.get_mut(id).ok_or_else(|| {
            protocol_error("function arguments arrived before the function call item")
        })?;

        call.arguments.push_str(required_string(event, "delta")?);

        Ok(())
    }

    fn finish_function_call(&mut self, event: &Value) -> Result<(), Error> {
        let id = required_nonempty_string(event, "item_id")?;
        let mut call = self.function_calls.remove(id).ok_or_else(|| {
            protocol_error("function arguments completed before the function call item")
        })?;

        call.arguments = required_string(event, "arguments")?.to_owned();
        self.completed_calls.insert(
            id.to_owned(),
            PendingToolCall {
                item_id: id.to_owned(),
                call_id: call.call_id.clone(),
            },
        );
        self.push_replay_item(serde_json::json!({
            "type": "function_call",
            "id": id,
            "call_id": call.call_id.clone(),
            "name": call.name.clone(),
            "arguments": call.arguments.clone(),
        }))?;
        self.parts.push(MessagePart::ToolCall {
            id: call.call_id,
            name: call.name,
            input: call.arguments,
        });

        Ok(())
    }

    fn push_replay_item(&mut self, item: Value) -> Result<(), Error> {
        let id = required_nonempty_string(&item, "id")?.to_owned();
        if self.replay_item_positions.contains_key(&id)
            || self.replay_items.len() == MAX_CHATGPT_REPLAY_ITEMS
            || serde_json::to_vec(&item)
                .map_or(true, |bytes| bytes.len() > MAX_CHATGPT_REPLAY_ITEM_BYTES)
        {
            return Err(protocol_error("replay output exceeds provider bounds"));
        }
        self.replay_item_positions
            .insert(id, self.replay_items.len());
        self.replay_items.push(item);
        Ok(())
    }

    fn replace_completed_function_call_replay_item(
        &mut self,
        id: &str,
        item: Value,
    ) -> Result<(), Error> {
        let Some(position) = self.replay_item_positions.get(id).copied() else {
            return Err(protocol_error(
                "completed function call item was not started",
            ));
        };
        if !self
            .completed_function_output_item_ids
            .insert(id.to_owned())
        {
            return Err(protocol_error("duplicate replay output item"));
        }
        if serde_json::to_vec(&item)
            .map_or(true, |bytes| bytes.len() > MAX_CHATGPT_REPLAY_ITEM_BYTES)
        {
            return Err(protocol_error("replay output exceeds provider bounds"));
        }
        self.replay_items[position] = item;
        Ok(())
    }
}

fn function_tools_json(tools: &[OpenAiFunctionTool]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(OpenAiFunctionTool::to_response_api_json)
            .collect(),
    )
}

fn chatgpt_function_tools_json(tools: &[OpenAiFunctionTool]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(OpenAiFunctionTool::to_chatgpt_response_api_json)
            .collect(),
    )
}

fn chatgpt_responses_endpoint(base_url: Option<&str>) -> Result<String, Error> {
    let base_url = base_url
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(DEFAULT_CHATGPT_BASE_URL);
    let mut url = url::Url::parse(base_url)
        .map_err(|_| Error::Provider("ChatGPT HTTP endpoint is invalid".into()))?;
    if url.cannot_be_a_base() || url.host_str().is_none() {
        return Err(Error::Provider("ChatGPT HTTP endpoint is invalid".into()));
    }

    let path = url.path().trim_end_matches('/');
    let path = if path.ends_with("/codex/responses") {
        path.to_owned()
    } else if path.ends_with("/codex") {
        format!("{path}/responses")
    } else {
        format!("{path}/codex/responses")
    };
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.into())
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_error("event is missing a required string field"))
}

fn required_nonempty_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    match required_string(value, field) {
        Ok(candidate) if !candidate.is_empty() => Ok(candidate),
        Ok(_) | Err(_) => Err(protocol_error(
            "event is missing a required non-empty string field",
        )),
    }
}

fn required_object<'a>(value: &'a Value, field: &str) -> Result<&'a Value, Error> {
    value
        .get(field)
        .filter(|candidate| candidate.is_object())
        .ok_or_else(|| protocol_error("event is missing a required object field"))
}

fn required_array<'a>(value: &'a Value, field: &str) -> Result<&'a Vec<Value>, Error> {
    value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_error("event is missing a required array field"))
}

fn usage_from_event(event: &Value) -> Usage {
    let usage = event
        .get("response")
        .and_then(|response| response.get("usage"));
    let field = |name| {
        usage
            .and_then(|usage| usage.get(name))
            .and_then(Value::as_u64)
    };

    Usage {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        total_tokens: field("total_tokens"),
        context_window: None,
    }
}

fn upstream_error(event: &Value) -> Error {
    let _ = event;
    Error::Provider("OpenAI stream failed: upstream provider reported an error".to_owned())
}

fn response_failed_error(event: &Value) -> Error {
    let _ = event;
    Error::Provider("OpenAI stream failed: upstream provider reported an error".to_owned())
}

fn protocol_error(detail: &str) -> Error {
    Error::Provider(format!("OpenAI stream protocol error: {detail}"))
}

fn read_credentials(path: &Path) -> Result<Value, Error> {
    let contents = fs::read(path).map_err(|_| auth_error("credentials file is unavailable"))?;
    serde_json::from_slice(&contents).map_err(|_| auth_error("credentials file is invalid"))
}

fn chatgpt_entry(credentials: &Value) -> Result<&Value, Error> {
    credentials
        .as_object()
        .and_then(|entries| entries.get(CHATGPT_PROVIDER_ID))
        .ok_or_else(|| auth_error("credentials are incomplete"))
}

fn chatgpt_entry_mut(credentials: &mut Value) -> Result<&mut Value, Error> {
    credentials
        .as_object_mut()
        .and_then(|entries| entries.get_mut(CHATGPT_PROVIDER_ID))
        .ok_or_else(|| auth_error("credentials are incomplete"))
}

fn required_credential_string<'a>(entry: &'a Value, field: &str) -> Result<&'a str, Error> {
    entry
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error("credentials are incomplete"))
}

fn write_credentials_atomically(path: &Path, contents: &[u8]) -> Result<(), Error> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| auth_error("credentials path is invalid"))?;

    ensure_private_directory(parent)?;
    let (temporary_path, mut temporary_file) = create_private_temporary_file(parent)?;

    let write_result = (|| {
        temporary_file
            .write_all(contents)
            .map_err(|_| auth_error("refreshed credentials could not be persisted"))?;
        temporary_file
            .sync_all()
            .map_err(|_| auth_error("refreshed credentials could not be persisted"))?;
        drop(temporary_file);

        if fail_before_rename_for_test() {
            return Err(auth_error("refreshed credentials could not be persisted"));
        }

        fs::rename(&temporary_path, path)
            .map_err(|_| auth_error("refreshed credentials could not be persisted"))?;
        set_private_file_permissions(path)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| auth_error("refreshed credentials could not be persisted"))
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }

    write_result
}

fn ensure_private_directory(path: &Path) -> Result<(), Error> {
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|_| auth_error("credentials directory could not be created"))?;
    }

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| auth_error("credentials directory could not be secured"))
}

#[cfg(test)]
fn fail_before_rename_for_test() -> bool {
    FAIL_BEFORE_RENAME.with(|failure| failure.replace(false))
}

#[cfg(not(test))]
fn fail_before_rename_for_test() -> bool {
    false
}

#[cfg(test)]
fn inject_pre_rename_failure() {
    FAIL_BEFORE_RENAME.with(|failure| failure.set(true));
}

fn create_private_temporary_file(parent: &Path) -> Result<(std::path::PathBuf, File), Error> {
    for _ in 0..128 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".auth-{}-{sequence}.json", std::process::id()));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path);

        match file {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                return Err(auth_error(
                    "temporary credentials file could not be created",
                ));
            }
        }
    }

    Err(auth_error(
        "temporary credentials file could not be created",
    ))
}

fn set_private_file_permissions(path: &Path) -> Result<(), Error> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| auth_error("credentials file could not be secured"))
}

fn parse_rfc3339_timestamp(value: &str) -> Option<SystemTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok().map(Into::into)
}

fn timestamp_to_rfc3339(timestamp: SystemTime) -> String {
    let timestamp = OffsetDateTime::from(timestamp);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        timestamp.year(),
        timestamp.month() as u8,
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second()
    )
}

fn jwt_expiry(token: &str) -> Option<SystemTime> {
    let payload = token.split('.').nth(1)?;
    let mut value = 0_u32;
    let mut bits = 0_u8;
    let mut bytes = Vec::new();

    for byte in payload.bytes() {
        let sextet = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32;
        value = (value << 6) | sextet;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((value >> bits) as u8);
            value &= (1 << bits) - 1;
        }
    }

    let seconds = serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_i64()?;
    (seconds >= 0).then(|| std::time::UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

async fn acquire_refresh_file_lock(
    credentials_path: &Path,
    cancellation: &HeadlessTurnCancellation,
) -> Result<RefreshFileLock, HeadlessTurnPortError> {
    let credentials_path = canonical_credential_identity(credentials_path)?;
    let lock_path = refresh_lock_path(&credentials_path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|_| HeadlessTurnPortError::Provider)?;
    set_private_file_permissions(&lock_path).map_err(|_| HeadlessTurnPortError::Provider)?;

    loop {
        stop_before_mapping(cancellation)?;

        let locked = match file.try_lock_exclusive() {
            Ok(locked) => locked,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(_) => return Err(HeadlessTurnPortError::Provider),
        };
        if locked {
            return Ok(RefreshFileLock { _file: file });
        }

        tokio::select! {
            _ = tokio::time::sleep(HTTP_CANCELLATION_POLL_INTERVAL) => {}
            stop = wait_for_stop(cancellation) => return Err(stop),
        }
    }
}

fn canonical_credential_identity(path: &Path) -> Result<PathBuf, HeadlessTurnPortError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(HeadlessTurnPortError::Authentication)?;
    let filename = path
        .file_name()
        .filter(|filename| !filename.is_empty())
        .ok_or(HeadlessTurnPortError::Authentication)?;
    let parent = fs::canonicalize(parent).map_err(|_| HeadlessTurnPortError::Authentication)?;

    Ok(parent.join(filename))
}

fn refresh_lock_path(credentials_path: &Path) -> Result<PathBuf, HeadlessTurnPortError> {
    let parent = credentials_path
        .parent()
        .ok_or(HeadlessTurnPortError::Authentication)?;
    let filename = credentials_path
        .file_name()
        .ok_or(HeadlessTurnPortError::Authentication)?
        .to_string_lossy();

    Ok(parent.join(format!(".{filename}.refresh.lock")))
}

fn is_permanent_refresh_failure(body: &Value) -> bool {
    let error = body
        .get("error")
        .and_then(|error| match error {
            Value::String(value) => Some(value.as_str()),
            Value::Object(_) => error.get("code").and_then(Value::as_str),
            _ => None,
        })
        .unwrap_or_default()
        .to_ascii_lowercase();
    ["invalid_grant", "expired", "reused", "invalidated"]
        .iter()
        .any(|needle| error.contains(needle))
}

fn auth_error(detail: &str) -> Error {
    Error::Auth(format!("ChatGPT authentication required: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn preserves_existing_credentials_and_removes_temporary_file_after_pre_rename_failure() {
        let directory = temporary_directory("pre-rename-failure");
        let credentials_path = directory.join("auth.json");
        let original = credentials();
        fs::write(&credentials_path, &original).expect("credentials should be written");

        inject_pre_rename_failure();

        assert_eq!(
            persist_chatgpt_refresh(
                &credentials_path,
                "synthetic-new-access",
                Some("synthetic-new-refresh"),
                "2026-07-17T13:00:00Z",
            ),
            Err(Error::Auth(
                "ChatGPT authentication required: refreshed credentials could not be persisted"
                    .to_owned()
            ))
        );
        assert_eq!(
            fs::read(&credentials_path).expect("existing credentials should remain readable"),
            original.as_bytes()
        );
        assert!(
            fs::read_dir(&directory)
                .expect("credential directory should remain readable")
                .all(|entry| {
                    !entry
                        .expect("credential directory entry should be readable")
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".auth-")
                })
        );

        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_credential_directory_and_file() {
        use std::os::unix::fs::PermissionsExt;

        let directory = temporary_directory("permissions");
        let credentials_directory = directory.join("credentials");
        let credentials_path = credentials_directory.join("auth.json");

        ensure_private_directory(&credentials_directory)
            .expect("credential directory should be created");
        fs::write(&credentials_path, credentials()).expect("credentials should be written");
        fs::set_permissions(&credentials_directory, fs::Permissions::from_mode(0o755))
            .expect("credential directory permissions should be relaxed for the test");
        fs::set_permissions(&credentials_path, fs::Permissions::from_mode(0o644))
            .expect("credential file permissions should be relaxed for the test");

        persist_chatgpt_refresh(
            &credentials_path,
            "synthetic-new-access",
            Some("synthetic-new-refresh"),
            "2026-07-17T13:00:00Z",
        )
        .expect("refresh should persist credentials");

        assert_eq!(
            fs::metadata(&credentials_path)
                .expect("credential file metadata should be readable")
                .permissions()
                .mode()
                & 0o077,
            0
        );
        assert_eq!(
            fs::metadata(&credentials_directory)
                .expect("credential directory metadata should be readable")
                .permissions()
                .mode()
                & 0o077,
            0
        );

        fs::remove_dir_all(directory).expect("temporary directory should be removed");
    }

    #[test]
    fn resumed_input_preserves_typed_message_boundaries() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                parts: vec![
                    MessagePart::Reasoning("reasoning".into()),
                    MessagePart::ToolCall {
                        id: "call-1".into(),
                        name: "native::read".into(),
                        input: r#"{"path":"notes.md"}"#.into(),
                    },
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: "contents".into(),
                    is_error: false,
                }],
            },
            Message {
                role: Role::User,
                parts: vec![MessagePart::Text("new input".into())],
            },
        ];

        assert_eq!(
            resumed_input("test-model", &messages, &[]).expect("history should encode"),
            vec![
                serde_json::json!({
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "reasoning"}],
                }),
                serde_json::json!({
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "native::read",
                    "arguments": r#"{"path":"notes.md"}"#,
                }),
                serde_json::json!({
                    "type": "function_call_output",
                    "call_id": "call-1",
                    "output": "contents",
                }),
                serde_json::json!({
                    "role": "user",
                    "content": [{"type": "input_text", "text": "new input"}],
                }),
            ]
        );
    }

    #[test]
    fn provider_tool_and_replay_caps_count_utf8_bytes() {
        let bounded = bounded_tool_output(&"€".repeat(MAX_TOOL_OUTPUT_BYTES));
        assert_eq!(
            bounded.len(),
            MAX_TOOL_OUTPUT_BYTES - (MAX_TOOL_OUTPUT_BYTES % 3)
        );
        assert!(bounded.is_char_boundary(bounded.len()));

        let exact = serde_json::json!({
            "type": "function_call_output",
            "output": "x".repeat(MAX_CHATGPT_REPLAY_ITEM_BYTES - 44),
        });
        let oversized = serde_json::json!({
            "type": "function_call_output",
            "output": "x".repeat(MAX_CHATGPT_REPLAY_ITEM_BYTES),
        });

        assert!(validate_chatgpt_replay_history(&[exact]).is_ok());
        assert!(validate_chatgpt_replay_history(&[oversized]).is_err());
    }

    fn credentials() -> String {
        r#"{
            "openai-chatgpt": {
                "access_token": "synthetic-old-access",
                "refresh_token": "synthetic-old-refresh",
                "account_id": "account_123",
                "expires_at": "2026-07-17T11:00:00Z"
            }
        }"#
        .to_owned()
    }

    fn temporary_directory(name: &str) -> std::path::PathBuf {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agens-providers-chatgpt-auth-{name}-{}-{sequence}",
            std::process::id()
        ));

        fs::create_dir(&path).expect("temporary directory should be created");
        path
    }
}

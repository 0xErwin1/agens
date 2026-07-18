use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, SystemTime};

use agens_core::{
    Error, HeadlessTurnCancellation, HeadlessTurnPortError, Message, MessagePart, Role, TurnEvent,
    TurnProvider,
};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const CHATGPT_PROVIDER_ID: &str = "openai-chatgpt";
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);
const HTTP_CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(5);
const DEFAULT_OPENAI_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_SSE_FRAME_BYTES: usize = 64 * 1024;
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
    prompt: String,
    client: reqwest::Client,
    sent_initial_request: bool,
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
            prompt,
            client: reqwest::Client::builder()
                .connect_timeout(request_timeout)
                .build()
                .map_err(|_| Error::Provider("OpenAI HTTP client is unavailable".into()))?,
            sent_initial_request: false,
        })
    }

    async fn request_initial_response(
        &self,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
        let payload = serde_json::json!({
            "model": self.model,
            "input": [{ "role": "user", "content": self.prompt }],
            "stream": true,
        });
        let request = self
            .client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .header("Accept", "text/event-stream")
            .json(&payload)
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
        if !response.status().is_success() {
            return Err(HeadlessTurnPortError::Provider);
        }

        decode_http_response_stream(response, cancellation).await
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
        if self.sent_initial_request {
            return Err(HeadlessTurnPortError::Provider);
        }

        self.sent_initial_request = true;
        self.request_initial_response(cancellation).await
    }
}

async fn decode_http_response_stream(
    mut response: reqwest::Response,
    cancellation: &HeadlessTurnCancellation,
) -> Result<Vec<MessagePart>, HeadlessTurnPortError> {
    let mut decoder = OpenAiResponseDecoder::default();
    let mut frame = Vec::new();

    loop {
        let next_chunk = tokio::select! {
            chunk = response.chunk() => {
                stop_before_mapping(cancellation)?;
                chunk.map_err(|_| HeadlessTurnPortError::Provider)?
            }
            stop = wait_for_stop(cancellation) => return Err(stop),
        };
        let Some(chunk) = next_chunk else {
            let completed = decoder.finish();
            stop_before_mapping(cancellation)?;
            return completed.map_err(|_| HeadlessTurnPortError::Provider);
        };

        for byte in chunk {
            if byte == b'\n' {
                let processed = process_sse_frame(&mut decoder, &mut frame);
                stop_before_mapping(cancellation)?;
                processed.map_err(|_| HeadlessTurnPortError::Provider)?;
                continue;
            }

            if frame.len() == MAX_SSE_FRAME_BYTES {
                stop_before_mapping(cancellation)?;
                return Err(HeadlessTurnPortError::Provider);
            }
            frame.push(byte);
        }

        stop_before_mapping(cancellation)?;
    }
}

fn process_sse_frame(decoder: &mut OpenAiResponseDecoder, frame: &mut Vec<u8>) -> Result<(), ()> {
    if frame.last() == Some(&b'\r') {
        frame.pop();
    }

    let data = frame
        .strip_prefix(b"data:")
        .map(|value| value.strip_prefix(b" ").unwrap_or(value));
    if let Some(data) = data.filter(|data| !data.is_empty()) {
        let event = std::str::from_utf8(data).map_err(|_| ())?;
        decoder.process(event).map_err(|_| ())?;
    }
    frame.clear();
    Ok(())
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

    if required_credential_string(entry, "access_token").is_err() || now >= expires_at {
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
    if access_token.is_empty() || refresh_token.is_some_and(str::is_empty) {
        return Err(auth_error("refreshed credentials are incomplete"));
    }

    if parse_rfc3339_timestamp(expires_at).is_none() {
        return Err(auth_error("refreshed credentials are invalid"));
    }

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

    let contents = serde_json::to_vec(&credentials)
        .map_err(|_| auth_error("refreshed credentials could not be encoded"))?;
    write_credentials_atomically(credentials_path, &contents)
}

pub fn encode_openai_response_request(model: &str, message: &Message) -> Result<String, Error> {
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

    Ok(serde_json::json!({
        "model": model,
        "input": [{ "role": role, "content": content }],
        "stream": true,
    })
    .to_string())
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

    decoder.finish()
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

                return decoder.finish();
            }
        }
    }
}

#[derive(Default)]
struct OpenAiResponseDecoder {
    parts: Vec<MessagePart>,
    function_calls: BTreeMap<String, FunctionCall>,
    completed: bool,
}

struct FunctionCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl OpenAiResponseDecoder {
    fn process(&mut self, event_json: &str) -> Result<(), Error> {
        let event: Value =
            serde_json::from_str(event_json).map_err(|_| protocol_error("invalid event JSON"))?;
        let event_type = required_string(&event, "type")?;

        match event_type {
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
            "response.completed" => self.completed = true,
            _ => {}
        }

        Ok(())
    }

    fn finish(self) -> Result<Vec<MessagePart>, Error> {
        if !self.completed {
            return Err(protocol_error("stream ended before response.completed"));
        }

        if !self.function_calls.is_empty() {
            return Err(protocol_error(
                "stream completed with unfinished function calls",
            ));
        }

        Ok(self.parts)
    }

    fn process_output_item(&mut self, event: &Value) -> Result<(), Error> {
        let item = required_object(event, "item")?;

        if required_string(item, "type")? != "reasoning" {
            return Ok(());
        }

        let summaries = required_array(item, "summary")?;

        for summary in summaries {
            if required_string(summary, "type")? == "summary_text" {
                self.parts.push(MessagePart::Reasoning(
                    required_string(summary, "text")?.to_owned(),
                ));
            }
        }

        Ok(())
    }

    fn add_function_call(&mut self, event: &Value) -> Result<(), Error> {
        let item = required_object(event, "item")?;

        if required_string(item, "type")? != "function_call" {
            return Ok(());
        }

        let id = required_string(item, "id")?.to_owned();
        let call = FunctionCall {
            call_id: required_string(item, "call_id")?.to_owned(),
            name: required_string(item, "name")?.to_owned(),
            arguments: required_string(item, "arguments")?.to_owned(),
        };

        if self.function_calls.insert(id, call).is_some() {
            return Err(protocol_error("duplicate function call item"));
        }

        Ok(())
    }

    fn append_function_arguments(&mut self, event: &Value) -> Result<(), Error> {
        let id = required_string(event, "item_id")?;
        let call = self.function_calls.get_mut(id).ok_or_else(|| {
            protocol_error("function arguments arrived before the function call item")
        })?;

        call.arguments.push_str(required_string(event, "delta")?);

        Ok(())
    }

    fn finish_function_call(&mut self, event: &Value) -> Result<(), Error> {
        let id = required_string(event, "item_id")?;
        let mut call = self.function_calls.remove(id).ok_or_else(|| {
            protocol_error("function arguments completed before the function call item")
        })?;

        call.arguments = required_string(event, "arguments")?.to_owned();
        self.parts.push(MessagePart::ToolCall {
            id: call.call_id,
            name: call.name,
            input: call.arguments,
        });

        Ok(())
    }
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| protocol_error("event is missing a required string field"))
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
    if path.exists() {
        return Ok(());
    }

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
        .map_err(|_| auth_error("credentials directory could not be created"))
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

fn parse_rfc3339_timestamp(value: &str) -> Option<SystemTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok().map(Into::into)
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

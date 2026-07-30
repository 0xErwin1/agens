//! Signing in to a ChatGPT subscription.
//!
//! Two flows reach the same credentials: a browser round trip and a device
//! code. Both report progress through a callback, so a terminal, a headless run
//! or a test observes the same sequence without this crate knowing which.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use agens_providers::chatgpt_login::{
    ChatGptCredentials, ChatGptDeviceCodeLoginOptions, ChatGptLoginOptions, LoginCancellation,
    LoginError, device_code_login_with_progress, login, remove_provider_entry,
    upsert_chatgpt_credentials,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatGptAuthFlow {
    Browser,
    Device,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatGptAuthProgress {
    BrowserUrl(String),
    DeviceCode {
        verification_url: String,
        user_code: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatGptAuthError {
    message: &'static str,
    action: &'static str,
}

impl ChatGptAuthError {
    pub fn message(&self) -> &'static str {
        self.message
    }

    pub fn action(&self) -> &'static str {
        self.action
    }

    fn login(error: LoginError) -> Self {
        let action = match error {
            LoginError::Cancelled => "Run authentication again when ready.",
            LoginError::TimedOut => "Retry authentication.",
            LoginError::Authentication("authorization was denied") => {
                "Retry and approve access, or use device authentication."
            }
            LoginError::Authentication(_) => "Retry authentication or use device authentication.",
            LoginError::TokenTransport | LoginError::TokenStatus => "Retry authentication.",
            LoginError::TokenFormat | LoginError::Account | LoginError::Expiry => {
                "Run authentication again."
            }
        };
        Self {
            message: error.stage_message(),
            action,
        }
    }

    fn persistence() -> Self {
        Self {
            message: "ChatGPT credentials could not be saved",
            action: "Check credential-file access and retry authentication.",
        }
    }
}
type ProgressSink = Arc<dyn Fn(ChatGptAuthProgress) + Send + Sync>;
type BrowserOpener = Arc<dyn Fn(&str) -> std::io::Result<()> + Send + Sync>;
type Authenticator = dyn Fn(ChatGptAuthFlow, LoginCancellation, ProgressSink) -> Result<ChatGptCredentials, LoginError>
    + Send
    + Sync;

#[derive(Clone)]
pub struct ChatGptAuthCoordinator {
    authenticate: Arc<Authenticator>,
    open_browser: BrowserOpener,
}

impl ChatGptAuthCoordinator {
    pub fn production() -> Self {
        let open_browser: BrowserOpener = Arc::new(|url| {
            std::process::Command::new("xdg-open")
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map(|_| ())
        });
        let browser_login = Arc::clone(&open_browser);
        Self::with_authenticator_and_browser_opener(
            move |flow, cancellation, publish| match flow {
                ChatGptAuthFlow::Browser => {
                    let progress = Arc::clone(&publish);
                    let options = ChatGptLoginOptions::new(
                        Arc::clone(&browser_login),
                        Arc::new(move |url| {
                            progress(ChatGptAuthProgress::BrowserUrl(url.to_owned()));
                        }),
                    );
                    login(options, cancellation)
                }
                ChatGptAuthFlow::Device => device_code_login_with_progress(
                    ChatGptDeviceCodeLoginOptions::default(),
                    cancellation,
                    move |verification_url, user_code| {
                        publish(ChatGptAuthProgress::DeviceCode {
                            verification_url: verification_url.to_owned(),
                            user_code: user_code.to_owned(),
                        });
                    },
                )
                .map(|result| result.credentials),
            },
            open_browser,
        )
    }

    pub fn with_authenticator(
        authenticate: impl Fn(
            ChatGptAuthFlow,
            LoginCancellation,
            ProgressSink,
        ) -> Result<ChatGptCredentials, LoginError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self::with_authenticator_and_browser_opener(authenticate, Arc::new(|_| Ok(())))
    }

    pub fn with_authenticator_and_browser_opener(
        authenticate: impl Fn(
            ChatGptAuthFlow,
            LoginCancellation,
            ProgressSink,
        ) -> Result<ChatGptCredentials, LoginError>
        + Send
        + Sync
        + 'static,
        open_browser: BrowserOpener,
    ) -> Self {
        Self {
            authenticate: Arc::new(authenticate),
            open_browser,
        }
    }

    /// Opens only the base verification URL, preserving the active device polling flow.
    pub fn open_device_auth_url(&self, url: &str) -> Result<(), ChatGptAuthError> {
        (self.open_browser)(url).map_err(|_| ChatGptAuthError {
            message: "ChatGPT verification page could not be opened",
            action: "Copy the verification link and open it in a browser.",
        })
    }

    pub fn login(
        &self,
        path: &Path,
        flow: ChatGptAuthFlow,
        cancellation: LoginCancellation,
        deadline: Instant,
        publish: impl Fn(ChatGptAuthProgress) + Send + Sync + 'static,
    ) -> Result<(), ChatGptAuthError> {
        let credentials = (self.authenticate)(flow, cancellation.clone(), Arc::new(publish))
            .map_err(ChatGptAuthError::login)?;
        if cancellation.is_cancelled() {
            return Err(ChatGptAuthError::login(LoginError::Cancelled));
        }
        if Instant::now() >= deadline {
            return Err(ChatGptAuthError::login(LoginError::TimedOut));
        }
        upsert_chatgpt_credentials(path, &credentials).map_err(|_| ChatGptAuthError::persistence())
    }

    pub fn disconnect(&self, path: &Path) -> Result<bool, ChatGptAuthError> {
        remove_provider_entry(path, "openai-chatgpt").map_err(|_| ChatGptAuthError::persistence())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use agens_providers::chatgpt_login::{ChatGptCredentials, LoginCancellation, LoginError};

    use super::*;

    #[test]
    fn auth_login_coordinator_selects_flow_and_securely_merges_credentials() {
        let temporary = TemporaryDirectory::new("flow-merge");
        let credentials = temporary.path().join("auth.json");
        std::fs::write(&credentials, r#"{"other":{"api_key":"preserved"}}"#).unwrap();
        let selected = Arc::new(Mutex::new(Vec::new()));
        let coordinator = ChatGptAuthCoordinator::with_authenticator({
            let selected = Arc::clone(&selected);
            move |flow, _, publish| {
                selected.lock().unwrap().push(flow);
                match flow {
                    ChatGptAuthFlow::Browser => publish(ChatGptAuthProgress::BrowserUrl(
                        "https://auth.example/connect".into(),
                    )),
                    ChatGptAuthFlow::Device => publish(ChatGptAuthProgress::DeviceCode {
                        verification_url: "https://auth.example/device".into(),
                        user_code: "ABCD-EFGH".into(),
                    }),
                }
                Ok(test_credentials("new-access"))
            }
        });

        let progress = Arc::new(Mutex::new(Vec::new()));
        for flow in [ChatGptAuthFlow::Browser, ChatGptAuthFlow::Device] {
            coordinator
                .login(
                    &credentials,
                    flow,
                    LoginCancellation::new(),
                    Instant::now() + Duration::from_secs(1),
                    {
                        let progress = Arc::clone(&progress);
                        move |event| progress.lock().unwrap().push(event)
                    },
                )
                .unwrap();
        }

        assert_eq!(
            *selected.lock().unwrap(),
            vec![ChatGptAuthFlow::Browser, ChatGptAuthFlow::Device]
        );
        let progress = progress.lock().unwrap();
        assert!(matches!(progress[0], ChatGptAuthProgress::BrowserUrl(_)));
        assert!(matches!(
            progress[1],
            ChatGptAuthProgress::DeviceCode { .. }
        ));
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&credentials).unwrap()).unwrap();
        assert_eq!(stored["other"]["api_key"], "preserved");
        assert_eq!(stored["openai-chatgpt"]["access_token"], "new-access");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&credentials)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn auth_login_coordinator_rejects_late_completion_and_sanitizes_failures() {
        let temporary = TemporaryDirectory::new("late-cancel");
        let credentials = temporary.path().join("auth.json");
        std::fs::write(&credentials, r#"{"other":{"api_key":"preserved"}}"#).unwrap();
        let before = std::fs::read(&credentials).unwrap();
        let coordinator = ChatGptAuthCoordinator::with_authenticator(|_, cancellation, _| {
            cancellation.cancel();
            Ok(test_credentials("must-not-persist"))
        });
        let error = coordinator
            .login(
                &credentials,
                ChatGptAuthFlow::Browser,
                LoginCancellation::new(),
                Instant::now() + Duration::from_secs(1),
                |_| {},
            )
            .unwrap_err();

        assert_eq!(error.message(), "ChatGPT login was cancelled");
        assert_eq!(error.action(), "Run authentication again when ready.");
        assert_eq!(std::fs::read(&credentials).unwrap(), before);

        let coordinator =
            ChatGptAuthCoordinator::with_authenticator(|_, _, _| Err(LoginError::TokenTransport));
        let error = coordinator
            .login(
                &credentials,
                ChatGptAuthFlow::Device,
                LoginCancellation::new(),
                Instant::now() + Duration::from_secs(1),
                |_| {},
            )
            .unwrap_err();
        assert_eq!(
            error.message(),
            "ChatGPT token request failed; check the network and retry"
        );
        assert_eq!(error.action(), "Retry authentication.");
        assert!(!format!("{} {}", error.message(), error.action()).contains("must-not-persist"));
    }

    fn test_credentials(access_token: &str) -> ChatGptCredentials {
        ChatGptCredentials {
            access_token: access_token.into(),
            refresh_token: "refresh".into(),
            account_id: "account".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
        }
    }

    struct TemporaryDirectory {
        path: std::path::PathBuf,
    }

    impl TemporaryDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "agens-auth-coordinator-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

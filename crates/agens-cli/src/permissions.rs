//! Production permission evaluation: the native-tool permission gate, the interactive
//! (TTY/TUI) prompt resolver, and the config-rule-to-policy translation they share with
//! the tool dispatcher in `dispatch.rs`.

pub(crate) mod prompt;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use agens_config::{ConfigPermissionDecision, ConfigPermissionRule, ConfigPermissionScope};
use agens_core::{
    FactPath, HeadlessPermissionGate, HeadlessPermissionResolver, HeadlessToolCall,
    HeadlessTurnCancellation, HeadlessTurnPortError, PermissionDecision, PermissionMode,
    PermissionPattern, PermissionPolicy, PermissionRule, PermissionSession, ToolInput, ToolOutcome,
    ToolResultFacts,
};
use agens_store::PermissionGrantStore;
use agens_tools::{
    AuthorizedToolCall, EffectiveCapabilitySet, PermissionPromptContext, ToolDispatchRequest,
    ToolDispatcher, ToolEvaluationOutcome,
};

use crate::error::CliError;
use crate::tools::runtime::is_dangerous_child_native_tool;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NativePermissionTarget {
    Command(String),
    Path(String),
    Pattern(String),
    Url(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NativePermissionTargetError {
    UnknownTool,
    ArgumentsNotObject,
    InvalidField(&'static str),
    FieldTooLong(&'static str),
}

impl fmt::Display for NativePermissionTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTool => formatter.write_str("unknown native tool"),
            Self::ArgumentsNotObject => {
                formatter.write_str("native tool arguments must be an object")
            }
            Self::InvalidField(field) => write!(formatter, "native tool {field} is invalid"),
            Self::FieldTooLong(field) => {
                write!(formatter, "native tool {field} exceeds size limit")
            }
        }
    }
}

impl NativePermissionTarget {
    pub(crate) fn parse(
        tool: &str,
        arguments: &serde_json::Value,
    ) -> Result<Self, NativePermissionTargetError> {
        let arguments = arguments
            .as_object()
            .ok_or(NativePermissionTargetError::ArgumentsNotObject)?;

        let field = |field| native_permission_target_field(arguments, field);

        match tool {
            "native::bash" => field("command").map(Self::Command),
            "native::read" | "native::write" | "native::edit" | "native::list"
            | "native::search" => field("path").map(Self::Path),
            "native::glob" => field("pattern").map(Self::Pattern),
            "native::grep" => {
                if arguments.contains_key("path") {
                    field("path")?;
                }

                field("pattern").map(Self::Pattern)
            }
            "native::webfetch" => field("url").map(Self::Url),
            _ => Err(NativePermissionTargetError::UnknownTool),
        }
    }

    pub(crate) fn into_value(self) -> String {
        match self {
            Self::Command(value) | Self::Path(value) | Self::Pattern(value) | Self::Url(value) => {
                value
            }
        }
    }
}

fn native_permission_target_field(
    arguments: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<String, NativePermissionTargetError> {
    let value = arguments
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or(NativePermissionTargetError::InvalidField(field))?;

    if value.trim().is_empty() {
        return Err(NativePermissionTargetError::InvalidField(field));
    }

    if value.len() > agens_core::MAX_PERMISSION_TARGET_BYTES {
        return Err(NativePermissionTargetError::FieldTooLong(field));
    }

    Ok(value.to_owned())
}

pub(crate) trait ParseToolInput: Sized {
    fn parse(name: &str, raw: &str) -> Self;
}

impl ParseToolInput for agens_core::ToolInput {
    fn parse(name: &str, raw: &str) -> Self {
        let fallback = || Self::Other {
            name: name.to_owned(),
            raw: raw.to_owned(),
        };

        let Ok(serde_json::Value::Object(arguments)) = serde_json::from_str(raw) else {
            return fallback();
        };

        let field = |field| native_permission_target_field(&arguments, field).ok();

        match name {
            "read" => field("path").map(|path| Self::Read { path }),
            "write" => field("path").map(|path| Self::Write { path }),
            "edit" => field("path").map(|path| Self::Edit { path }),
            "list" => field("path").map(|path| Self::List { path }),
            "search" => field("path").map(|path| Self::Search { path }),
            "glob" => field("pattern").map(|pattern| Self::Glob {
                pattern,
                path: field("path"),
            }),
            "grep" => field("pattern").map(|pattern| Self::Grep {
                pattern,
                path: field("path"),
            }),
            "bash" => field("command").map(|command| Self::Bash { command }),
            "webfetch" => field("url").map(|url| Self::WebFetch { url }),
            "skill" => field("skill").map(|skill| Self::Skill { skill }),
            _ => None,
        }
        .unwrap_or_else(fallback)
    }
}

pub(crate) struct AllowedNativeCall {
    pub(crate) name: String,
    pub(crate) input: String,
    pub(crate) handle: AuthorizedToolCall,
}

pub(crate) type SharedToolDispatcher = Arc<Mutex<ToolDispatcher>>;
type SharedProjectPermissionGrants = Arc<Mutex<Vec<agens_core::ProjectPermissionGrant>>>;
type PendingPermissionPrompts = Arc<Mutex<BTreeMap<String, PermissionPromptContext>>>;

pub(crate) struct ProductionPermissionGate {
    pub(crate) policy: PermissionPolicy,
    pub(crate) grants: SharedProjectPermissionGrants,
    session: PermissionSession,
    project: String,
    dispatcher: SharedToolDispatcher,
    allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
    prompts: PendingPermissionPrompts,
    dangerous_override: bool,
}

impl ProductionPermissionGate {
    pub(crate) fn new(
        policy: PermissionPolicy,
        grants: SharedProjectPermissionGrants,
        session: PermissionSession,
        project: String,
        dispatcher: SharedToolDispatcher,
        allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
        prompts: PendingPermissionPrompts,
    ) -> Self {
        Self {
            policy,
            grants,
            session,
            project,
            dispatcher,
            allowed,
            prompts,
            dangerous_override: false,
        }
    }

    pub(crate) fn with_dangerous_override(mut self, dangerous_override: bool) -> Self {
        self.dangerous_override = dangerous_override;
        self
    }
}

impl HeadlessPermissionGate for ProductionPermissionGate {
    fn evaluate(
        &mut self,
        call: &HeadlessToolCall,
        _cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
    {
        let result = self
            .grants
            .lock()
            .map_err(|_| HeadlessTurnPortError::Permission)
            .and_then(|grants| {
                self.dispatcher
                    .lock()
                    .map_err(|_| HeadlessTurnPortError::Permission)
                    .and_then(|dispatcher| {
                        dispatcher
                            .evaluate_with_policy_override(
                                &self.policy,
                                &grants,
                                &self.session,
                                ToolDispatchRequest::new(
                                    &self.project,
                                    &call.name,
                                    parse_tool_input(call)?,
                                ),
                                self.dangerous_override
                                    && is_dangerous_child_native_tool(&call.name),
                            )
                            .map_err(|_| HeadlessTurnPortError::Permission)
                    })
            })
            .and_then(|outcome| match outcome {
                ToolEvaluationOutcome::Authorized(handle) => self
                    .allowed
                    .lock()
                    .map_err(|_| HeadlessTurnPortError::Permission)
                    .map(|mut allowed| {
                        allowed.insert(
                            call.id.clone(),
                            AllowedNativeCall {
                                name: call.name.clone(),
                                input: call.input.clone(),
                                handle,
                            },
                        );
                        PermissionDecision::Allow
                    }),
                ToolEvaluationOutcome::Denied => Ok(PermissionDecision::Deny),
                ToolEvaluationOutcome::PromptRequired(context) => self
                    .prompts
                    .lock()
                    .map_err(|_| HeadlessTurnPortError::Permission)
                    .map(|mut prompts| {
                        prompts.insert(call.id.clone(), context);
                        PermissionDecision::Ask
                    }),
            });
        std::future::ready(result)
    }

    /// Reports the path a denied `write` or `edit` targeted, or that a
    /// `bash` call was denied. The engine loop short-circuits a denial
    /// before any tool runs, so this is the only vantage point left from
    /// which the harness can still surface what a denied call touched.
    ///
    /// `call.name` carries its dispatcher prefix (`native::`/`mcp::`); the
    /// prefix is stripped before parsing so the bare name matches
    /// `ToolInput::parse`'s vocabulary, mirroring the same strip performed
    /// when reconstructing a saved session's tool history.
    fn denial_facts(&self, call: &HeadlessToolCall) -> Option<ToolResultFacts> {
        let bare = call
            .name
            .strip_prefix("native::")
            .or_else(|| call.name.strip_prefix("mcp::"))
            .unwrap_or(call.name.as_str());

        match bare {
            "write" => Some(ToolResultFacts::Write {
                path: denied_input_path(ToolInput::parse(bare, &call.input)),
                outcome: ToolOutcome::Denied,
                written: None,
            }),
            "edit" => Some(ToolResultFacts::Edit {
                path: denied_input_path(ToolInput::parse(bare, &call.input)),
                outcome: ToolOutcome::Denied,
                changed: None,
            }),
            "bash" => Some(ToolResultFacts::Bash {
                outcome: ToolOutcome::Denied,
                exit_code: None,
            }),
            _ => None,
        }
    }
}

/// Extracts the path a `write` or `edit` call reported, or an unrepresentable
/// `FactPath` when the call's input did not parse into that shape at all —
/// a malformed payload for a known tool must not fabricate a path, but the
/// denial itself is still reported by its caller.
fn denied_input_path(parsed: ToolInput) -> FactPath {
    match parsed {
        ToolInput::Write { path } | ToolInput::Edit { path } => FactPath::new(&path),
        _ => FactPath::new(""),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermissionPromptAnswer {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
    Cancel,
}

pub(crate) trait PermissionPrompter: Send {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError>;
}

pub(crate) struct ProductionPermissionResolver<P> {
    prompt: P,
    grant_store: PermissionGrantStore,
    grants: SharedProjectPermissionGrants,
    prompts: PendingPermissionPrompts,
    pub(crate) authorization: ProductionPromptAuthorization,
}

pub(crate) struct ProductionPromptAuthorization {
    pub(crate) policy: PermissionPolicy,
    pub(crate) session: PermissionSession,
    pub(crate) project: String,
    pub(crate) dispatcher: SharedToolDispatcher,
    pub(crate) allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
}

impl<P> ProductionPermissionResolver<P> {
    pub(crate) fn new(
        prompt: P,
        grant_store: PermissionGrantStore,
        grants: SharedProjectPermissionGrants,
        prompts: PendingPermissionPrompts,
        authorization: ProductionPromptAuthorization,
    ) -> Self {
        Self {
            prompt,
            grant_store,
            grants,
            prompts,
            authorization,
        }
    }

    fn authorize_prompted_allow(
        &self,
        call: &HeadlessToolCall,
        ephemeral_grant: Option<agens_core::ProjectPermissionGrant>,
    ) -> Result<PermissionDecision, HeadlessTurnPortError> {
        let mut grants = self
            .grants
            .lock()
            .map_err(|_| HeadlessTurnPortError::Permission)?
            .clone();
        if let Some(grant) = ephemeral_grant {
            grants.push(grant);
        }

        let outcome = self
            .authorization
            .dispatcher
            .lock()
            .map_err(|_| HeadlessTurnPortError::Permission)?
            .evaluate(
                &self.authorization.policy,
                &grants,
                &self.authorization.session,
                ToolDispatchRequest::new(
                    &self.authorization.project,
                    &call.name,
                    parse_tool_input(call)?,
                ),
            )
            .map_err(|_| HeadlessTurnPortError::Permission)?;

        match outcome {
            ToolEvaluationOutcome::Authorized(handle) => self
                .authorization
                .allowed
                .lock()
                .map_err(|_| HeadlessTurnPortError::Permission)
                .map(|mut allowed| {
                    allowed.insert(
                        call.id.clone(),
                        AllowedNativeCall {
                            name: call.name.clone(),
                            input: call.input.clone(),
                            handle,
                        },
                    );
                    PermissionDecision::Allow
                }),
            ToolEvaluationOutcome::Denied => Ok(PermissionDecision::Deny),
            ToolEvaluationOutcome::PromptRequired(_) => Err(HeadlessTurnPortError::Permission),
        }
    }
}

impl<P: PermissionPrompter> HeadlessPermissionResolver for ProductionPermissionResolver<P> {
    fn resolve(
        &mut self,
        call: &HeadlessToolCall,
        cancellation: &HeadlessTurnCancellation,
    ) -> impl std::future::Future<Output = Result<PermissionDecision, HeadlessTurnPortError>> + Send
    {
        let result = (|| {
            if cancellation.is_cancelled() {
                return Err(HeadlessTurnPortError::Cancelled);
            }
            if cancellation.is_expired() {
                return Err(HeadlessTurnPortError::TimedOut);
            }

            let context = self
                .prompts
                .lock()
                .map_err(|_| HeadlessTurnPortError::Permission)?
                .remove(&call.id)
                .ok_or(HeadlessTurnPortError::Permission)?;
            let answer = self.prompt.prompt(&context, cancellation)?;

            if cancellation.is_cancelled() || answer == PermissionPromptAnswer::Cancel {
                return Err(HeadlessTurnPortError::Cancelled);
            }
            if cancellation.is_expired() {
                return Err(HeadlessTurnPortError::TimedOut);
            }

            let decision = match answer {
                PermissionPromptAnswer::AllowOnce => {
                    let grant = agens_core::ProjectPermissionGrant::allow(
                        context.project_id,
                        PermissionPattern::Exact(context.qualified_tool_name),
                        PermissionPattern::Exact(context.target_identifier),
                    );
                    self.authorize_prompted_allow(call, Some(grant))?
                }
                PermissionPromptAnswer::DenyOnce => PermissionDecision::Deny,
                PermissionPromptAnswer::AllowAlways | PermissionPromptAnswer::DenyAlways => {
                    let decision = if answer == PermissionPromptAnswer::AllowAlways {
                        PermissionDecision::Allow
                    } else {
                        PermissionDecision::Deny
                    };
                    let grant = agens_core::ProjectPermissionGrant::new(
                        context.project_id,
                        decision,
                        PermissionPattern::Exact(context.qualified_tool_name),
                        PermissionPattern::Exact(context.target_identifier),
                    );
                    self.grant_store
                        .append_grants(std::slice::from_ref(&grant))
                        .map_err(|_| HeadlessTurnPortError::Permission)?;
                    self.grants
                        .lock()
                        .map_err(|_| HeadlessTurnPortError::Permission)?
                        .push(grant);
                    if decision == PermissionDecision::Allow {
                        self.authorize_prompted_allow(call, None)?
                    } else {
                        decision
                    }
                }
                PermissionPromptAnswer::Cancel => unreachable!(),
            };
            Ok(decision)
        })();
        std::future::ready(result)
    }
}

pub(crate) fn permission_policy(
    rules: &[ConfigPermissionRule],
    project: &str,
    mode: PermissionMode,
    dispatcher: &SharedToolDispatcher,
    effective_capabilities: Option<&EffectiveCapabilitySet>,
) -> Result<PermissionPolicy, CliError> {
    let mut rules = rules
        .iter()
        .map(|rule| {
            let decision = match rule.decision {
                ConfigPermissionDecision::Allow => PermissionDecision::Allow,
                ConfigPermissionDecision::Deny => PermissionDecision::Deny,
            };
            let configured = configured_tool_name(&rule.tool_pattern)?;
            let tool = dispatcher
                .lock()
                .map_err(|_| CliError::configuration("tool catalog is invalid"))?
                .canonical_identity(&configured)
                .map(|identity| PermissionPattern::Exact(identity.as_str().to_owned()))
                .ok_or_else(|| CliError::configuration("permission configuration is invalid"))?;
            let target = match &rule.target_pattern {
                Some(pattern) => PermissionPattern::glob(pattern.clone())
                    .map_err(|_| CliError::configuration("permission configuration is invalid"))?,
                None => PermissionPattern::Any,
            };
            Ok(match rule.scope {
                ConfigPermissionScope::Global => PermissionRule::global(decision, tool, target),
                ConfigPermissionScope::Project => {
                    PermissionRule::project(project, decision, tool, target)
                }
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    if let Some(capabilities) = effective_capabilities {
        rules.extend(capabilities.permission_rules());
    }
    Ok(PermissionPolicy::new(mode, rules))
}

fn configured_tool_name(name: &str) -> Result<String, CliError> {
    match name {
        "read" => Ok("native::read".to_owned()),
        "write" | "edit" => Ok("native::write".to_owned()),
        "list" => Ok("native::list".to_owned()),
        "search" => Ok("native::search".to_owned()),
        "bash" => Ok("native::bash".to_owned()),
        name => Ok(name.to_owned()),
    }
}

fn parse_tool_input(call: &HeadlessToolCall) -> Result<serde_json::Value, HeadlessTurnPortError> {
    serde_json::from_str(&call.input).map_err(|_| HeadlessTurnPortError::Permission)
}

pub(crate) fn sanitize_permission_target(tool: &str, target: &str) -> String {
    if tool == "native::bash" {
        return "[command redacted]".into();
    }

    if serde_json::from_str::<serde_json::Value>(target).is_ok() {
        return "[redacted]".into();
    }

    if let Some((scheme, remainder)) = target.split_once("://") {
        let remainder = remainder.split(['?', '#']).next().unwrap_or_default();
        let (authority, path) = remainder.split_once('/').unwrap_or((remainder, ""));
        let authority = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        return format!("{scheme}://{authority}/{path}");
    }

    if contains_sensitive_marker(target) {
        return "[redacted]".into();
    }

    target.to_owned()
}

pub(crate) fn contains_sensitive_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    ["api_key", "authorization", "password", "secret", "token"]
        .iter()
        .any(|marker| value.contains(marker))
}

#[cfg(test)]
mod tests {
    use agens_core::{HeadlessTurnError, MessagePart, TurnEvent, TurnState};
    use agens_tools::{DispatchTool, RemoteToolMetadata, ToolExecutionContext, ToolOutput};

    use super::*;
    use crate::test_support::{
        ProductionBatchInput, batch_call, native_batch_call, run_production_batch,
        run_production_batch_with_policy,
    };

    #[test]
    fn native_permission_target_projects_each_registered_tool_to_its_canonical_field() {
        let cases = [
            (
                "native::bash",
                serde_json::json!({"command": "git status"}),
                NativePermissionTarget::Command("git status".into()),
            ),
            (
                "native::read",
                serde_json::json!({"path": "notes.md"}),
                NativePermissionTarget::Path("notes.md".into()),
            ),
            (
                "native::write",
                serde_json::json!({"path": "notes.md", "content": "body"}),
                NativePermissionTarget::Path("notes.md".into()),
            ),
            (
                "native::edit",
                serde_json::json!({"path": "notes.md", "old": "old", "new": "new"}),
                NativePermissionTarget::Path("notes.md".into()),
            ),
            (
                "native::list",
                serde_json::json!({"path": "src"}),
                NativePermissionTarget::Path("src".into()),
            ),
            (
                "native::search",
                serde_json::json!({"path": "src", "query": "permission"}),
                NativePermissionTarget::Path("src".into()),
            ),
            (
                "native::glob",
                serde_json::json!({"pattern": "src/**/*.rs"}),
                NativePermissionTarget::Pattern("src/**/*.rs".into()),
            ),
            (
                "native::grep",
                serde_json::json!({"pattern": "permission"}),
                NativePermissionTarget::Pattern("permission".into()),
            ),
            (
                "native::webfetch",
                serde_json::json!({"url": "https://example.test/docs"}),
                NativePermissionTarget::Url("https://example.test/docs".into()),
            ),
        ];

        for (tool, arguments, expected) in cases {
            assert_eq!(
                NativePermissionTarget::parse(tool, &arguments),
                Ok(expected)
            );
        }
    }

    #[test]
    fn native_permission_target_keeps_grep_path_separate_from_its_pattern() {
        assert_eq!(
            NativePermissionTarget::parse(
                "native::grep",
                &serde_json::json!({"pattern": "TODO", "path": "crates/agens-cli"}),
            ),
            Ok(NativePermissionTarget::Pattern("TODO".into()))
        );
    }

    #[test]
    fn native_permission_target_rejects_invalid_target_fields_for_every_registered_tool() {
        let too_long = "x".repeat(agens_core::MAX_PERMISSION_TARGET_BYTES + 1);

        for (tool, field) in [
            ("native::bash", "command"),
            ("native::read", "path"),
            ("native::write", "path"),
            ("native::edit", "path"),
            ("native::list", "path"),
            ("native::search", "path"),
            ("native::glob", "pattern"),
            ("native::grep", "pattern"),
            ("native::webfetch", "url"),
        ] {
            assert_eq!(
                NativePermissionTarget::parse(tool, &serde_json::json!({})),
                Err(NativePermissionTargetError::InvalidField(field))
            );

            for (value, expected) in [
                (
                    serde_json::json!(1),
                    NativePermissionTargetError::InvalidField(field),
                ),
                (
                    serde_json::json!(""),
                    NativePermissionTargetError::InvalidField(field),
                ),
                (
                    serde_json::json!(too_long.clone()),
                    NativePermissionTargetError::FieldTooLong(field),
                ),
            ] {
                let arguments = serde_json::Value::Object(serde_json::Map::from_iter([(
                    field.to_owned(),
                    value,
                )]));

                assert_eq!(
                    NativePermissionTarget::parse(tool, &arguments),
                    Err(expected)
                );
            }
        }

        for (value, expected) in [
            (
                serde_json::json!(1),
                NativePermissionTargetError::InvalidField("path"),
            ),
            (
                serde_json::json!(""),
                NativePermissionTargetError::InvalidField("path"),
            ),
            (
                serde_json::json!(too_long),
                NativePermissionTargetError::FieldTooLong("path"),
            ),
        ] {
            assert_eq!(
                NativePermissionTarget::parse(
                    "native::grep",
                    &serde_json::json!({"pattern": "TODO", "path": value}),
                ),
                Err(expected)
            );
        }

        assert_eq!(
            NativePermissionTarget::parse("native::glob", &serde_json::json!([])),
            Err(NativePermissionTargetError::ArgumentsNotObject)
        );
        assert_eq!(
            NativePermissionTarget::parse(
                "native::unknown",
                &serde_json::json!({"path": "notes.md"}),
            ),
            Err(NativePermissionTargetError::UnknownTool)
        );
    }

    #[test]
    fn tool_input_parses_every_native_tool_into_its_typed_kind() {
        let cases = [
            (
                "read",
                serde_json::json!({"path": "notes.md"}),
                agens_core::ToolInput::Read {
                    path: "notes.md".into(),
                },
            ),
            (
                "write",
                serde_json::json!({"path": "notes.md", "content": "body"}),
                agens_core::ToolInput::Write {
                    path: "notes.md".into(),
                },
            ),
            (
                "edit",
                serde_json::json!({"path": "notes.md", "old": "old", "new": "new"}),
                agens_core::ToolInput::Edit {
                    path: "notes.md".into(),
                },
            ),
            (
                "list",
                serde_json::json!({"path": "src"}),
                agens_core::ToolInput::List { path: "src".into() },
            ),
            (
                "search",
                serde_json::json!({"path": "src", "query": "permission"}),
                agens_core::ToolInput::Search { path: "src".into() },
            ),
            (
                "glob",
                serde_json::json!({"pattern": "src/**/*.rs"}),
                agens_core::ToolInput::Glob {
                    pattern: "src/**/*.rs".into(),
                    path: None,
                },
            ),
            (
                "grep",
                serde_json::json!({"pattern": "TODO", "path": "crates/agens-cli"}),
                agens_core::ToolInput::Grep {
                    pattern: "TODO".into(),
                    path: Some("crates/agens-cli".into()),
                },
            ),
            (
                "bash",
                serde_json::json!({"command": "git status"}),
                agens_core::ToolInput::Bash {
                    command: "git status".into(),
                },
            ),
            (
                "webfetch",
                serde_json::json!({"url": "https://example.test/docs"}),
                agens_core::ToolInput::WebFetch {
                    url: "https://example.test/docs".into(),
                },
            ),
            (
                "skill",
                serde_json::json!({"skill": "shared"}),
                agens_core::ToolInput::Skill {
                    skill: "shared".into(),
                },
            ),
        ];

        for (name, arguments, expected) in cases {
            let raw = arguments.to_string();
            assert_eq!(agens_core::ToolInput::parse(name, &raw), expected);
        }
    }

    #[test]
    fn tool_input_degrades_unknown_and_mcp_tools_to_other_without_erroring() {
        let raw = serde_json::json!({"foo": "bar"}).to_string();
        assert_eq!(
            agens_core::ToolInput::parse("mcp_server_tool", &raw),
            agens_core::ToolInput::Other {
                name: "mcp_server_tool".into(),
                raw: raw.clone(),
            }
        );

        let malformed = "{not json";
        assert_eq!(
            agens_core::ToolInput::parse("read", malformed),
            agens_core::ToolInput::Other {
                name: "read".into(),
                raw: malformed.into(),
            }
        );

        let missing_field = serde_json::json!({}).to_string();
        assert_eq!(
            agens_core::ToolInput::parse("read", &missing_field),
            agens_core::ToolInput::Other {
                name: "read".into(),
                raw: missing_field.clone(),
            }
        );
    }

    #[test]
    fn production_allow_always_remembers_a_matching_call_within_one_batch() {
        let outcome = run_production_batch(
            "batch-allow-always",
            vec![PermissionPromptAnswer::AllowAlways],
            vec![
                batch_call("first", "notes.md"),
                batch_call("later", "notes.md"),
            ],
            None,
            None,
            false,
        );

        assert!(outcome.result.is_ok());
        assert_eq!(outcome.prompts, ["notes.md"]);
        assert_eq!(outcome.executions, ["notes.md", "notes.md"]);
    }

    #[test]
    fn production_deny_always_denies_later_matching_calls_without_execution() {
        let outcome = run_production_batch(
            "batch-deny-always",
            vec![PermissionPromptAnswer::DenyAlways],
            vec![
                batch_call("first", "notes.md"),
                batch_call("later", "notes.md"),
            ],
            None,
            None,
            false,
        );

        let snapshot = outcome
            .result
            .expect("denied calls should let the turn complete");
        assert_eq!(outcome.prompts, ["notes.md"]);
        assert!(outcome.executions.is_empty());
        assert_eq!(
            snapshot
                .events()
                .iter()
                .filter_map(|event| match event {
                    TurnEvent::ToolResult(MessagePart::ToolResult {
                        tool_call_id,
                        is_error,
                        ..
                    }) => {
                        Some((tool_call_id.as_str(), *is_error))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [("first", true), ("later", true)]
        );
    }

    #[test]
    fn grouped_native_permission_regressions_preserve_native_target_boundaries() {
        let ask_every_native_tool = || {
            PermissionPolicy::new(
                PermissionMode::Edit,
                vec![PermissionRule::global(
                    PermissionDecision::Ask,
                    PermissionPattern::glob("native::*").expect("native glob should be valid"),
                    PermissionPattern::Any,
                )],
            )
        };
        let valid_calls = || {
            vec![
                native_batch_call("list", "native::list", serde_json::json!({"path":"src"})),
                native_batch_call(
                    "glob",
                    "native::glob",
                    serde_json::json!({"pattern":"src/**/*.rs"}),
                ),
                native_batch_call(
                    "grep",
                    "native::grep",
                    serde_json::json!({"pattern":"Permission", "path":"src"}),
                ),
                native_batch_call(
                    "webfetch",
                    "native::webfetch",
                    serde_json::json!({"url":"https://example.test/docs"}),
                ),
            ]
        };

        let allowed = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "grouped-native-allow-always",
                vec![
                    PermissionPromptAnswer::AllowAlways,
                    PermissionPromptAnswer::AllowAlways,
                    PermissionPromptAnswer::AllowAlways,
                    PermissionPromptAnswer::AllowAlways,
                ],
                valid_calls(),
            )
            .with_policy(ask_every_native_tool()),
        );
        assert!(allowed.result.is_ok());
        assert_eq!(
            allowed.prompts,
            [
                "src",
                "src/**/*.rs",
                "Permission",
                "https://example.test/docs"
            ]
        );
        assert_eq!(
            allowed.executions,
            [
                "src",
                "src/**/*.rs",
                "Permission",
                "https://example.test/docs"
            ]
        );

        let partial = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "grouped-native-partial-grant",
                vec![
                    PermissionPromptAnswer::AllowAlways,
                    PermissionPromptAnswer::DenyOnce,
                ],
                vec![
                    native_batch_call(
                        "granted",
                        "native::glob",
                        serde_json::json!({"pattern":"src/**/*.rs"}),
                    ),
                    native_batch_call(
                        "sibling",
                        "native::glob",
                        serde_json::json!({"pattern":"tests/**/*.rs"}),
                    ),
                ],
            )
            .with_policy(ask_every_native_tool()),
        );
        assert!(partial.result.is_ok());
        assert_eq!(partial.prompts, ["src/**/*.rs", "tests/**/*.rs"]);
        assert_eq!(partial.executions, ["src/**/*.rs"]);

        let ask = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "grouped-native-ask",
                vec![PermissionPromptAnswer::Cancel],
                vec![native_batch_call(
                    "ask",
                    "native::grep",
                    serde_json::json!({"pattern":"TODO", "path":"src"}),
                )],
            )
            .with_policy(ask_every_native_tool()),
        );
        assert_eq!(ask.result, Err(HeadlessTurnError::Cancelled));
        assert_eq!(ask.prompts, ["TODO"]);
        assert!(ask.executions.is_empty());

        let deny_policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Deny,
                PermissionPattern::Exact("native::webfetch".into()),
                PermissionPattern::Any,
            )],
        );
        let denied = run_production_batch_with_policy(
            ProductionBatchInput::new(
                "grouped-native-deny-bypass",
                vec![PermissionPromptAnswer::AllowAlways],
                vec![native_batch_call(
                    "denied",
                    "native::webfetch",
                    serde_json::json!({"url":"https://example.test/blocked"}),
                )],
            )
            .with_policy(deny_policy)
            .with_bypass(),
        );
        assert!(denied.result.is_ok());
        assert!(denied.prompts.is_empty());
        assert!(denied.executions.is_empty());

        for (name, input) in [
            ("native::list", "{malformed"),
            ("native::glob", r#"{}"#),
            ("native::unknown", r#"{"path":"src"}"#),
            (
                "native::grep",
                r#"{"pattern":"TODO","_inject_permission_evaluator_failure":true}"#,
            ),
        ] {
            let invalid = run_production_batch_with_policy(
                ProductionBatchInput::new(
                    "grouped-native-invalid",
                    Vec::new(),
                    vec![MessagePart::ToolCall {
                        id: "invalid".into(),
                        name: name.into(),
                        input: input.into(),
                    }],
                )
                .with_policy(ask_every_native_tool())
                .with_bypass(),
            );
            assert_eq!(invalid.result, Err(HeadlessTurnError::PermissionEvaluation));
            assert!(invalid.prompts.is_empty());
            assert!(invalid.executions.is_empty());
        }
    }

    #[test]
    fn production_batch_prompts_each_distinct_ask_individually() {
        let outcome = run_production_batch(
            "batch-distinct-prompts",
            vec![
                PermissionPromptAnswer::AllowOnce,
                PermissionPromptAnswer::DenyOnce,
            ],
            vec![
                batch_call("first", "first.md"),
                batch_call("second", "second.md"),
            ],
            None,
            None,
            false,
        );

        assert!(outcome.result.is_ok());
        assert_eq!(outcome.prompts, ["first.md", "second.md"]);
        assert_eq!(outcome.executions, ["first.md"]);
    }

    #[test]
    fn production_batch_progress_has_boundaries_and_cancellation_never_completes() {
        let cancellation = HeadlessTurnCancellation::new();
        let outcome = run_production_batch(
            "batch-cancellation-progress",
            vec![
                PermissionPromptAnswer::AllowOnce,
                PermissionPromptAnswer::AllowOnce,
            ],
            vec![
                batch_call("first", "first.md"),
                batch_call("second", "second.md"),
            ],
            Some(cancellation),
            None,
            false,
        );

        assert_eq!(outcome.result, Err(HeadlessTurnError::Cancelled));
        assert_eq!(outcome.executions, ["first.md"]);
        assert_eq!(
            outcome.progress,
            vec![
                TurnEvent::StateChanged(TurnState::Requesting),
                TurnEvent::StateChanged(TurnState::Streaming),
                TurnEvent::ProviderPart(batch_call("first", "first.md")),
                TurnEvent::ProviderPart(batch_call("second", "second.md")),
                TurnEvent::StateChanged(TurnState::Dispatching),
                TurnEvent::ToolCallRequested {
                    id: "first".into(),
                    name: "native::read".into(),
                    input: r#"{"path":"first.md"}"#.into(),
                },
                TurnEvent::ToolCallRequested {
                    id: "second".into(),
                    name: "native::read".into(),
                    input: r#"{"path":"second.md"}"#.into(),
                },
                TurnEvent::ToolResult(MessagePart::ToolResult {
                    tool_call_id: "first".into(),
                    content: "tool execution failed".into(),
                    is_error: true,
                }),
                TurnEvent::StateChanged(TurnState::Cancelled),
            ]
        );
    }

    #[test]
    fn canonical_and_legacy_mcp_permission_aliases_resolve_after_reload() {
        struct RuntimeTool;

        impl DispatchTool for RuntimeTool {
            fn execute(
                &mut self,
                _: &ToolExecutionContext,
                _: serde_json::Value,
            ) -> Result<ToolOutput, agens_core::Error> {
                Ok(ToolOutput::success("executed"))
            }
        }

        fn dispatcher() -> ToolDispatcher {
            let mut dispatcher = ToolDispatcher::new();
            dispatcher
                .register_mcp(
                    &RemoteToolMetadata {
                        qualified_name: "files::read".into(),
                        server_name: "files".into(),
                        tool_name: "read".into(),
                        description: None,
                        input_schema: serde_json::json!({}),
                        access: agens_tools::RemoteToolAccess::ReadOnly,
                    },
                    RuntimeTool,
                )
                .expect("MCP tool should register");
            dispatcher
        }

        let directory =
            std::env::temp_dir().join(format!("agens-canonical-grants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let request = || {
            ToolDispatchRequest::new(
                "project",
                "files_read",
                serde_json::json!({"target": "notes.md"}),
            )
        };
        let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);
        let initial = dispatcher();
        let ToolEvaluationOutcome::PromptRequired(context) = initial
            .evaluate(&policy, &[], &PermissionSession::new(), request())
            .expect("canonical model name should resolve")
        else {
            panic!("ungranted MCP call should require a prompt");
        };
        assert_ne!(context.qualified_tool_name, "files::read");
        let canonical_name = context.qualified_tool_name.clone();

        let canonical = agens_core::ProjectPermissionGrant::allow(
            "project",
            PermissionPattern::Exact(canonical_name.clone()),
            PermissionPattern::Exact(context.target_identifier),
        );
        PermissionGrantStore::open(&directory)
            .expect("grant store should open")
            .append_grants(&[canonical])
            .expect("canonical grant should save");
        let grants = PermissionGrantStore::open(&directory)
            .expect("grant store should reopen")
            .grants_for_project("project")
            .expect("canonical grant should reload");
        assert_eq!(
            grants[0].tool,
            PermissionPattern::Exact(canonical_name),
            "prompt grants must persist the canonical identity"
        );
        let mut reloaded = dispatcher();
        let ToolEvaluationOutcome::Authorized(handle) = reloaded
            .evaluate(&policy, &grants, &PermissionSession::new(), request())
            .expect("canonical grant should resolve after reload")
        else {
            panic!("canonical grant should allow the model call");
        };
        assert_eq!(
            reloaded
                .execute(
                    handle,
                    &ToolExecutionContext::with_timeout(std::time::Duration::from_secs(1))
                )
                .expect("reloaded canonical grant should execute"),
            ToolOutput::success("executed")
        );

        for decision in [PermissionDecision::Allow, PermissionDecision::Deny] {
            let directory = directory.join(format!("legacy-{decision:?}"));
            PermissionGrantStore::open(&directory)
                .expect("grant store should open")
                .append_grants(&[agens_core::ProjectPermissionGrant::new(
                    "project",
                    decision,
                    PermissionPattern::Exact("files::read".into()),
                    PermissionPattern::Exact("notes.md".into()),
                )])
                .expect("legacy grant should save");
            let grants = PermissionGrantStore::open(&directory)
                .expect("grant store should reopen")
                .grants_for_project("project")
                .expect("legacy grant should reload");
            let outcome = dispatcher()
                .evaluate(&policy, &grants, &PermissionSession::new(), request())
                .expect("legacy grant should resolve through the model alias");
            match decision {
                PermissionDecision::Allow => {
                    assert!(matches!(outcome, ToolEvaluationOutcome::Authorized(_)));
                }
                PermissionDecision::Deny => {
                    assert!(matches!(outcome, ToolEvaluationOutcome::Denied));
                }
                PermissionDecision::Ask => unreachable!(),
            }
        }

        for (configured_decision, expected_decision) in [
            (ConfigPermissionDecision::Allow, PermissionDecision::Allow),
            (ConfigPermissionDecision::Deny, PermissionDecision::Deny),
        ] {
            let runtime = Arc::new(Mutex::new(dispatcher()));
            let policy = permission_policy(
                &[ConfigPermissionRule {
                    scope: ConfigPermissionScope::Global,
                    decision: configured_decision,
                    tool_pattern: "files::read".into(),
                    target_pattern: None,
                }],
                "project",
                PermissionMode::Edit,
                &runtime,
                None,
            )
            .expect("legacy configuration should resolve to the canonical model tool");
            let outcome = runtime
                .lock()
                .expect("dispatcher should remain available")
                .evaluate(&policy, &[], &PermissionSession::new(), request())
                .expect("canonical model call should evaluate");
            match expected_decision {
                PermissionDecision::Allow => {
                    assert!(matches!(outcome, ToolEvaluationOutcome::Authorized(_)));
                }
                PermissionDecision::Deny => {
                    assert!(matches!(outcome, ToolEvaluationOutcome::Denied));
                }
                PermissionDecision::Ask => unreachable!(),
            }
        }

        std::fs::remove_dir_all(&directory).expect("temporary grant directory should be removed");
    }

    fn permission_gate_with_no_grants() -> ProductionPermissionGate {
        ProductionPermissionGate::new(
            PermissionPolicy::new(PermissionMode::Edit, vec![]),
            Arc::new(Mutex::new(Vec::new())),
            PermissionSession::new(),
            "project".into(),
            Arc::new(Mutex::new(ToolDispatcher::new())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(BTreeMap::new())),
        )
    }

    #[test]
    fn a_denied_native_write_reports_the_path_it_targeted() {
        let gate = permission_gate_with_no_grants();
        let call = HeadlessToolCall {
            id: "denied-write".into(),
            name: "native::write".into(),
            input: r#"{"path":"secret.txt","content":"x"}"#.into(),
        };

        assert_eq!(
            gate.denial_facts(&call),
            Some(ToolResultFacts::Write {
                path: FactPath::new("secret.txt"),
                outcome: ToolOutcome::Denied,
                written: None,
            })
        );
    }

    #[test]
    fn a_denied_native_edit_reports_the_path_it_targeted() {
        let gate = permission_gate_with_no_grants();
        let call = HeadlessToolCall {
            id: "denied-edit".into(),
            name: "native::edit".into(),
            input: r#"{"path":"secret.txt","old":"a","new":"b"}"#.into(),
        };

        assert_eq!(
            gate.denial_facts(&call),
            Some(ToolResultFacts::Edit {
                path: FactPath::new("secret.txt"),
                outcome: ToolOutcome::Denied,
                changed: None,
            })
        );
    }

    #[test]
    fn a_denied_native_bash_carries_no_path() {
        let gate = permission_gate_with_no_grants();
        let call = HeadlessToolCall {
            id: "denied-bash".into(),
            name: "native::bash".into(),
            input: r#"{"command":"rm -rf /"}"#.into(),
        };

        assert_eq!(
            gate.denial_facts(&call),
            Some(ToolResultFacts::Bash {
                outcome: ToolOutcome::Denied,
                exit_code: None,
            })
        );
    }

    #[test]
    fn a_denied_call_with_an_unrecognized_tool_name_reports_no_facts() {
        let gate = permission_gate_with_no_grants();
        let call = HeadlessToolCall {
            id: "denied-unknown".into(),
            name: "mcp::files::read".into(),
            input: r#"{"path":"secret.txt"}"#.into(),
        };

        assert_eq!(gate.denial_facts(&call), None);
    }

    /// A malformed payload for a known native tool parses to `ToolInput::Other`,
    /// per `ParseToolInput`'s `serde_json` failure fallback. This is a decision,
    /// not a silent hole: the denial still reports that a write was attempted,
    /// with an unrepresentable path rather than a fabricated one, and the call
    /// remains visible via its `ToolResult` regardless.
    #[test]
    fn a_denied_native_write_with_a_malformed_payload_is_pathless_not_absent() {
        let gate = permission_gate_with_no_grants();
        let call = HeadlessToolCall {
            id: "denied-malformed-write".into(),
            name: "native::write".into(),
            input: "{not json".into(),
        };

        assert_eq!(
            ToolInput::parse("write", &call.input),
            ToolInput::Other {
                name: "write".into(),
                raw: "{not json".into(),
            }
        );
        match gate.denial_facts(&call) {
            Some(ToolResultFacts::Write {
                path,
                outcome,
                written,
            }) => {
                assert!(!path.is_representable());
                assert_eq!(outcome, ToolOutcome::Denied);
                assert_eq!(written, None);
            }
            other => panic!("expected pathless write denial facts, got {other:?}"),
        }
    }
}

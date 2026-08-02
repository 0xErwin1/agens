//! Deciding whether a tool call is allowed.
//!
//! Policy only: which rules apply, which grants exist, what a session already
//! authorized, and which tools a delegated subagent may reach. Asking a person
//! is a surface concern and lives with the surfaces.

pub use agens_tools::PermissionPromptContext;

pub const DANGEROUS_CHILD_NATIVE_TOOLS: [&str; 10] = [
    "native::read",
    "native::git_read",
    "native::list",
    "native::search",
    "native::glob",
    "native::grep",
    "native::write",
    "native::edit",
    "native::bash",
    "native::webfetch",
];

pub fn is_dangerous_child_native_tool(name: &str) -> bool {
    DANGEROUS_CHILD_NATIVE_TOOLS.iter().any(|registered| {
        name == *registered || name == registered.strip_prefix("native::").unwrap_or_default()
    })
}

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use agens_config::{ConfigPermissionDecision, ConfigPermissionRule, ConfigPermissionScope};
use agens_core::{
    ConfiguredFloor, FactPath, HeadlessPermissionGate, HeadlessPermissionResolver,
    HeadlessToolCall, HeadlessTurnCancellation, HeadlessTurnPortError, PermissionDecision,
    PermissionMode, PermissionPattern, PermissionPolicy, PermissionReach, PermissionRule,
    PermissionSession, SafetyPredicate, ToolInput, ToolOutcome, ToolResultFacts,
    permission_target_kind_for_tool,
};
use agens_store::PermissionGrantStore;
use agens_tools::{
    AuthorizedToolCall, EffectiveCapabilitySet, ToolDispatchRequest, ToolDispatcher,
    ToolEvaluationOutcome,
};

use agens_error::CliError;

#[derive(Debug, PartialEq, Eq)]
pub enum NativePermissionTarget {
    Command(String),
    Path(String),
    Pattern(String),
    Operation(String),
    Url(String),
    /// A content search, which two arguments describe at once: the pattern it
    /// is named by, and the path deciding which files it reads. Both are kept,
    /// because either one alone is enough to reach a file's contents and a rule
    /// may be written against either.
    Search {
        pattern: String,
        path: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum NativePermissionTargetError {
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
    pub fn parse(
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
            "native::git_read" => field("operation").map(Self::Operation),
            "native::grep" => {
                let path = arguments
                    .contains_key("path")
                    .then(|| field("path"))
                    .transpose()?;

                field("pattern").map(|pattern| Self::Search { pattern, path })
            }
            "native::webfetch" => field("url").map(Self::Url),
            _ => Err(NativePermissionTargetError::UnknownTool),
        }
    }

    pub fn into_value(self) -> String {
        match self {
            Self::Command(value)
            | Self::Path(value)
            | Self::Pattern(value)
            | Self::Operation(value)
            | Self::Url(value)
            | Self::Search { pattern: value, .. } => value,
        }
    }

    /// What the call reaches beyond the target it is named by.
    ///
    /// A search reads the files under the path it is given, and every file in
    /// the worktree when it is given none — the pattern it is named by says
    /// nothing about which those are. Every other native tool is named by the
    /// one thing it touches.
    pub fn reach(&self) -> Vec<PermissionReach> {
        match self {
            Self::Search { path, .. } => vec![
                path.clone()
                    .map_or(PermissionReach::EveryPath, PermissionReach::Path),
            ],
            Self::Command(_)
            | Self::Path(_)
            | Self::Pattern(_)
            | Self::Operation(_)
            | Self::Url(_) => Vec::new(),
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

pub trait ParseToolInput: Sized {
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

pub struct AllowedNativeCall {
    pub name: String,
    pub input: String,
    pub handle: AuthorizedToolCall,
}

pub type SharedToolDispatcher = Arc<Mutex<ToolDispatcher>>;
type SharedProjectPermissionGrants = Arc<Mutex<Vec<agens_core::ProjectPermissionGrant>>>;
type PendingPermissionPrompts = Arc<Mutex<BTreeMap<String, PermissionPromptContext>>>;

pub struct ProductionPermissionGate {
    pub policy: PermissionPolicy,
    pub grants: SharedProjectPermissionGrants,
    session: PermissionSession,
    project: String,
    dispatcher: SharedToolDispatcher,
    allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
    prompts: PendingPermissionPrompts,
    dangerous_override: bool,
}

impl ProductionPermissionGate {
    pub fn new(
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

    /// Sets the fallback this gate applies when a call matches no static
    /// rule and no grant. This never touches a matched decision: a declared
    /// `deny` or `ask` for a dangerous child tool still denies or asks even
    /// with the override set, because `evaluate_with_unmatched_override`
    /// only consults it after the matched-decision chain comes up empty.
    pub fn with_dangerous_override(mut self, dangerous_override: bool) -> Self {
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
                        if dispatcher.canonical_identity(&call.name).is_none() {
                            return Ok(ToolEvaluationOutcome::Denied);
                        }

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
                            .map_err(|_| HeadlessTurnPortError::Tool)
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
pub enum PermissionPromptAnswer {
    AllowOnce,
    AllowAlways,
    DenyOnce,
    DenyAlways,
    Cancel,
}

pub trait PermissionPrompter: Send {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError>;
}

/// Lets the engine hold the port without naming any implementation of it.
impl PermissionPrompter for Box<dyn PermissionPrompter> {
    fn prompt(
        &mut self,
        context: &PermissionPromptContext,
        cancellation: &HeadlessTurnCancellation,
    ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
        self.as_mut().prompt(context, cancellation)
    }
}

pub struct ProductionPermissionResolver<P> {
    prompt: P,
    grant_store: PermissionGrantStore,
    grants: SharedProjectPermissionGrants,
    prompts: PendingPermissionPrompts,
    pub authorization: ProductionPromptAuthorization,
}

pub struct ProductionPromptAuthorization {
    pub policy: PermissionPolicy,
    pub session: PermissionSession,
    pub project: String,
    pub dispatcher: SharedToolDispatcher,
    pub allowed: Arc<Mutex<BTreeMap<String, AllowedNativeCall>>>,
}

impl<P> ProductionPermissionResolver<P> {
    pub fn new(
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

pub fn permission_policy(
    rules: &[ConfigPermissionRule],
    project: &str,
    mode: PermissionMode,
    dispatcher: &SharedToolDispatcher,
    effective_capabilities: Option<&EffectiveCapabilitySet>,
) -> Result<PermissionPolicy, CliError> {
    let configured = configured_permission_rules(rules, project, |configured| {
        dispatcher
            .lock()
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?
            .canonical_identity(configured)
            .map(|identity| PermissionPattern::Exact(identity.as_str().to_owned()))
            .ok_or_else(|| CliError::configuration("permission configuration is invalid"))
    })?;
    let declared = effective_capabilities
        .map(EffectiveCapabilitySet::permission_rules)
        .unwrap_or_default();

    Ok(PermissionPolicy::with_safety_predicates(
        mode,
        declared,
        vec![SafetyPredicate::WorktreeEscape, SafetyPredicate::ChatWrite],
    )
    .with_configured_floor(ConfiguredFloor::governing(configured)))
}

/// Converts configured `[permissions]` entries into policy rules, resolving
/// each entry's tool name through `resolve_tool`.
///
/// The mapping from a configured name to the tool it names and the target's
/// `/`-crossing kind live here alone, so a delegated child derives the parent's
/// configured rules exactly as the parent itself does. Only the tool pattern
/// differs between callers: the primary path resolves it to a live dispatcher
/// identity, while a caller building a child surface has no dispatcher yet and
/// keeps the qualified name.
pub fn configured_permission_rules(
    rules: &[ConfigPermissionRule],
    project: &str,
    resolve_tool: impl Fn(&str) -> Result<PermissionPattern, CliError>,
) -> Result<Vec<PermissionRule>, CliError> {
    rules
        .iter()
        .map(|rule| {
            let decision = match rule.decision {
                ConfigPermissionDecision::Allow => PermissionDecision::Allow,
                ConfigPermissionDecision::Deny => PermissionDecision::Deny,
                ConfigPermissionDecision::Ask => PermissionDecision::Ask,
            };
            let configured = configured_tool_name(&rule.tool_pattern)?;
            let tool = resolve_tool(&configured)?;
            let target = match &rule.target_pattern {
                Some(pattern) => PermissionPattern::glob_for_target_kind(
                    pattern.clone(),
                    permission_target_kind_for_tool(&configured),
                )
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
        .collect()
}

/// Qualifies a bare configured tool name. Each name maps to the tool it
/// actually names: `edit` and `write` are separately registered tools, so a
/// rule naming one must never be retargeted at the other.
fn configured_tool_name(name: &str) -> Result<String, CliError> {
    match name {
        "read" => Ok("native::read".to_owned()),
        "write" => Ok("native::write".to_owned()),
        "edit" => Ok("native::edit".to_owned()),
        "list" => Ok("native::list".to_owned()),
        "search" => Ok("native::search".to_owned()),
        "bash" => Ok("native::bash".to_owned()),
        name => Ok(name.to_owned()),
    }
}

fn parse_tool_input(call: &HeadlessToolCall) -> Result<serde_json::Value, HeadlessTurnPortError> {
    serde_json::from_str(&call.input).map_err(|_| HeadlessTurnPortError::Tool)
}

pub fn sanitize_permission_target(tool: &str, target: &str) -> String {
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

/// Redacts a value recorded for display when it looks like a secret. Lives with
/// the marker it asks about rather than with whichever surface shows the metric.
pub fn sanitize_metric(value: &str) -> String {
    if contains_sensitive_marker(value) {
        "[redacted]".to_owned()
    } else {
        value.to_owned()
    }
}

pub fn contains_sensitive_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    ["api_key", "authorization", "password", "secret", "token"]
        .iter()
        .any(|marker| value.contains(marker))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use agens_config::{ConfigPermissionDecision, ConfigPermissionRule, ConfigPermissionScope};
    use agens_core::{
        Error, HeadlessPermissionGate, HeadlessToolCall, HeadlessTurnCancellation,
        HeadlessTurnPortError, PermissionDecision, PermissionMode, PermissionPolicy,
        PermissionSession, ToolAccess,
    };
    use agens_tools::{DispatchTool, ToolDispatchRequest, ToolDispatcher, ToolEvaluationOutcome};

    use super::{ProductionPermissionGate, SharedToolDispatcher, permission_policy};

    fn run_ready<T>(
        future: impl std::future::Future<Output = Result<T, HeadlessTurnPortError>>,
    ) -> Result<T, HeadlessTurnPortError> {
        let mut future = std::pin::pin!(future);
        let context = &mut std::task::Context::from_waker(std::task::Waker::noop());

        match future.as_mut().poll(context) {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => {
                panic!("production permission ports must complete synchronously")
            }
        }
    }

    #[test]
    fn evaluate_denies_a_call_to_a_tool_absent_from_the_dispatcher() {
        let dispatcher: SharedToolDispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        let grants = Arc::new(Mutex::new(Vec::new()));
        let allowed = Arc::new(Mutex::new(BTreeMap::new()));
        let prompts = Arc::new(Mutex::new(BTreeMap::new()));

        let mut gate = ProductionPermissionGate::new(
            PermissionPolicy::new(PermissionMode::Edit, Vec::new()),
            grants,
            PermissionSession::new(),
            "project".into(),
            dispatcher,
            allowed,
            prompts,
        );

        let call = HeadlessToolCall {
            id: "current".into(),
            name: "native::write".into(),
            input: r#"{"path":"notes.md"}"#.into(),
        };
        let cancellation = HeadlessTurnCancellation::new();

        assert_eq!(
            run_ready(gate.evaluate(&call, &cancellation)),
            Ok(PermissionDecision::Deny)
        );
    }

    #[test]
    fn a_configured_ask_rule_produces_a_matched_ask_decision_that_survives_bypass() {
        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register_native("native::bash", ToolAccess::Write, StubBashTool)
            .expect("native bash should register");
        let dispatcher: SharedToolDispatcher = Arc::new(Mutex::new(dispatcher));

        let policy = permission_policy(
            &[ConfigPermissionRule {
                scope: ConfigPermissionScope::Global,
                decision: ConfigPermissionDecision::Ask,
                tool_pattern: "bash".into(),
                target_pattern: None,
            }],
            "project",
            PermissionMode::Edit,
            &dispatcher,
            None,
        )
        .expect("a configured ask rule should resolve");

        let request = || {
            ToolDispatchRequest::new(
                "project",
                "native::bash",
                serde_json::json!({"target": "echo hi"}),
            )
        };

        let plain_outcome = dispatcher
            .lock()
            .expect("dispatcher should remain available")
            .evaluate(&policy, &[], &PermissionSession::new(), request())
            .expect("configured ask should evaluate");
        assert!(matches!(
            plain_outcome,
            ToolEvaluationOutcome::PromptRequired(_)
        ));

        let bypassed_outcome = dispatcher
            .lock()
            .expect("dispatcher should remain available")
            .evaluate(
                &policy,
                &[],
                &PermissionSession::with_temporary_bypass(),
                request(),
            )
            .expect("configured ask should evaluate under bypass");
        assert!(
            matches!(bypassed_outcome, ToolEvaluationOutcome::PromptRequired(_)),
            "a configured ask must survive temporary_bypass exactly like an agent-declared ask"
        );
    }

    #[test]
    fn a_configured_bash_deny_matches_a_command_carrying_a_path_argument() {
        let cases = [
            ("git reset --hard*", "git reset --hard origin/main"),
            ("git push*", "git push origin feature/x"),
            ("git rebase*", "git rebase origin/main"),
            ("rm*", "rm -rf /tmp/x"),
        ];

        for (target_pattern, command) in cases {
            let mut dispatcher = ToolDispatcher::new();
            dispatcher
                .register_native("native::bash", ToolAccess::Write, StubBashTool)
                .expect("native bash should register");
            let dispatcher: SharedToolDispatcher = Arc::new(Mutex::new(dispatcher));

            let policy = permission_policy(
                &[ConfigPermissionRule {
                    scope: ConfigPermissionScope::Global,
                    decision: ConfigPermissionDecision::Deny,
                    tool_pattern: "bash".into(),
                    target_pattern: Some(target_pattern.into()),
                }],
                "project",
                PermissionMode::Edit,
                &dispatcher,
                None,
            )
            .expect("a configured deny rule should resolve");

            let outcome = dispatcher
                .lock()
                .expect("dispatcher should remain available")
                .evaluate(
                    &policy,
                    &[],
                    &PermissionSession::new(),
                    ToolDispatchRequest::new(
                        "project",
                        "native::bash",
                        serde_json::json!({"target": command}),
                    ),
                )
                .expect("configured deny should evaluate");

            assert!(
                matches!(outcome, ToolEvaluationOutcome::Denied),
                "deny bash({target_pattern}) should have denied {command:?}, got {outcome:?}"
            );
        }
    }

    /// `edit` is a registered tool of its own, not a spelling of `write`, so a
    /// configured rule naming it has to reach it. Resolving it to
    /// `native::write` left `native::edit` unaffected by its own deny and
    /// silently retargeted the rule at a different tool.
    #[test]
    fn a_configured_rule_naming_edit_reaches_the_edit_tool() {
        let mut dispatcher = ToolDispatcher::new();
        for tool in ["native::write", "native::edit"] {
            dispatcher
                .register_native(tool, ToolAccess::Write, StubBashTool)
                .expect("native tool should register");
        }
        let dispatcher: SharedToolDispatcher = Arc::new(Mutex::new(dispatcher));

        let policy = permission_policy(
            &[ConfigPermissionRule {
                scope: ConfigPermissionScope::Global,
                decision: ConfigPermissionDecision::Deny,
                tool_pattern: "edit".into(),
                target_pattern: None,
            }],
            "project",
            PermissionMode::Edit,
            &dispatcher,
            None,
        )
        .expect("a configured deny rule should resolve");

        let decision = |tool: &str| {
            dispatcher
                .lock()
                .expect("dispatcher should remain available")
                .evaluate(
                    &policy,
                    &[],
                    &PermissionSession::new(),
                    ToolDispatchRequest::new(
                        "project",
                        tool,
                        serde_json::json!({"target": "notes.md"}),
                    ),
                )
                .expect("configured deny should evaluate")
        };

        assert!(
            matches!(decision("native::edit"), ToolEvaluationOutcome::Denied),
            "a configured deny naming edit must deny the edit tool"
        );
        assert!(
            !matches!(decision("native::write"), ToolEvaluationOutcome::Denied),
            "a configured deny naming edit must not deny a different tool"
        );
    }

    struct StubBashTool;

    impl DispatchTool for StubBashTool {
        fn permission_target(&self, arguments: &serde_json::Value) -> Result<String, Error> {
            arguments
                .get("target")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| Error::Tool("tool target is required".into()))
        }

        fn execute(
            &mut self,
            _context: &agens_tools::ToolExecutionContext,
            _arguments: serde_json::Value,
        ) -> Result<agens_tools::ToolOutput, Error> {
            unreachable!("test tool is never executed")
        }
    }
}

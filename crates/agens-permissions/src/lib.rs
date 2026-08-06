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

/// Whether `name` is one of the natives a dangerous-mode override may widen.
///
/// The name is reduced through [`agens_core::bare_tool_name`] rather than
/// compared against a list of spellings, so this cannot answer differently for
/// the same tool depending on which of its names reached it — the bare one the
/// model is advertised, the qualified one a rule is written in, or the
/// dispatcher's own encoding of either.
pub fn is_dangerous_child_native_tool(name: &str) -> bool {
    let bare = agens_core::bare_tool_name(name);

    DANGEROUS_CHILD_NATIVE_TOOLS
        .iter()
        .filter_map(|registered| registered.strip_prefix("native::"))
        .any(|registered| bare == registered)
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
    /// A search reads the files under the path it is given, which the pattern
    /// it is named by says nothing about, so that path is reported here and a
    /// rule naming it selects the call. A search given no path names no file
    /// at all, and which of the files it walks into it may report is settled
    /// per file while it runs. Every other native tool is named by the one
    /// thing it touches.
    pub fn reach(&self) -> Vec<PermissionReach> {
        match self {
            Self::Search { path, .. } => path
                .clone()
                .map(PermissionReach::Path)
                .into_iter()
                .collect(),
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
                            return if names_a_tool_agens_ships(&call.name) {
                                Ok(ToolEvaluationOutcome::Denied)
                            } else {
                                Err(HeadlessTurnPortError::UnknownTool)
                            };
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
                            .map_err(evaluation_failure)
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
    /// A call the model made carries the bare name it was advertised under,
    /// which is already the vocabulary `ToolInput::parse` expects. A call agens
    /// makes on its own behalf carries the qualified one instead — the TUI's
    /// own task launch spells it `native::task` — and both are aliases of the
    /// same tool everywhere else in the dispatcher. The name is reduced through
    /// [`agens_core::bare_tool_name`] so this cannot answer differently for the
    /// same call depending on which spelling reached it, the dispatcher's own
    /// encoding included.
    ///
    /// A remote call reduces to `<server>::<tool>`, which is no native name, so
    /// it falls through to no facts. That is the honest answer — a remote
    /// tool's arguments belong to the server that serves it.
    fn denial_facts(&self, call: &HeadlessToolCall) -> Option<ToolResultFacts> {
        let called = agens_core::bare_tool_name(&call.name);

        match called.as_ref() {
            name @ "write" => Some(ToolResultFacts::Write {
                path: denied_input_path(ToolInput::parse(name, &call.input)),
                outcome: ToolOutcome::Denied,
                written: None,
            }),
            name @ "edit" => Some(ToolResultFacts::Edit {
                path: denied_input_path(ToolInput::parse(name, &call.input)),
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

/// Classifies why the dispatcher could not evaluate a call.
///
/// A tool reports a malformed payload as [`agens_core::Error::Tool`], which
/// keeps travelling the argument-error channel the model already reads as
/// "you called it wrong". A tool that cannot state what a well-formed call
/// reaches reports [`agens_core::Error::Permission`] instead: nothing the
/// model writes fixes that, so it must not arrive wearing the wording that
/// asks it to try.
fn evaluation_failure(error: agens_core::Error) -> HeadlessTurnPortError {
    match error {
        agens_core::Error::Permission(_) => HeadlessTurnPortError::PermissionUnresolvable,
        _ => HeadlessTurnPortError::Tool,
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
        // Kept for call-site clarity: AllowOnce/Always still build a grant for
        // persistence paths, but this re-auth no longer re-matches policy.
        _ephemeral_grant: Option<agens_core::ProjectPermissionGrant>,
    ) -> Result<PermissionDecision, HeadlessTurnPortError> {
        let arguments = parse_tool_input(call)?;
        let request = ToolDispatchRequest::new(
            &self.authorization.project,
            &call.name,
            arguments,
        );

        // Human Allow is the decision. Re-running policy here used to leave a
        // residual PromptRequired (configured floor ask, compound bash subjects,
        // empty-project grant stripping) and refuse the call after the person
        // already said yes. Hard safety still applies inside the force path.
        let outcome = self
            .authorization
            .dispatcher
            .lock()
            .map_err(|_| HeadlessTurnPortError::Permission)?
            .authorize_after_human_approval(&self.authorization.policy, request)
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

            let context = self
                .prompts
                .lock()
                .map_err(|_| HeadlessTurnPortError::Permission)?
                .remove(&call.id)
                .ok_or(HeadlessTurnPortError::Permission)?;
            // While a person is answering, only cancel ends the wait — never
            // the turn deadline. Pass a cancel-only view so a surface that
            // still reads deadlines cannot time out a parked question.
            let human_wait = HeadlessTurnCancellation::with_cancellation_and_deadline(
                cancellation.adapter_view().cancellation_handle(),
                None,
            );
            let answer = self.prompt.prompt(&context, &human_wait)?;

            if cancellation.is_cancelled() || answer == PermissionPromptAnswer::Cancel {
                return Err(HeadlessTurnPortError::Cancelled);
            }

            let decision = match answer {
                PermissionPromptAnswer::AllowOnce => {
                    // Any-target for this single re-auth: the person approved this
                    // call id, not a brittle Exact(full compound command) match.
                    let grant = agens_core::ProjectPermissionGrant::allow(
                        context.project_id.clone(),
                        PermissionPattern::Exact(context.tool_identity.clone()),
                        PermissionPattern::Any,
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
                    // Persist Exact(target) for future calls when it matches; re-auth
                    // for this call still goes through authorize_prompted_allow which
                    // falls back to Any if needed.
                    let grant = agens_core::ProjectPermissionGrant::new(
                        context.project_id.clone(),
                        decision,
                        PermissionPattern::Exact(context.tool_identity.clone()),
                        PermissionPattern::Exact(context.target_identifier.clone()),
                    );
                    self.grant_store
                        .append_grants(std::slice::from_ref(&grant))
                        .map_err(|_| HeadlessTurnPortError::Permission)?;
                    self.grants
                        .lock()
                        .map_err(|_| HeadlessTurnPortError::Permission)?
                        .push(grant);
                    if decision == PermissionDecision::Allow {
                        // Also seed an Any-target ephemeral grant so this call
                        // authorizes even if Exact(target) subject matching fails.
                        let force = agens_core::ProjectPermissionGrant::allow(
                            context.project_id,
                            PermissionPattern::Exact(context.tool_identity),
                            PermissionPattern::Any,
                        );
                        self.authorize_prompted_allow(call, Some(force))?
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
    let enforceable = enforceable_configured_rules(rules, dispatcher)?;
    let configured = configured_permission_rules(&enforceable, project, |configured| {
        let dispatcher = dispatcher
            .lock()
            .map_err(|_| CliError::configuration("tool catalog is invalid"))?;

        if let Some(identity) = dispatcher.canonical_identity(configured) {
            return Ok(PermissionPattern::Exact(identity.as_str().to_owned()));
        }
        if names_a_tool_this_session_does_not_hold(&dispatcher, configured) {
            return Ok(PermissionPattern::Exact(configured.to_owned()));
        }

        Err(unresolvable_configured_rule(&dispatcher, configured))
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

/// Drops a configured `allow` whose tool name nothing this session can account
/// for, and keeps every other entry for the resolver to bind or to refuse.
///
/// A grant for a tool no call can name grants nothing, so refusing to run over
/// one turns a stale line into a session that cannot start — and `[permissions]`
/// may be written in a project document that is committed to a repository,
/// while `[mcp]` may not, so that line can reach a collaborator whose own
/// configuration never declared the server it names. This is the same trade an
/// agent definition's unmatched `allow` already makes for the same reason
/// (`agens_tools`' `resolved_selectors`).
///
/// `deny` and `ask` keep refusing. Dropping one of those would leave an
/// operator believing a restriction is in force when the name they wrote
/// reaches nothing, which is fail-open in exactly the direction a dropped
/// `allow` is fail-closed.
fn enforceable_configured_rules(
    rules: &[ConfigPermissionRule],
    dispatcher: &SharedToolDispatcher,
) -> Result<Vec<ConfigPermissionRule>, CliError> {
    let dispatcher = dispatcher
        .lock()
        .map_err(|_| CliError::configuration("tool catalog is invalid"))?;

    Ok(rules
        .iter()
        .filter(|rule| {
            let configured = configured_tool_name(&rule.tool_pattern);

            rule.decision != ConfigPermissionDecision::Allow
                || dispatcher.canonical_identity(&configured).is_some()
                || names_a_tool_this_session_does_not_hold(&dispatcher, &configured)
        })
        .cloned()
        .collect())
}

/// Says which rule was refused and what could not be resolved about it.
///
/// The operator who removed an MCP server and the operator who mistyped a tool
/// name are looking at different mistakes, and neither of them necessarily
/// wrote the rule: a project document carries `[permissions]` into every
/// checkout of a repository.
fn unresolvable_configured_rule(dispatcher: &ToolDispatcher, configured: &str) -> CliError {
    let reason = match qualified_mcp_server(configured) {
        Some(server) if dispatcher.declares_mcp_server(server) => {
            format!("MCP server \"{server}\" is running and serves no tool by that name")
        }
        Some(server) => {
            format!("no [mcp.{server}] block declares an MCP server named \"{server}\"")
        }
        None => "it names no native tool, and no MCP server this configuration declares \
                 advertises a tool under that name"
            .to_owned(),
    };

    CliError::configuration(format!(
        "permission rule \"{configured}\" cannot be resolved: {reason}"
    ))
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
            let configured = configured_tool_name(&rule.tool_pattern);
            let tool = resolve_tool(&configured)?;
            let target = match &rule.target_pattern {
                Some(pattern) => PermissionPattern::glob_for_target_kind(
                    pattern.clone(),
                    permission_target_kind_for_tool(&configured),
                )
                .map_err(|_| {
                    CliError::configuration(format!(
                        "permission rule \"{configured}\" cannot be resolved: its target is not a \
                         valid pattern"
                    ))
                })?,
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

/// Qualifies a bare configured tool name against the native catalog, so a rule
/// naming a tool reaches that tool and no other: `edit` and `write` are
/// separately registered, and a rule naming one must never be retargeted at the
/// other.
///
/// The surface is asked rather than a list kept here, because a list kept here
/// qualifies whatever was remembered when it was written. The catalog alone is
/// not that surface: `skill`, `task` and the two coordination tools are
/// registered beside it, and a rule naming one of those has to reach it too.
/// Anything the surface does not hold — an MCP tool, an already-qualified name,
/// a typo — is left as written, for the caller to resolve or to diagnose.
fn configured_tool_name(name: &str) -> String {
    let qualified = format!("native::{name}");

    if names_the_native_surface(&qualified) {
        qualified
    } else {
        name.to_owned()
    }
}

/// Whether a qualified name is one of the natives agens ships, across every
/// dispatcher any session builds.
///
/// The catalog alone is not that surface: `skill`, `task` and the two
/// coordination tools are registered beside it.
fn names_the_native_surface(qualified: &str) -> bool {
    agens_tools::NativeToolCatalog::metadata()
        .iter()
        .any(|entry| entry.qualified_name == qualified)
        || agens_tools::NATIVE_TOOLS_REGISTERED_OUTSIDE_THE_CATALOG.contains(&qualified)
}

/// Whether a called name spells a tool agens ships, in either the bare
/// spelling a tool is advertised to the model under or the qualified one agens
/// uses on its own behalf.
///
/// Asked only about a call the live dispatcher does not hold, to separate the
/// two ways that happens. A tool agens ships that this session's surface leaves
/// out was refused — the model asked for something real and does not have it.
/// A name that spells no tool agens ships was refused by nothing, because there
/// was no tool there to refuse it, and telling the model it was denied would
/// send it looking for a permission it could never be granted.
fn names_a_tool_agens_ships(called: &str) -> bool {
    names_the_native_surface(called) || names_the_native_surface(&format!("native::{called}"))
}

/// Whether a configured name spells a tool this session's own configuration
/// accounts for and this session's dispatcher does not hold — the one case a
/// live dispatcher cannot answer for on its own.
///
/// Such a rule is kept as written rather than rejected. Rejecting it would
/// report a surface that is legitimately not here as an operator error, and the
/// dispatcher's own alias lookup resolves the retained name at evaluation time,
/// so it binds for real the moment that surface appears. This is the same trade
/// an agent definition's unmatched `deny` already makes for the same reason
/// (`agens_tools`' `resolved_selectors`), and the two paths agree on the other
/// side of it too: an unmatched `allow` is dropped rather than kept, here by
/// [`enforceable_configured_rules`].
///
/// Two surfaces can legitimately be absent, and nothing else is softened.
///
/// **A native this session does not register.** `task` and the two tools that
/// coordinate a live delegation reach the dispatcher only when some agent runs
/// in subagent mode, so a session configured without one holds none of them; a
/// rule naming one is spelt correctly and names a real tool. A name on no
/// native surface at all is a typo with no other reading and keeps failing,
/// including in its `native::`-qualified spelling.
///
/// **A tool of a declared MCP server this session reached nothing from.** A
/// server that failed to start, or one configured `disabled`, contributes no
/// tools, and a rule naming its tools is what the documentation asks operators
/// to write. Both names one remote tool answers to are softened together:
/// `<server>::<tool>` and the `<server>_<tool>` it is advertised to the model
/// under, which `register_mcp` installs as an alias and the repository's own
/// configuration fixture writes as a rule. Only the first says on its own that
/// it is remote — the second is shaped exactly like a bare native name — so
/// both are keyed on the same thing instead: a server this session's
/// configuration declares. A tool misspelt against a server this session DOES
/// hold has a live surface to be checked against and fails there.
fn names_a_tool_this_session_does_not_hold(dispatcher: &ToolDispatcher, configured: &str) -> bool {
    let absent_native =
        names_the_native_surface(configured) && dispatcher.canonical_identity(configured).is_none();

    absent_native || names_a_tool_of_an_absent_mcp_server(dispatcher, configured)
}

/// A `<server>::<tool>` name says which server it means, so that one server
/// answers for it. A `<server>_<tool>` name says nothing: it carries no
/// separator, and more than one declared server can explain it — a server `a`
/// serving `b_c` and a server `a_b` serving `c` are both advertised as `a_b_c`.
/// The declared set is therefore asked whether ANY of them explains the name
/// and is absent. Deciding on whichever one is examined first refuses a
/// correctly spelt rule whenever that one happens to be the server that is
/// running.
fn names_a_tool_of_an_absent_mcp_server(dispatcher: &ToolDispatcher, configured: &str) -> bool {
    if let Some(server) = qualified_mcp_server(configured) {
        return dispatcher.declares_mcp_server(server) && !dispatcher.holds_mcp_server(server);
    }

    dispatcher
        .declared_mcp_servers()
        .filter(|server| advertises_a_tool_as(server, configured))
        .any(|server| !dispatcher.holds_mcp_server(server))
}

/// The server a `<server>::<tool>` name names.
///
/// `native` is excluded from the server side, exactly rather than case-folded:
/// `native::` is the literal prefix [`ToolDispatcher::register_native`]
/// qualifies its own tools under, so `native::webfetc` is a misspelt native
/// rather than a remote tool of a server called `native` — and stays one even
/// in a session that declares such a server. `Native` is a legal MCP server
/// name and is treated as one: `Native::writ` is refused for the ordinary
/// reason that no server by that name was declared, not for being native.
fn qualified_mcp_server(configured: &str) -> Option<&str> {
    configured
        .split_once("::")
        .filter(|(server, tool)| !server.is_empty() && *server != "native" && !tool.is_empty())
        .map(|(server, _)| server)
}

/// Whether `server` would advertise a tool of its own under `configured`.
fn advertises_a_tool_as(server: &str, configured: &str) -> bool {
    configured
        .strip_prefix(server)
        .and_then(|tool| tool.strip_prefix('_'))
        .is_some_and(|tool| !tool.is_empty())
}

fn parse_tool_input(call: &HeadlessToolCall) -> Result<serde_json::Value, HeadlessTurnPortError> {
    serde_json::from_str(&call.input).map_err(|_| HeadlessTurnPortError::Tool)
}

/// Reduces a target to what may be shown for it.
///
/// `tool` is whatever spelling the caller holds, including a dispatcher
/// identity, so it is reduced through [`agens_core::bare_tool_name`] before
/// anything is decided on it. A `bash` target is the command line itself: shaped
/// secrets are redacted in place so the person can still read what will run.
/// WebFetch URLs keep their host/path while credentials and query are stripped.
pub fn sanitize_permission_target(tool: &str, target: &str) -> String {
    if agens_core::bare_tool_name(tool) == "bash" {
        return agens_core::redaction::redact_credential_values(target);
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
    use agens_core::ask_user::UnavailableAskUserPort;
    use agens_core::{
        Error, HeadlessPermissionGate, HeadlessToolCall, HeadlessTurnCancellation,
        HeadlessTurnPortError, PermissionDecision, PermissionMode, PermissionPattern,
        PermissionPolicy, PermissionRule, PermissionSession, ToolAccess,
    };
    use agens_tools::{
        AskUserTool, DispatchTool, ToolDispatchRequest, ToolDispatcher, ToolEvaluationOutcome,
    };

    use super::*;

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

    /// Every reader of a tool name in this crate answers the same for every
    /// spelling of the same tool.
    ///
    /// Two of them decide something a person then relies on: whether a
    /// dangerous-mode override may widen this call, and what a denied call is
    /// reported to have touched. A dispatcher identity is a name either could
    /// be handed — it is what a `PermissionRequest` and a prompt context carry
    /// — and comparing it against a written spelling answers `false` for a tool
    /// that is plainly on the list.
    #[test]
    fn a_tool_name_decides_the_same_way_in_every_spelling_it_answers_to() {
        for spelling in ["bash", "native::bash", "native:4:bash"] {
            assert!(
                super::is_dangerous_child_native_tool(spelling),
                "`{spelling}` names bash and must be recognized as one of the tools a \
                 dangerous-mode override widens"
            );
        }
        assert!(
            !super::is_dangerous_child_native_tool("mcp:5:probe:14:read_text_file"),
            "a remote tool is on no native list, whichever name it arrives under"
        );

        let dispatcher: SharedToolDispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        let gate = ProductionPermissionGate::new(
            PermissionPolicy::new(PermissionMode::Edit, Vec::new()),
            Arc::new(Mutex::new(Vec::new())),
            PermissionSession::new(),
            "project".into(),
            dispatcher,
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(Mutex::new(BTreeMap::new())),
        );

        for spelling in ["write", "native::write", "native:5:write"] {
            let facts = gate.denial_facts(&HeadlessToolCall {
                id: "current".into(),
                name: spelling.into(),
                input: r#"{"path":"notes.md"}"#.into(),
            });

            assert!(
                matches!(
                    facts,
                    Some(agens_core::ToolResultFacts::Write { ref path, .. })
                        if path.relative() == Some("notes.md")
                ),
                "a denied write has to report the path it targeted whichever name it arrived \
                 under, and `{spelling}` reported {facts:?}"
            );
        }
    }

    /// The two surfaces a rule may legitimately name and this session not hold
    /// are asked the same question, so neither branch answers for a tool that
    /// is right here.
    ///
    /// The caller reaches this only after the dispatcher failed to resolve the
    /// name, so a held tool never gets here today. A branch that would answer
    /// `true` for one is still wrong: it makes the function's answer depend on
    /// a check outside it, and the next caller inherits that.
    #[test]
    fn a_tool_this_session_holds_is_not_reported_as_one_it_lacks() {
        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register_native("native::write", ToolAccess::Write, StubBashTool)
            .expect("native write should register");

        assert!(
            !super::names_a_tool_this_session_does_not_hold(&dispatcher, "native::write"),
            "a native this dispatcher registered is held, not absent"
        );
        assert!(
            super::names_a_tool_this_session_does_not_hold(&dispatcher, "native::task"),
            "a native on the shared surface that this dispatcher never registered is absent"
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

    #[test]
    fn native_ask_user_is_not_a_dangerous_child_native_tool() {
        assert!(!is_dangerous_child_native_tool("native::ask_user"));
        assert!(!is_dangerous_child_native_tool("ask_user"));
    }

    /// Registers the REAL `agens_tools::AskUserTool` — not a self-describing stub — so
    /// `permission_target` and `execute` are the actual production behavior, not a hand-rolled
    /// double that trivially agrees with itself.
    ///
    /// The `ToolAccess::ReadOnly` argument below is still supplied by this test, not read back
    /// from production wiring: `agens-permissions` is a DEPENDENCY of `agens-tool-runtime` (see
    /// `agens-tool-runtime/Cargo.toml`), so it cannot see or call the production registration
    /// site (`register_native("native::ask_user", ToolAccess::ReadOnly, ...)` in
    /// `crates/agens-tool-runtime/src/runtime.rs`) — that direction of proof belongs in
    /// `agens-tool-runtime`'s own tests, which DO call the real production builder:
    /// `agens-tool-runtime/src/runtime.rs::tests::ask_user_is_registered_read_only_and_survives_chat_mode_hard_safety`.
    /// What THIS test proves is narrower and still real: the permission-evaluation machinery
    /// authorizes the actual `AskUserTool` type exactly like it authorizes any other read-only
    /// native tool, mirroring the codebase's established convention for "authorized under the
    /// default policy with no rule configured" (see `agens-tools/tests/runtime_contracts.rs`) —
    /// an empty rule set under `PermissionMode::Edit` resolves every tool to `Ask` UNLESS the
    /// session carries a temporary bypass, at which point `Ask` resolves to `Allow`.
    #[test]
    fn native_ask_user_is_authorized_with_no_prompt_when_the_session_bypasses_prompts() {
        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register_native(
                "native::ask_user",
                agens_core::ToolAccess::ReadOnly,
                AskUserTool::new(Box::new(UnavailableAskUserPort)),
            )
            .unwrap();
        let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]);

        let outcome = dispatcher
            .evaluate(
                &policy,
                &[],
                &agens_core::PermissionSession::with_temporary_bypass(),
                ToolDispatchRequest::new("project", "native::ask_user", serde_json::json!({})),
            )
            .unwrap();

        assert!(
            matches!(outcome, ToolEvaluationOutcome::Authorized(_)),
            "native::ask_user should authorize like any other read-only native tool, saw {outcome:?}"
        );
    }

    /// Same real-type rationale as above: registers the actual `agens_tools::AskUserTool`.
    /// `hard_safety_allows` denies a `ToolAccess::Write` tool outright in
    /// `PermissionMode::Chat`. `native::ask_user` is registered `ReadOnly` here (see the
    /// dependency-direction note above — the production registration itself is proven in
    /// `agens-tool-runtime`), so it must survive that hard-safety check even without any
    /// matching rule.
    #[test]
    fn native_ask_user_is_not_hard_denied_in_chat_mode() {
        let mut dispatcher = ToolDispatcher::new();
        dispatcher
            .register_native(
                "native::ask_user",
                agens_core::ToolAccess::ReadOnly,
                AskUserTool::new(Box::new(UnavailableAskUserPort)),
            )
            .unwrap();
        let policy = PermissionPolicy::new(
            PermissionMode::Chat,
            vec![PermissionRule::global(
                PermissionDecision::Allow,
                PermissionPattern::Exact("native::ask_user".into()),
                PermissionPattern::Any,
            )],
        );

        let outcome = dispatcher
            .evaluate(
                &policy,
                &[],
                &agens_core::PermissionSession::new(),
                ToolDispatchRequest::new("project", "native::ask_user", serde_json::json!({})),
            )
            .unwrap();

        assert!(matches!(outcome, ToolEvaluationOutcome::Authorized(_)));
    }

    /// After the person says Allow, soft policy `ask` (including a configured
    /// floor) must not re-refuse the call. That is how peers work: human Allow
    /// is the authorization. Regression for "approval could not be completed"
    /// after Allow on simple `rm` / compound bash while a floor still says ask.
    #[test]
    fn allow_once_authorizes_under_configured_floor_ask_including_bash() {
        struct AllowOncePrompter;

        impl PermissionPrompter for AllowOncePrompter {
            fn prompt(
                &mut self,
                _: &PermissionPromptContext,
                _: &HeadlessTurnCancellation,
            ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
                Ok(PermissionPromptAnswer::AllowOnce)
            }
        }

        struct BashTool;

        impl DispatchTool for BashTool {
            fn permission_target(
                &self,
                arguments: &serde_json::Value,
            ) -> Result<String, Error> {
                Ok(arguments["command"].as_str().unwrap_or_default().to_owned())
            }

            fn execute(
                &mut self,
                _: &agens_tools::ToolExecutionContext,
                _: serde_json::Value,
            ) -> Result<agens_tools::ToolOutput, Error> {
                Ok(agens_tools::ToolOutput::success("ok"))
            }
        }

        let directory = std::env::temp_dir().join(format!(
            "agens-permission-floor-allow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&directory);

        let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        dispatcher
            .lock()
            .expect("dispatcher lock")
            .register_native("native::bash", ToolAccess::Write, BashTool)
            .expect("bash should register");

        let grants = Arc::new(Mutex::new(Vec::new()));
        let allowed = Arc::new(Mutex::new(BTreeMap::new()));
        let prompts = Arc::new(Mutex::new(BTreeMap::new()));
        // Configured floor `ask bash *` is more restrictive than any Allow grant.
        // Re-evaluating policy after the prompt used to stay PromptRequired and
        // refuse the call the person just allowed.
        let policy = PermissionPolicy::new(PermissionMode::Edit, vec![]).with_configured_floor(
            agens_core::ConfiguredFloor::governing(vec![PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::Exact("native::bash".into()),
                PermissionPattern::Any,
            )]),
        );
        let call = HeadlessToolCall {
            id: "bash-rm".into(),
            name: "native::bash".into(),
            input: r#"{"command":"rm -f conformance/manifest/.gitkeep"}"#.into(),
        };
        let cancellation = HeadlessTurnCancellation::new();
        let mut gate = ProductionPermissionGate::new(
            policy.clone(),
            Arc::clone(&grants),
            PermissionSession::new(),
            "project".into(),
            Arc::clone(&dispatcher),
            Arc::clone(&allowed),
            Arc::clone(&prompts),
        );
        let store = PermissionGrantStore::open(&directory).expect("grant store should open");
        let mut resolver = ProductionPermissionResolver::new(
            AllowOncePrompter,
            store,
            Arc::clone(&grants),
            Arc::clone(&prompts),
            ProductionPromptAuthorization {
                policy,
                session: PermissionSession::new(),
                project: "project".into(),
                dispatcher: Arc::clone(&dispatcher),
                allowed: Arc::clone(&allowed),
            },
        );

        assert_eq!(
            run_ready(gate.evaluate(&call, &cancellation)),
            Ok(PermissionDecision::Ask)
        );
        assert_eq!(
            run_ready(resolver.resolve(&call, &cancellation)),
            Ok(PermissionDecision::Allow),
            "human Allow must authorize even when the configured floor still says ask"
        );
        assert!(
            allowed.lock().expect("allowed").contains_key("bash-rm"),
            "the allowed map must hold the call so dispatch can run it"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A person taking longer than the turn deadline must still be able to allow
    /// a prompted call. The resolver strips the deadline before the surface wait
    /// and never maps an elapsed deadline to TimedOut around the prompt.
    #[test]
    fn permission_prompt_succeeds_after_the_turn_deadline_elapses_during_the_wait() {
        struct SleepingAllowPrompter;

        impl PermissionPrompter for SleepingAllowPrompter {
            fn prompt(
                &mut self,
                _: &PermissionPromptContext,
                cancellation: &HeadlessTurnCancellation,
            ) -> Result<PermissionPromptAnswer, HeadlessTurnPortError> {
                std::thread::sleep(std::time::Duration::from_millis(30));
                assert!(
                    !cancellation.is_expired(),
                    "the human-wait cancellation must not carry a deadline"
                );
                Ok(PermissionPromptAnswer::AllowOnce)
            }
        }

        struct PathTool;

        impl DispatchTool for PathTool {
            fn permission_target(
                &self,
                arguments: &serde_json::Value,
            ) -> Result<String, Error> {
                Ok(arguments["path"].as_str().unwrap_or_default().to_owned())
            }

            fn execute(
                &mut self,
                _: &agens_tools::ToolExecutionContext,
                _: serde_json::Value,
            ) -> Result<agens_tools::ToolOutput, Error> {
                Ok(agens_tools::ToolOutput::success("ok"))
            }
        }

        let directory = std::env::temp_dir().join(format!(
            "agens-permission-deadline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&directory);

        let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        dispatcher
            .lock()
            .expect("dispatcher lock")
            .register_native("native::write", ToolAccess::Write, PathTool)
            .expect("write tool should register");

        let grants = Arc::new(Mutex::new(Vec::new()));
        let allowed = Arc::new(Mutex::new(BTreeMap::new()));
        let prompts = Arc::new(Mutex::new(BTreeMap::new()));
        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                PermissionDecision::Ask,
                PermissionPattern::Exact("native::write".into()),
                PermissionPattern::Exact("notes.md".into()),
            )],
        );
        let call = HeadlessToolCall {
            id: "current".into(),
            name: "native::write".into(),
            input: r#"{"path":"notes.md","content":"body"}"#.into(),
        };
        let cancellation = HeadlessTurnCancellation::with_deadline(std::time::Duration::from_millis(5));
        let mut gate = ProductionPermissionGate::new(
            policy.clone(),
            Arc::clone(&grants),
            PermissionSession::new(),
            "project".into(),
            Arc::clone(&dispatcher),
            Arc::clone(&allowed),
            Arc::clone(&prompts),
        );
        let store = PermissionGrantStore::open(&directory).expect("grant store should open");
        let mut resolver = ProductionPermissionResolver::new(
            SleepingAllowPrompter,
            store,
            Arc::clone(&grants),
            Arc::clone(&prompts),
            ProductionPromptAuthorization {
                policy,
                session: PermissionSession::new(),
                project: "project".into(),
                dispatcher: Arc::clone(&dispatcher),
                allowed: Arc::clone(&allowed),
            },
        );

        assert_eq!(
            run_ready(gate.evaluate(&call, &cancellation)),
            Ok(PermissionDecision::Ask)
        );
        assert_eq!(
            run_ready(resolver.resolve(&call, &cancellation)),
            Ok(PermissionDecision::Allow),
            "an elapsed turn deadline during the prompt must not fail the allow"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }
}

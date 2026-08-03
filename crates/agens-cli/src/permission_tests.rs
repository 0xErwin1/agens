//! Permission-policy tests that drive a real dispatch through the CLI's test
//! support, so they live here rather than inside `agens-permissions`.

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::future::ready;
    use std::sync::{Arc, Mutex};

    use agens_config::{ConfigPermissionDecision, ConfigPermissionRule, ConfigPermissionScope};
    use agens_core::{
        DiscardCompletedTurnRepository, FactPath, HeadlessPermissionGate,
        HeadlessPermissionResolver, HeadlessToolCall, HeadlessTurnCancellation, HeadlessTurnError,
        HeadlessTurnPortError, MessagePart, PermissionDecision, PermissionMode, PermissionPattern,
        PermissionPolicy, PermissionRule, PermissionSession, ToolInput, ToolOutcome,
        ToolResultFacts, TurnEvent, TurnProvider, TurnState,
    };
    use agens_store::PermissionGrantStore;
    use agens_tools::{
        DispatchTool, RemoteToolMetadata, ToolDispatchRequest, ToolDispatcher,
        ToolEvaluationOutcome, ToolExecutionContext, ToolOutput,
    };

    use agens_dispatch::ProductionToolDispatcher;
    use agens_permissions::*;
    use agens_tool_runtime::block_on_headless_turn;
    use agens_tui_app::test_support::{
        ProductionBatchInput, batch_call, native_batch_call, run_production_batch,
        run_production_batch_with_policy,
    };

    #[test]
    fn production_permission_infrastructure_failure_remains_terminal() {
        struct ToolCallingProvider;

        impl TurnProvider for ToolCallingProvider {
            fn next_parts(
                &mut self,
                _events: &[TurnEvent],
                _cancellation: &HeadlessTurnCancellation,
            ) -> impl std::future::Future<Output = Result<Vec<MessagePart>, HeadlessTurnPortError>> + Send
            {
                ready(Ok(vec![MessagePart::ToolCall {
                    id: "call".into(),
                    name: "native::read".into(),
                    input: r#"{"path":"notes.md"}"#.into(),
                }]))
            }
        }

        struct DenyingResolver;

        impl HeadlessPermissionResolver for DenyingResolver {
            fn resolve(
                &mut self,
                _call: &HeadlessToolCall,
                _cancellation: &HeadlessTurnCancellation,
            ) -> impl std::future::Future<
                Output = Result<PermissionDecision, HeadlessTurnPortError>,
            > + Send {
                ready(Ok(PermissionDecision::Deny))
            }
        }

        let grants = Arc::new(Mutex::new(Vec::new()));
        let poisoned = Arc::clone(&grants);
        assert!(
            std::thread::spawn(move || {
                let _guard = poisoned.lock().unwrap();
                panic!("poison grants lock");
            })
            .join()
            .is_err()
        );
        let dispatcher = Arc::new(Mutex::new(ToolDispatcher::new()));
        let allowed = Arc::new(Mutex::new(BTreeMap::new()));
        let mut gate = ProductionPermissionGate::new(
            PermissionPolicy::new(PermissionMode::Edit, Vec::new()),
            grants,
            PermissionSession::new(),
            "project".into(),
            Arc::clone(&dispatcher),
            Arc::clone(&allowed),
            Arc::new(Mutex::new(BTreeMap::new())),
        );
        let mut tool_dispatcher = ProductionToolDispatcher::new(dispatcher, allowed);

        let result = block_on_headless_turn(agens_core::run_headless_turn(
            &mut ToolCallingProvider,
            &mut gate,
            &mut DenyingResolver,
            &mut tool_dispatcher,
            &mut DiscardCompletedTurnRepository,
            &HeadlessTurnCancellation::new(),
        ))
        .unwrap();

        assert_eq!(result, Err(HeadlessTurnError::PermissionEvaluation));
    }

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
                "native::git_read",
                serde_json::json!({"operation": "status"}),
                NativePermissionTarget::Operation("status".into()),
            ),
            (
                "native::grep",
                serde_json::json!({"pattern": "permission"}),
                NativePermissionTarget::Search {
                    pattern: "permission".into(),
                    path: None,
                },
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

    /// A search is named by its pattern and reads whatever its path points at.
    /// Keeping both is what lets a rule written against the file select the
    /// call that would read it; projecting only the pattern left every path
    /// rule unable to reach a tool that reports the lines it matched.
    #[test]
    fn native_permission_target_keeps_grep_path_beside_its_pattern() {
        let with_path = NativePermissionTarget::parse(
            "native::grep",
            &serde_json::json!({"pattern": "TODO", "path": "crates/agens-cli"}),
        )
        .expect("a grep call must parse");

        assert_eq!(
            with_path,
            NativePermissionTarget::Search {
                pattern: "TODO".into(),
                path: Some("crates/agens-cli".into()),
            }
        );
        assert_eq!(
            with_path.reach(),
            vec![agens_core::PermissionReach::Path("crates/agens-cli".into())]
        );
        assert_eq!(
            NativePermissionTarget::parse("native::grep", &serde_json::json!({"pattern": "TODO"}))
                .expect("a grep call must parse")
                .reach(),
            Vec::new(),
            "a search given no path names no file, so nothing here decides which it may read"
        );
    }

    /// `glob` reports the paths its pattern names and never their contents, and
    /// it takes no path argument to read a file through, so its pattern is the
    /// whole of what it reaches.
    #[test]
    fn native_permission_target_gives_glob_no_reach_beyond_its_pattern() {
        assert_eq!(
            NativePermissionTarget::parse(
                "native::glob",
                &serde_json::json!({"pattern": "src/**/*.rs"}),
            )
            .expect("a glob call must parse")
            .reach(),
            Vec::new()
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
            ("native::git_read", "operation"),
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

        for (name, input, expected_content) in [
            ("native::list", "{malformed", "invalid tool arguments"),
            ("native::glob", r#"{}"#, "invalid tool arguments"),
            ("native::unknown", r#"{"path":"src"}"#, "permission denied"),
            (
                "native::grep",
                r#"{"pattern":"TODO","_inject_permission_evaluator_failure":true}"#,
                "invalid tool arguments",
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
            assert!(invalid.result.is_ok());
            assert!(invalid.prompts.is_empty());
            assert!(invalid.executions.is_empty());
            assert!(
                invalid.progress.iter().any(|event| {
                    matches!(
                        event,
                        TurnEvent::ToolResult(MessagePart::ToolResult {
                            is_error: true,
                            content,
                            ..
                        }) if content == expected_content
                    )
                }),
                "{name} should report {expected_content:?}"
            );
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
                    content: "tool execution cancelled".into(),
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

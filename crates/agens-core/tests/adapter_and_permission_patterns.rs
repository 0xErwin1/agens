use std::time::Duration;

use agens_core::{
    HeadlessTurnCancellation, MAX_PERMISSION_GLOB_SEGMENTS, MAX_PERMISSION_TARGET_BYTES,
    PermissionDecision, PermissionMode, PermissionPattern, PermissionPatternError,
    PermissionPolicy, PermissionRequest, PermissionRule, PermissionScope, PermissionSession,
    PermissionTarget, PermissionTargetKind, ProjectPermissionGrant, ToolAccess,
    permission_target_kind_for_tool,
};

#[test]
fn cancellation_adapter_view_is_cloneable_read_only_and_observes_live_cancellation() {
    let cancellation = HeadlessTurnCancellation::with_deadline(Duration::from_secs(1));
    let adapter = cancellation.adapter_view();
    let cloned_adapter = adapter.clone();
    let deadline = adapter.deadline().expect("deadline should be available");
    let remaining = adapter
        .remaining_duration()
        .expect("remaining duration should be available");

    assert!(!adapter.is_cancelled());
    assert!(!cloned_adapter.is_cancelled());
    assert!(deadline > std::time::Instant::now());
    assert!(remaining > Duration::ZERO);
    assert!(remaining <= Duration::from_secs(1));

    cancellation.cancel();

    assert!(adapter.is_cancelled());
    assert!(cloned_adapter.is_cancelled());
}

#[test]
fn cancellation_adapter_distinguishes_absent_and_elapsed_deadlines() {
    let no_deadline = HeadlessTurnCancellation::new().adapter_view();
    let elapsed_deadline = HeadlessTurnCancellation::with_deadline(Duration::ZERO).adapter_view();

    assert_eq!(no_deadline.deadline(), None);
    assert_eq!(no_deadline.remaining_duration(), None);
    assert_eq!(elapsed_deadline.remaining_duration(), Some(Duration::ZERO));
}

#[test]
fn validated_target_globs_match_paths_with_documented_segment_semantics() {
    let cases = [
        ("資料/**/*.txt", "資料/plan.txt", true),
        ("資料/**/*.txt", "資料/notes/plan.txt", true),
        ("資料/**/*.txt", "資料/notes/plan.md", false),
        ("file*.txt", "file9.txt", true),
        ("file*.txt", "dir/file9.txt", false),
        ("dir/**/secret", "dir/secret", true),
        ("dir/**/secret", "dir/nested/secret", true),
        ("dir/**/secret", "other/secret", false),
    ];

    for (pattern, target, expected) in cases {
        let pattern = PermissionPattern::glob(pattern).expect("glob should be valid");

        assert_eq!(
            pattern.matches(target),
            expected,
            "pattern {pattern:?} should have matched {target:?} as {expected}"
        );
    }
}

#[test]
fn a_bare_star_target_does_not_cross_a_slash_for_a_path_shaped_target() {
    let narrow = PermissionPattern::glob("rm*").expect("glob should be valid");
    let path_aware = PermissionPattern::glob("rm -rf /**").expect("glob should be valid");

    assert!(narrow.matches("rm -rf"));
    assert!(
        !narrow.matches("rm -rf /tmp/x"),
        "path-shaped targets (read/write/edit/list/search paths) must keep segment discipline: \
         a bare * never crosses a /, only an explicit ** does"
    );
    assert!(path_aware.matches("rm -rf /tmp/x"));
}

/// `bash` (and `git_read`'s operation keyword) are free-form text, not
/// filesystem paths, even though a shell command routinely contains `/`.
/// Regression-pins the previously silent failure of the most common
/// deny-by-prefix rules a user would write for `bash`.
#[test]
fn a_bare_star_target_crosses_a_slash_for_a_free_form_command_target() {
    let cases = [
        ("git reset --hard*", "git reset --hard origin/main"),
        ("git push*", "git push origin feature/x"),
        ("git rebase*", "git rebase origin/main"),
        ("rm*", "rm -rf /tmp/x"),
    ];

    for (pattern, command) in cases {
        let free_form =
            PermissionPattern::glob_for_target_kind(pattern, PermissionTargetKind::FreeFormText)
                .expect("glob should be valid");

        assert!(
            free_form.matches(command),
            "free-form command pattern {pattern:?} should have matched {command:?}"
        );
    }
}

#[test]
fn permission_target_kind_classifies_bash_and_git_read_as_free_form_and_everything_else_as_path() {
    for tool in ["bash", "native::bash", "git_read", "native::git_read"] {
        assert_eq!(
            permission_target_kind_for_tool(tool),
            PermissionTargetKind::FreeFormText,
            "{tool} should classify as free-form text"
        );
    }

    for tool in [
        "read",
        "native::read",
        "write",
        "edit",
        "list",
        "search",
        "glob",
        "native::glob",
        "grep",
        "native::grep",
        "webfetch",
        "native::webfetch",
    ] {
        assert_eq!(
            permission_target_kind_for_tool(tool),
            PermissionTargetKind::Path,
            "{tool} should classify as a path-shaped target"
        );
    }
}

#[test]
fn malformed_target_globs_are_rejected_by_the_safe_constructor() {
    for pattern in ["", "   ", "file[", "file[z-a].txt"] {
        assert!(matches!(
            PermissionPattern::glob(pattern),
            Err(PermissionPatternError::InvalidGlob { .. })
        ));
    }
}

#[test]
fn oversized_glob_patterns_are_rejected_by_bytes_and_segments() {
    let oversized_bytes = "a".repeat(400_001);
    let oversized_segments = std::iter::repeat_n("a", MAX_PERMISSION_GLOB_SEGMENTS + 1)
        .collect::<Vec<_>>()
        .join("/");

    for pattern in [oversized_bytes, oversized_segments] {
        let error = PermissionPattern::glob(pattern).expect_err("glob should exceed a limit");
        let PermissionPatternError::GlobTooLarge { actual, limit } = error else {
            panic!("glob should return a typed size error");
        };

        assert!(actual > limit);
    }
}

#[test]
fn oversized_glob_targets_fail_closed_before_matching() {
    let pattern = PermissionPattern::glob("src/**").expect("glob should be valid");
    let target_within_limit = format!("src/{}", "a".repeat(MAX_PERMISSION_TARGET_BYTES - 4));
    let oversized_target = format!("src/{}", "a".repeat(MAX_PERMISSION_TARGET_BYTES));

    assert!(pattern.matches(&target_within_limit));
    assert!(!pattern.matches(&oversized_target));
}

#[test]
fn any_and_exact_patterns_remain_literal_and_unicode_safe() {
    assert!(PermissionPattern::Any.matches("資料/plan.txt"));
    assert!(PermissionPattern::Exact("資料/plan.txt".into()).matches("資料/plan.txt"));
    assert!(!PermissionPattern::Exact("資料/plan.txt".into()).matches("資料/notes/plan.txt"));
}

#[test]
fn glob_rules_preserve_deny_mode_allow_grant_and_bypass_precedence() {
    let request = PermissionRequest::new(
        "project",
        "read",
        "src/private/secret.txt",
        ToolAccess::ReadOnly,
    );
    let tool = PermissionPattern::Exact("read".into());
    let target = PermissionPattern::glob("src/**").expect("glob should be valid");
    let deny_target = PermissionPattern::glob("src/private/**").expect("glob should be valid");

    let global_deny = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Deny,
            tool.clone(),
            deny_target,
        )],
    );
    assert_eq!(
        global_deny.evaluate(&request, &[], &PermissionSession::with_temporary_bypass()),
        PermissionDecision::Deny
    );

    let chat_mode = PermissionPolicy::new(
        PermissionMode::Chat,
        vec![PermissionRule::global(
            PermissionDecision::Allow,
            tool.clone(),
            target.clone(),
        )],
    );
    let write_request =
        PermissionRequest::new("project", "read", "src/write.txt", ToolAccess::Write);
    assert_eq!(
        chat_mode.evaluate(&write_request, &[], &PermissionSession::new()),
        PermissionDecision::Deny
    );

    let allow = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Allow,
            tool.clone(),
            target.clone(),
        )],
    );
    assert_eq!(
        allow.evaluate(&request, &[], &PermissionSession::new()),
        PermissionDecision::Allow
    );

    let grant = ProjectPermissionGrant::allow("project", tool.clone(), target.clone());
    let no_static_match = PermissionPolicy::new(PermissionMode::Edit, vec![]);
    assert_eq!(
        no_static_match.evaluate(&request, &[grant], &PermissionSession::new()),
        PermissionDecision::Allow
    );

    let ask = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule {
            scope: PermissionScope::Project,
            project: Some("project".into()),
            decision: PermissionDecision::Ask,
            tool,
            target,
        }],
    );
    assert_eq!(
        ask.evaluate(&request, &[], &PermissionSession::new()),
        PermissionDecision::Ask
    );
    // A matched `Ask` is a declaration, not a default: it must survive
    // `temporary_bypass` rather than being silently upgraded to `Allow`.
    // Only an *unmatched* call falls through to the bypass fallback.
    assert_eq!(
        ask.evaluate(&request, &[], &PermissionSession::with_temporary_bypass()),
        PermissionDecision::Ask
    );
}

#[test]
fn a_matched_ask_survives_bypass_and_dangerous_mode_regardless_of_its_source() {
    let tool = PermissionPattern::Exact("native::bash".into());
    let target = PermissionPattern::Any;

    for rule in [
        PermissionRule::global(PermissionDecision::Ask, tool.clone(), target.clone()),
        PermissionRule::project(
            "project",
            PermissionDecision::Ask,
            tool.clone(),
            target.clone(),
        ),
    ] {
        let policy = PermissionPolicy::new(PermissionMode::Edit, vec![rule]);
        let request =
            PermissionRequest::new("project", "native::bash", "echo hi", ToolAccess::ReadOnly);

        assert_eq!(
            policy.evaluate(&request, &[], &PermissionSession::new()),
            PermissionDecision::Ask
        );
        assert_eq!(
            policy.evaluate(&request, &[], &PermissionSession::with_temporary_bypass()),
            PermissionDecision::Ask,
            "a matched Ask must survive bypass no matter which static-rule producer emitted it"
        );
        assert_eq!(
            policy.evaluate_with_unmatched_override(
                &request,
                &[],
                &[],
                &PermissionSession::new(),
                true,
            ),
            PermissionDecision::Ask,
            "a matched Ask must survive the dangerous-mode unmatched-call fallback too"
        );
    }
}

#[test]
fn precedence_matrix_matched_decisions_always_outrank_the_unmatched_call_fallback() {
    let tool = PermissionPattern::Exact("native::write".into());
    let target = PermissionPattern::Any;
    let request = PermissionRequest::new("project", "native::write", "notes.md", ToolAccess::Write);

    for (decision, expected) in [
        (PermissionDecision::Allow, PermissionDecision::Allow),
        (PermissionDecision::Deny, PermissionDecision::Deny),
        (PermissionDecision::Ask, PermissionDecision::Ask),
    ] {
        let policy = PermissionPolicy::new(
            PermissionMode::Edit,
            vec![PermissionRule::global(
                decision,
                tool.clone(),
                target.clone(),
            )],
        );

        for session in [
            PermissionSession::new(),
            PermissionSession::with_temporary_bypass(),
        ] {
            for unmatched_allow in [false, true] {
                assert_eq!(
                    policy.evaluate_with_unmatched_override(
                        &request,
                        &[],
                        &[],
                        &session,
                        unmatched_allow,
                    ),
                    expected,
                    "matched {decision:?} must not move under bypass={session:?} unmatched_allow={unmatched_allow}"
                );
            }
        }
    }

    let unmatched_policy = PermissionPolicy::new(PermissionMode::Edit, Vec::new());
    let other_request =
        PermissionRequest::new("project", "native::read", "notes.md", ToolAccess::ReadOnly);

    assert_eq!(
        unmatched_policy.evaluate_with_unmatched_override(
            &other_request,
            &[],
            &[],
            &PermissionSession::new(),
            false,
        ),
        PermissionDecision::Ask
    );
    assert_eq!(
        unmatched_policy.evaluate_with_unmatched_override(
            &other_request,
            &[],
            &[],
            &PermissionSession::new(),
            true,
        ),
        PermissionDecision::Allow
    );
    assert_eq!(
        unmatched_policy.evaluate_with_unmatched_override(
            &other_request,
            &[],
            &[],
            &PermissionSession::with_temporary_bypass(),
            false,
        ),
        PermissionDecision::Allow
    );
}

#[test]
fn a_static_rule_deny_survives_a_later_matching_project_or_session_grant() {
    let tool = PermissionPattern::Exact("native::write".into());
    let target = PermissionPattern::Any;
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Deny,
            tool.clone(),
            target.clone(),
        )],
    );
    let request = PermissionRequest::new("project", "native::write", "notes.md", ToolAccess::Write);
    let project_grant = ProjectPermissionGrant::allow("project", tool.clone(), target.clone());
    let session_grant = ProjectPermissionGrant::allow("project", tool, target);

    assert_eq!(
        policy.evaluate(
            &request,
            std::slice::from_ref(&project_grant),
            &PermissionSession::new()
        ),
        PermissionDecision::Deny,
        "a persisted AllowAlways project grant must not outrank a declared deny"
    );
    assert_eq!(
        policy.evaluate_with_session_grants(
            &request,
            &[project_grant],
            &[session_grant],
            &PermissionSession::new(),
        ),
        PermissionDecision::Deny,
        "a persisted AllowAlways session grant must not outrank a declared deny"
    );
}

#[test]
fn permission_precedence_scans_config_grants_and_session_in_order() {
    let request = PermissionRequest::new("project", "write", "src/lib.rs", ToolAccess::Write);
    let tool = PermissionPattern::Exact("write".into());
    let target = PermissionPattern::Exact("src/lib.rs".into());
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![
            PermissionRule::global(PermissionDecision::Deny, tool.clone(), target.clone()),
            PermissionRule::project(
                "project",
                PermissionDecision::Allow,
                tool.clone(),
                target.clone(),
            ),
        ],
    );
    let grants = [ProjectPermissionGrant::new(
        "project",
        PermissionDecision::Deny,
        tool.clone(),
        target.clone(),
    )];
    let session = [ProjectPermissionGrant::allow("project", tool, target)];

    assert_eq!(
        policy.evaluate_with_session_grants(&request, &grants, &session, &PermissionSession::new()),
        PermissionDecision::Allow
    );
}

#[test]
fn permission_targets_project_paths_commands_urls_and_tool_inputs_to_the_shared_bound() {
    for value in [
        "x".repeat(MAX_PERMISSION_TARGET_BYTES + 1),
        "é".repeat(MAX_PERMISSION_TARGET_BYTES),
    ] {
        let targets = [
            PermissionTarget::path(&value),
            PermissionTarget::command(&value),
            PermissionTarget::url(&value),
            PermissionTarget::native(&value),
            PermissionTarget::mcp(&value),
        ];

        for target in targets {
            assert!(target.project().len() <= MAX_PERMISSION_TARGET_BYTES);
        }
    }
}

#[test]
fn multibyte_target_projection_keeps_configured_denies_effective_under_bypass() {
    let policy = PermissionPolicy::new(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Deny,
            PermissionPattern::Exact("read".into()),
            PermissionPattern::glob("a*").expect("glob should be valid"),
        )],
    );

    for suffix in ['€', '😀'] {
        let target = format!("{}{}", "a".repeat(MAX_PERMISSION_TARGET_BYTES - 1), suffix);
        let request = PermissionRequest::new("project", "read", target, ToolAccess::ReadOnly);

        assert!(request.target.len() <= MAX_PERMISSION_TARGET_BYTES);
        assert_eq!(
            policy.evaluate(&request, &[], &PermissionSession::with_temporary_bypass(),),
            PermissionDecision::Deny,
            "a configured Deny must not become bypassable Ask for {suffix:?}",
        );
    }
}

#[test]
fn safety_predicates_precede_rules_and_bypass() {
    let tool = PermissionPattern::Exact("write".into());
    let target = PermissionPattern::Any;
    let policy = PermissionPolicy::with_safety_predicates(
        PermissionMode::Edit,
        vec![PermissionRule::global(
            PermissionDecision::Allow,
            tool.clone(),
            target.clone(),
        )],
        vec![agens_core::SafetyPredicate::WorktreeEscape],
    );
    let escaped = PermissionRequest::new("project", "write", "src/lib.rs", ToolAccess::Write)
        .outside_worktree();

    assert_eq!(
        policy.evaluate(&escaped, &[], &PermissionSession::with_temporary_bypass()),
        PermissionDecision::Deny
    );
}

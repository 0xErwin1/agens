use std::time::Duration;

use agens_core::{
    HeadlessTurnCancellation, MAX_PERMISSION_GLOB_SEGMENTS, MAX_PERMISSION_TARGET_BYTES,
    PermissionDecision, PermissionMode, PermissionPattern, PermissionPatternError,
    PermissionPolicy, PermissionRequest, PermissionRule, PermissionScope, PermissionSession,
    ProjectPermissionGrant, ToolAccess,
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
    assert_eq!(
        ask.evaluate(&request, &[], &PermissionSession::with_temporary_bypass()),
        PermissionDecision::Allow
    );
}

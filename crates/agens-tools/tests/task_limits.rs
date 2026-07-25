use agens_tools::{TaskExecutionLimits, TaskExecutionRegistry, TaskLaunchMode};

#[test]
fn the_default_registry_admits_the_documented_number_of_subagents() {
    let registry = TaskExecutionRegistry::new();

    for _ in 0..TaskExecutionLimits::default().max_concurrency {
        assert!(registry.admit(TaskLaunchMode::Background).is_some());
    }

    assert!(registry.admit(TaskLaunchMode::Background).is_none());
}

#[test]
fn a_configured_concurrency_bounds_admission() {
    let limits = TaskExecutionLimits {
        max_concurrency: 1,
        ..TaskExecutionLimits::default()
    };
    let registry = TaskExecutionRegistry::with_limits(limits);

    assert!(registry.admit(TaskLaunchMode::Background).is_some());
    assert!(registry.admit(TaskLaunchMode::Background).is_none());
}

#[test]
fn a_registry_reports_the_limits_it_was_built_with() {
    let limits = TaskExecutionLimits {
        max_iterations: 3,
        max_concurrency: 2,
        max_output_chars: 2_048,
    };

    assert_eq!(TaskExecutionRegistry::with_limits(limits).limits(), limits);
    assert_eq!(
        TaskExecutionRegistry::new().limits(),
        TaskExecutionLimits::default()
    );
}

#[test]
fn the_documented_defaults_match_the_previously_hardcoded_bounds() {
    let limits = TaskExecutionLimits::default();

    assert_eq!(limits.max_iterations, 16);
    assert_eq!(limits.max_concurrency, 4);
    assert_eq!(limits.max_output_chars, 65_536);
}

use std::time::Duration;

use agens_core::HeadlessTurnCancellation;
use agens_tools::{ToolExecutionContext, ToolExecutionStatus};

#[test]
fn headless_adapter_preserves_live_cancellation_and_deadline() {
    let cancellation = HeadlessTurnCancellation::with_deadline(Duration::from_secs(1));
    let context = ToolExecutionContext::from_headless_adapter(cancellation.adapter_view());

    assert_eq!(context.check(), Ok(()));
    cancellation.cancel();
    assert_eq!(context.check(), Err(ToolExecutionStatus::Cancelled));
}

#[test]
fn headless_adapter_reports_an_elapsed_deadline() {
    let cancellation = HeadlessTurnCancellation::with_deadline(Duration::ZERO);
    let context = ToolExecutionContext::from_headless_adapter(cancellation.adapter_view());

    assert_eq!(context.check(), Err(ToolExecutionStatus::TimedOut));
}

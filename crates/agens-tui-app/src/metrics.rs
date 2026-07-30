use agens_permissions::sanitize_metric;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use agens_core::{MessagePart, TurnEvent, TurnState};
use agens_tools::{McpErrorCategory, McpLifecycleState, McpStatusHandle};
use agens_tui::{BridgeCancel, BridgeTx, DiffLine, DiffLineKind, ToolResultState, TuiRuntimeEvent};

use agens_error::CliError;
use agens_permissions::ParseToolInput;

/// The shared MCP status handle and the set of server names already noticed
/// this session, so a name is never re-noticed until it recovers to `Ready`.
struct McpNotices {
    status: McpStatusHandle,
    noticed: Arc<Mutex<BTreeSet<String>>>,
}

pub struct TuiMetricsPublisher {
    bridge: BridgeTx<TuiRuntimeEvent>,
    cancellation: BridgeCancel,
    model_id: String,
    turn_started_at: Option<std::time::Instant>,
    tools: BTreeMap<String, (String, std::time::Instant)>,
    mcp: Option<McpNotices>,
}

impl TuiMetricsPublisher {
    pub fn new(
        bridge: BridgeTx<TuiRuntimeEvent>,
        cancellation: BridgeCancel,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            bridge,
            cancellation,
            model_id: model_id.into(),
            turn_started_at: None,
            tools: BTreeMap::new(),
            mcp: None,
        }
    }

    /// Adds first-discovery MCP failure notices to this publisher.
    ///
    /// `noticed` is expected to be shared across every publisher built for
    /// the same session, so a server name is noticed at most once per
    /// failure and freshly re-noticed only after it recovers to `Ready`.
    pub fn with_mcp_notices(
        mut self,
        status: McpStatusHandle,
        noticed: Arc<Mutex<BTreeSet<String>>>,
    ) -> Self {
        self.mcp = Some(McpNotices { status, noticed });
        self
    }

    /// Publishes one `Notice` per enabled server currently `Failed` or
    /// `Degraded` that has not already been noticed, and clears any server
    /// that has recovered to `Ready` from the noticed set so a later
    /// failure notices again.
    ///
    /// The notice text is composed exclusively from the server name and the
    /// error category's closed label — never from the server's raw error
    /// message — so remote-controlled text can never reach it even if a
    /// future caller of `register_failed_server` forgets to sanitize its
    /// own message.
    fn publish_mcp_notices(&self) {
        let Some(mcp) = self.mcp.as_ref() else {
            return;
        };
        let Ok(mut noticed) = mcp.noticed.lock() else {
            return;
        };
        for server in mcp.status.snapshot().servers() {
            let descriptor = server.descriptor();
            if server.state() == McpLifecycleState::Ready {
                noticed.remove(descriptor.name());
                continue;
            }
            if !descriptor.enabled()
                || !matches!(
                    server.state(),
                    McpLifecycleState::Failed | McpLifecycleState::Degraded
                )
            {
                continue;
            }
            if !noticed.insert(descriptor.name().to_owned()) {
                continue;
            }
            let category = server
                .last_error()
                .map_or(McpErrorCategory::Unavailable, |error| error.category());
            let text = format!(
                "mcp: {} failed to connect ({})",
                descriptor.name(),
                category.label()
            );
            let _ = self
                .bridge
                .publish(TuiRuntimeEvent::Notice(text), &self.cancellation, None);
        }
    }

    pub fn observe(&mut self, event: &TurnEvent) {
        let now = std::time::Instant::now();
        let completed_tool = match event {
            TurnEvent::ToolResult(MessagePart::ToolResult { tool_call_id, .. }) => {
                self.tools.remove(tool_call_id)
            }
            _ => None,
        };
        let first_requesting = matches!(event, TurnEvent::StateChanged(TurnState::Requesting))
            && self.turn_started_at.is_none();
        let metric = match event {
            TurnEvent::StateChanged(TurnState::Requesting) => {
                if self.turn_started_at.is_none() {
                    self.turn_started_at = Some(now);
                    Some(TuiRuntimeEvent::TurnStarted)
                } else {
                    None
                }
            }
            TurnEvent::StateChanged(
                TurnState::Completed | TurnState::Cancelled | TurnState::Failed,
            ) => None,
            TurnEvent::Usage(usage) => {
                let mut usage = usage.clone();
                if usage.context_window.is_none() {
                    usage.context_window = agens_models::context_window_for(&self.model_id);
                }
                Some(TuiRuntimeEvent::Usage(usage))
            }
            TurnEvent::ToolCallRequested { id, name, input } => {
                self.tools.insert(id.clone(), (name.clone(), now));
                Some(TuiRuntimeEvent::ToolStarted {
                    call_id: id.clone(),
                    name: name.clone(),
                    input: sanitize_metric(input),
                    parsed: agens_core::ToolInput::parse(name, input),
                })
            }
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id,
                is_error,
                ..
            }) => {
                let duration = completed_tool
                    .as_ref()
                    .map(|(_, started)| now.duration_since(*started));
                Some(TuiRuntimeEvent::ToolEnded {
                    call_id: tool_call_id.clone(),
                    duration,
                    result: if *is_error {
                        ToolResultState::Failure
                    } else {
                        ToolResultState::Success
                    },
                })
            }
            TurnEvent::ProviderPart(_) | TurnEvent::StateChanged(_) => None,
            TurnEvent::ToolResult(_) => None,
            TurnEvent::ToolResultFacts { .. } => None,
        };

        if let Some(event) = metric {
            let _ = self.bridge.publish(event, &self.cancellation, None);
        }

        if first_requesting {
            self.publish_mcp_notices();
        }

        if let TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id,
            content,
            is_error: false,
        }) = event
            && completed_tool
                .as_ref()
                .is_some_and(|(name, _)| name.ends_with("::edit"))
        {
            let lines = parse_edit_diff(&sanitize_metric(content));
            if !lines.is_empty() {
                let _ = self.bridge.publish(
                    TuiRuntimeEvent::Diff {
                        call_id: tool_call_id.clone(),
                        lines,
                    },
                    &self.cancellation,
                    None,
                );
            }
        }
    }

    pub fn finish(&mut self, result: Result<(), &CliError>) {
        let status = match result {
            Ok(()) => TurnState::Completed,
            Err(error) if error.category == "cancelled" => TurnState::Cancelled,
            Err(_) => TurnState::Failed,
        };
        let duration = self.turn_started_at.take().map(|started| started.elapsed());
        let _ = self.bridge.publish(
            TuiRuntimeEvent::TurnEnded { status, duration },
            &self.cancellation,
            None,
        );
    }
}

pub fn finish_tui_metrics<T>(
    metrics: &Arc<Mutex<TuiMetricsPublisher>>,
    result: &Result<T, CliError>,
) {
    if let Ok(mut metrics) = metrics.lock() {
        metrics.finish(result.as_ref().map(|_| ()));
    }
}

fn parse_edit_diff(diff: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let mut old_number = 0;
    let mut new_number = 0;

    for line in diff.lines() {
        if let Some((old, new)) = parse_diff_hunk(line) {
            old_number = old;
            new_number = new;
        } else if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        } else if let Some(text) = line.strip_prefix('-') {
            lines.push(DiffLine::new(old_number, DiffLineKind::Removed, text));
            old_number += 1;
        } else if let Some(text) = line.strip_prefix('+') {
            lines.push(DiffLine::new(new_number, DiffLineKind::Added, text));
            new_number += 1;
        } else if line.starts_with(' ') {
            old_number += 1;
            new_number += 1;
        }
    }

    lines
}

fn parse_diff_hunk(line: &str) -> Option<(u32, u32)> {
    let ranges = line.strip_prefix("@@ -")?.strip_suffix(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    Some((
        old.split_once(',')?.0.parse().ok()?,
        new.split_once(',')?.0.parse().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use agens_core::{
        HeadlessTurnCancellation, HeadlessTurnError, HeadlessTurnPortError, TurnEvent,
    };

    use super::*;
    use crate::test_support::{batch_call, run_production_batch};
    use agens_permissions::PermissionPromptAnswer;

    #[test]
    fn tui_metrics_publish_one_terminal_after_the_production_turn_outcome() {
        let success = run_production_batch(
            "metrics-success",
            Vec::new(),
            vec![MessagePart::Text("complete".into())],
            None,
            None,
            false,
        );
        let cancellation = run_production_batch(
            "metrics-cancelled",
            vec![PermissionPromptAnswer::AllowOnce],
            vec![batch_call("first", "notes.md")],
            Some(HeadlessTurnCancellation::new()),
            None,
            false,
        );
        let provider_failure = run_production_batch(
            "metrics-provider-failure",
            Vec::new(),
            Vec::new(),
            None,
            Some(HeadlessTurnPortError::Provider),
            false,
        );
        let persistence_failure = run_production_batch(
            "metrics-persistence-failure",
            Vec::new(),
            vec![MessagePart::Text("complete".into())],
            None,
            None,
            true,
        );

        assert!(success.result.is_ok());
        assert!(matches!(
            success.metrics.as_slice(),
            [
                TuiRuntimeEvent::TurnStarted,
                TuiRuntimeEvent::TurnEnded {
                    status: TurnState::Completed,
                    duration: Some(_)
                },
            ]
        ));

        assert_eq!(cancellation.result, Err(HeadlessTurnError::Cancelled));
        assert!(matches!(
            cancellation.metrics.as_slice(),
            [
                TuiRuntimeEvent::TurnStarted,
                TuiRuntimeEvent::ToolStarted { call_id, .. },
                TuiRuntimeEvent::ToolEnded { call_id: ended_call_id, .. },
                TuiRuntimeEvent::TurnEnded { status: TurnState::Cancelled, duration: Some(_) },
            ] if call_id == "first" && ended_call_id == "first"
        ));

        assert_eq!(provider_failure.result, Err(HeadlessTurnError::Provider));
        assert!(matches!(
            provider_failure.metrics.as_slice(),
            [
                TuiRuntimeEvent::TurnStarted,
                TuiRuntimeEvent::TurnEnded {
                    status: TurnState::Failed,
                    duration: Some(_)
                },
            ]
        ));

        assert_eq!(persistence_failure.result, Err(HeadlessTurnError::Store));
        assert!(
            persistence_failure
                .progress
                .contains(&TurnEvent::StateChanged(TurnState::Completed))
        );
        assert!(matches!(
            persistence_failure.metrics.as_slice(),
            [
                TuiRuntimeEvent::TurnStarted,
                TuiRuntimeEvent::TurnEnded {
                    status: TurnState::Failed,
                    duration: Some(_)
                },
            ]
        ));
    }

    #[test]
    fn observe_on_tool_result_facts_publishes_nothing() {
        let (bridge, receiver) = agens_tui::BridgeTx::bounded(4);
        let cancellation = agens_tui::BridgeCancel::new();
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation, "unknown-model");

        let mut facts_source = agens_core::TurnCoordinator::new();
        facts_source.begin().unwrap();
        facts_source
            .accept_provider_part(MessagePart::ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                input: "{\"command\":\"exit 1\"}".into(),
            })
            .unwrap();
        facts_source.finish_provider_iteration().unwrap();
        facts_source
            .accept_tool_result(
                "call-1",
                "exit 1".into(),
                true,
                Some(agens_core::ToolResultFacts::Bash {
                    outcome: agens_core::ToolOutcome::Failed,
                    exit_code: Some(1),
                }),
            )
            .unwrap();
        let facts_event = facts_source
            .events()
            .iter()
            .find(|event| matches!(event, TurnEvent::ToolResultFacts { .. }))
            .expect("facts event must be present in the source coordinator");

        publisher.observe(facts_event);

        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "a facts event must publish nothing to the bridge"
        );
    }

    #[test]
    fn tui_metrics_production_publication_preserves_usage_tools_and_diffs_in_source_order() {
        let (bridge, receiver) = agens_tui::BridgeTx::bounded(16);
        let cancellation = agens_tui::BridgeCancel::new();
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation, "unknown-model");

        for event in [
            TurnEvent::StateChanged(TurnState::Requesting),
            TurnEvent::Usage(agens_core::Usage {
                input_tokens: Some(11),
                output_tokens: None,
                total_tokens: Some(17),
                context_window: None,
            }),
            TurnEvent::ToolCallRequested {
                id: "edit-1".into(),
                name: "native::edit".into(),
                input: r#"{"path":"notes.md","token":"SENTINEL"}"#.into(),
            },
            TurnEvent::ToolResult(MessagePart::ToolResult {
                tool_call_id: "edit-1".into(),
                content: "--- notes.md\n+++ notes.md\n@@ -1,1 +1,1 @@\n-old\n+new\n".into(),
                is_error: false,
            }),
        ] {
            publisher.observe(&event);
        }

        publisher.finish(Ok(()));

        let events = (0..6)
            .map(|_| {
                receiver
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .unwrap()
                    .into_parts()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            events
                .iter()
                .map(|(ordinal, _)| *ordinal)
                .collect::<Vec<_>>(),
            (0..6).collect::<Vec<_>>()
        );
        assert!(matches!(
            events.as_slice(),
            [
                (_, agens_tui::TuiRuntimeEvent::TurnStarted),
                (_, agens_tui::TuiRuntimeEvent::Usage(agens_core::Usage {
                    input_tokens: Some(11), output_tokens: None, total_tokens: Some(17), context_window: None,
                })),
                (_, agens_tui::TuiRuntimeEvent::ToolStarted { call_id, name, input, .. }),
                _, _, _,
            ] if call_id == "edit-1" && name == "native::edit" && input == "[redacted]"
        ));
        assert!(matches!(
            &events[3].1,
            agens_tui::TuiRuntimeEvent::ToolEnded {
                call_id,
                duration: Some(_),
                result: agens_tui::ToolResultState::Success,
            } if call_id == "edit-1"
        ));
        assert!(matches!(
            &events[4].1,
            agens_tui::TuiRuntimeEvent::Diff { call_id, lines }
                if call_id == "edit-1" && lines == &vec![
                    agens_tui::DiffLine::new(1, agens_tui::DiffLineKind::Removed, "old"),
                    agens_tui::DiffLine::new(1, agens_tui::DiffLineKind::Added, "new"),
                ]
        ));
        assert!(matches!(
            &events[5].1,
            agens_tui::TuiRuntimeEvent::TurnEnded {
                status: TurnState::Completed,
                duration: Some(_),
            }
        ));
    }

    #[test]
    fn tui_metrics_production_publication_keeps_missing_timing_and_failed_tool_state() {
        let (bridge, receiver) = agens_tui::BridgeTx::bounded(4);
        let cancellation = agens_tui::BridgeCancel::new();
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation, "unknown-model");

        publisher.observe(&TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "unknown".into(),
            content: "failed".into(),
            is_error: true,
        }));
        publisher.finish(Err(&CliError::runtime(HeadlessTurnError::Provider)));

        let events = (0..2)
            .map(|_| {
                receiver
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .unwrap()
                    .into_parts()
                    .1
            })
            .collect::<Vec<_>>();

        assert!(matches!(
            events.as_slice(),
            [
                agens_tui::TuiRuntimeEvent::ToolEnded {
                    call_id, duration: None, result: agens_tui::ToolResultState::Failure,
                },
                agens_tui::TuiRuntimeEvent::TurnEnded { status: TurnState::Failed, duration: None },
            ] if call_id == "unknown"
        ));

        publisher.observe(&TurnEvent::ToolCallRequested {
            id: "write-1".into(),
            name: "native::write".into(),
            input: r#"{"path":"notes.md"}"#.into(),
        });
        publisher.observe(&TurnEvent::ToolResult(MessagePart::ToolResult {
            tool_call_id: "write-1".into(),
            content: "--- notes.md\n+++ notes.md\n@@ -1,1 +1,1 @@\n-old\n+new\n".into(),
            is_error: false,
        }));

        let events = (0..2)
            .map(|_| {
                receiver
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .unwrap()
                    .into_parts()
                    .1
            })
            .collect::<Vec<_>>();

        assert!(matches!(
            events[0],
            agens_tui::TuiRuntimeEvent::ToolStarted { ref name, .. } if name == "native::write"
        ));
        assert!(matches!(
            events[1],
            agens_tui::TuiRuntimeEvent::ToolEnded {
                result: agens_tui::ToolResultState::Success,
                ..
            }
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn tui_metrics_publisher_enriches_context_window_from_registry_for_known_model() {
        let (bridge, receiver) = agens_tui::BridgeTx::bounded(4);
        let cancellation = agens_tui::BridgeCancel::new();
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation, "gpt-4.1");

        publisher.observe(&TurnEvent::Usage(agens_core::Usage {
            input_tokens: Some(11),
            output_tokens: None,
            total_tokens: Some(17),
            context_window: None,
        }));

        let event = receiver
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap()
            .into_parts()
            .1;

        assert!(matches!(
            event,
            agens_tui::TuiRuntimeEvent::Usage(agens_core::Usage {
                input_tokens: Some(11),
                output_tokens: None,
                total_tokens: Some(17),
                context_window: Some(1_047_576),
            })
        ));
    }

    #[test]
    fn tui_metrics_publisher_leaves_context_window_none_for_unknown_model() {
        let (bridge, receiver) = agens_tui::BridgeTx::bounded(4);
        let cancellation = agens_tui::BridgeCancel::new();
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation, "not-a-real-model-xyz");

        publisher.observe(&TurnEvent::Usage(agens_core::Usage {
            input_tokens: Some(3),
            output_tokens: Some(5),
            total_tokens: Some(8),
            context_window: None,
        }));

        let event = receiver
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap()
            .into_parts()
            .1;

        assert!(matches!(
            event,
            agens_tui::TuiRuntimeEvent::Usage(agens_core::Usage {
                total_tokens: Some(8),
                context_window: None,
                ..
            })
        ));
    }

    #[test]
    fn tui_metrics_publisher_preserves_provider_context_window_when_present() {
        let (bridge, receiver) = agens_tui::BridgeTx::bounded(4);
        let cancellation = agens_tui::BridgeCancel::new();
        let mut publisher = TuiMetricsPublisher::new(bridge, cancellation, "gpt-4.1");

        publisher.observe(&TurnEvent::Usage(agens_core::Usage {
            input_tokens: Some(1),
            output_tokens: Some(2),
            total_tokens: Some(3),
            context_window: Some(42),
        }));

        let event = receiver
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap()
            .into_parts()
            .1;

        assert!(matches!(
            event,
            agens_tui::TuiRuntimeEvent::Usage(agens_core::Usage {
                context_window: Some(42),
                ..
            })
        ));
    }

    /// A transport whose responses are scripted in advance, used to drive an
    /// MCP server through discovery without a real subprocess or socket.
    struct ScriptedTransport {
        responses: std::collections::VecDeque<
            Result<agens_tools::McpResponse, agens_tools::McpTransportError>,
        >,
    }

    impl agens_tools::McpTransport for ScriptedTransport {
        fn execute(
            &mut self,
            _request: agens_tools::McpRequest,
            _context: &agens_tools::McpOperationContext,
        ) -> Result<agens_tools::McpResponse, agens_tools::McpTransportError> {
            self.responses.pop_front().unwrap_or_else(|| {
                Err(agens_tools::McpTransportError::Protocol(
                    "missing deterministic response".into(),
                ))
            })
        }

        fn notify(
            &mut self,
            _request: agens_tools::McpRequest,
            _context: &agens_tools::McpOperationContext,
        ) -> Result<(), agens_tools::McpTransportError> {
            Ok(())
        }

        fn close(
            &mut self,
            _context: &agens_tools::McpOperationContext,
        ) -> Result<(), agens_tools::McpTransportError> {
            Ok(())
        }
    }

    fn ready_transport_responses()
    -> std::collections::VecDeque<Result<agens_tools::McpResponse, agens_tools::McpTransportError>>
    {
        std::collections::VecDeque::from([
            Ok(agens_tools::McpResponse::Initialized(
                agens_tools::McpInitializeResult::new(
                    agens_tools::MCP_PROTOCOL_VERSION,
                    serde_json::json!({"tools": {}}),
                ),
            )),
            Ok(agens_tools::McpResponse::ToolsListed(
                agens_tools::McpToolsPage::new(Vec::new(), None),
            )),
        ])
    }

    fn mcp_test_descriptor(name: &str, enabled: bool) -> agens_tools::McpServerDescriptor {
        agens_tools::McpServerDescriptor::new(
            name,
            agens_tools::McpServerSource::Global,
            agens_tools::McpServerTransport::Stdio,
            enabled,
            std::time::Duration::from_millis(500),
            None,
        )
    }

    #[test]
    fn tui_metrics_publisher_notices_failed_enabled_servers_once_and_refreshes_after_recovery_and_a_fresh_failure()
     {
        let status = agens_tools::McpStatusHandle::default();
        let mut registry = agens_tools::McpRegistry::with_status_handle(status.clone());

        registry
            .register_failed_server(
                mcp_test_descriptor("atlas", true),
                agens_tools::McpErrorCategory::Transport,
                "transport: SENTINEL_SECRET must never render",
            )
            .unwrap();
        registry
            .register_disabled_server(mcp_test_descriptor("vault", false))
            .unwrap();

        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory_attempts = std::sync::Arc::clone(&attempts);
        registry
            .configure_server_with_descriptor(
                mcp_test_descriptor("engram", true),
                move || {
                    if factory_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        Ok(Box::new(ScriptedTransport {
                            responses: ready_transport_responses(),
                        })
                            as Box<dyn agens_tools::McpTransport>)
                    } else {
                        Err(agens_tools::McpTransportError::Transport(
                            "engram unreachable".into(),
                        ))
                    }
                },
                agens_tools::McpTimeouts::new(
                    std::time::Duration::from_millis(50),
                    std::time::Duration::from_millis(50),
                    std::time::Duration::from_millis(50),
                )
                .unwrap(),
                agens_tools::McpLimits::default(),
            )
            .unwrap();
        assert!(!registry.discover_server("engram").is_failed());

        let noticed = Arc::new(Mutex::new(BTreeSet::new()));

        let (bridge_one, receiver_one) = agens_tui::BridgeTx::bounded(8);
        let mut publisher_one = TuiMetricsPublisher::new(bridge_one, BridgeCancel::new(), "model")
            .with_mcp_notices(status.clone(), Arc::clone(&noticed));
        publisher_one.observe(&TurnEvent::StateChanged(TurnState::Requesting));

        let first_turn_events = (0..2)
            .map(|_| {
                receiver_one
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .unwrap()
                    .into_parts()
                    .1
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            first_turn_events.as_slice(),
            [TuiRuntimeEvent::TurnStarted, TuiRuntimeEvent::Notice(text)]
                if text == "mcp: atlas failed to connect (transport)"
        ));
        assert!(receiver_one.try_recv().is_err());

        let (bridge_two, receiver_two) = agens_tui::BridgeTx::bounded(8);
        let mut publisher_two = TuiMetricsPublisher::new(bridge_two, BridgeCancel::new(), "model")
            .with_mcp_notices(status.clone(), Arc::clone(&noticed));
        publisher_two.observe(&TurnEvent::StateChanged(TurnState::Requesting));

        let second_turn_event = receiver_two
            .recv_timeout(std::time::Duration::from_millis(50))
            .unwrap()
            .into_parts()
            .1;
        assert!(matches!(second_turn_event, TuiRuntimeEvent::TurnStarted));
        assert!(
            receiver_two.try_recv().is_err(),
            "atlas must not be re-noticed while it stays failed"
        );

        assert!(registry.reload_server("engram").is_failed());

        let (bridge_three, receiver_three) = agens_tui::BridgeTx::bounded(8);
        let mut publisher_three =
            TuiMetricsPublisher::new(bridge_three, BridgeCancel::new(), "model")
                .with_mcp_notices(status.clone(), Arc::clone(&noticed));
        publisher_three.observe(&TurnEvent::StateChanged(TurnState::Requesting));

        let third_turn_events = (0..2)
            .map(|_| {
                receiver_three
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .unwrap()
                    .into_parts()
                    .1
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            third_turn_events.as_slice(),
            [TuiRuntimeEvent::TurnStarted, TuiRuntimeEvent::Notice(text)]
                if text == "mcp: engram failed to connect (transport)"
        ));
        assert!(receiver_three.try_recv().is_err());

        for text in [
            format!("{first_turn_events:?}"),
            format!("{third_turn_events:?}"),
        ] {
            assert!(!text.contains("SENTINEL_SECRET"), "{text}");
        }
    }
}

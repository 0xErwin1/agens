use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agens_core::{MessagePart, TurnEvent, TurnState};
use agens_tui::{BridgeCancel, BridgeTx, DiffLine, DiffLineKind, ToolResultState, TuiRuntimeEvent};

use crate::error::CliError;
use crate::model_registry;
use crate::permissions::{ParseToolInput, contains_sensitive_marker};

pub(crate) struct TuiMetricsPublisher {
    bridge: BridgeTx<TuiRuntimeEvent>,
    cancellation: BridgeCancel,
    model_id: String,
    turn_started_at: Option<std::time::Instant>,
    tools: BTreeMap<String, (String, std::time::Instant)>,
}

impl TuiMetricsPublisher {
    pub(crate) fn new(
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
        }
    }

    pub(crate) fn observe(&mut self, event: &TurnEvent) {
        let now = std::time::Instant::now();
        let completed_tool = match event {
            TurnEvent::ToolResult(MessagePart::ToolResult { tool_call_id, .. }) => {
                self.tools.remove(tool_call_id)
            }
            _ => None,
        };
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
                    usage.context_window = model_registry::context_window_for(&self.model_id);
                }
                Some(TuiRuntimeEvent::Usage(usage))
            }
            TurnEvent::ToolCallRequested { id, name, input } => {
                self.tools.insert(id.clone(), (name.clone(), now));
                Some(TuiRuntimeEvent::ToolStarted {
                    call_id: id.clone(),
                    name: name.clone(),
                    input: sanitize_tui_metric(input),
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
        };

        if let Some(event) = metric {
            let _ = self.bridge.publish(event, &self.cancellation, None);
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
            let lines = parse_edit_diff(&sanitize_tui_metric(content));
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

    pub(crate) fn finish(&mut self, result: Result<(), &CliError>) {
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

pub(crate) fn finish_tui_metrics<T>(
    metrics: &Arc<Mutex<TuiMetricsPublisher>>,
    result: &Result<T, CliError>,
) {
    if let Ok(mut metrics) = metrics.lock() {
        metrics.finish(result.as_ref().map(|_| ()));
    }
}

pub(crate) fn sanitize_tui_metric(value: &str) -> String {
    if contains_sensitive_marker(value) {
        "[redacted]".to_owned()
    } else {
        value.to_owned()
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

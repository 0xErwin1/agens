use std::fs;
use std::path::Path;

use agens_providers::DiagnosticRef;
use agens_tools::{McpEndpointSummary, McpStatusSnapshot};
use agens_tui::{DialogEntry, DialogView};

use agens_diagnostics::DIAGNOSTIC_FILE_LIMIT_BYTES;

const MCP_STATUS_REFRESH_ROUTE: &str = "mcp:reload";

pub fn mcp_status_dialog(snapshot: McpStatusSnapshot) -> DialogView {
    let entries = snapshot
        .servers()
        .iter()
        .map(|server| {
            let descriptor = server.descriptor();
            let transport = format!("{:?}", descriptor.transport()).to_lowercase();
            let state = format!("{:?}", server.state()).to_lowercase();
            let enabled = if descriptor.enabled() { "enabled" } else { "disabled" };
            let source = format!("{:?}", descriptor.source()).to_lowercase();
            let tools = server.tool_names().join(", ");
            let endpoint = descriptor.endpoint().map_or("not configured", McpEndpointSummary::as_str);
            let error = server
                .last_error()
                .map_or_else(|| "none".into(), |error| error.message().to_owned());
            DialogEntry::read_only(
                format!("{}  {transport}  {enabled}/{state}  {} tools", descriptor.name(), server.tool_count()),
                format!("{} {transport} {state} {tools}", descriptor.name()),
                format!(
                    "Source: {source}\nEndpoint: {endpoint}\nTool call timeout: {}ms\nTools: {}\nLast error: {error}",
                    descriptor.timeout().as_millis(),
                    if tools.is_empty() { "none" } else { &tools },
                ),
            )
        })
        .collect();
    DialogView::read_only(
        "MCP servers",
        None::<&str>,
        entries,
        MCP_STATUS_REFRESH_ROUTE,
    )
    .with_empty_message("No MCP servers configured.")
}

pub fn diagnostics_dialog(data_directory: &Path) -> DialogView {
    let directory = data_directory.join("diagnostics");
    let safe_directory =
        fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.file_type().is_dir());
    let mut files = match safe_directory.then(|| fs::read_dir(&directory)) {
        Some(Ok(entries)) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                is_diagnostic_file_name(&name).then_some((name, entry.path()))
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut entries = Vec::new();
    for (name, path) in files {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() || metadata.len() > DIAGNOSTIC_FILE_LIMIT_BYTES {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let relative_path = format!("diagnostics/{name}");
        entries.extend(content.lines().filter_map(|line| {
            let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
            safe_diagnostic_entry(&value, &relative_path)
        }));
    }

    DialogView::read_only(
        "Runtime diagnostics",
        Some("Sanitized local events"),
        entries,
        "diagnostics",
    )
    .with_empty_message("No runtime diagnostics are available.")
}

fn is_diagnostic_file_name(name: &str) -> bool {
    let Some(identifier) = name
        .strip_prefix("agens-")
        .and_then(|name| name.strip_suffix(".jsonl"))
    else {
        return false;
    };
    let mut parts = identifier.split('.');
    let Some(process) = parts.next() else {
        return false;
    };
    if process.is_empty() || !process.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match (parts.next(), parts.next()) {
        (None, None) => true,
        (Some(generation), None) => matches!(generation, "1" | "2" | "3"),
        _ => false,
    }
}

fn safe_diagnostic_entry(value: &serde_json::Value, relative_path: &str) -> Option<DialogEntry> {
    let object = value.as_object()?;
    let timestamp = object.get("timestamp_ms")?.as_u64()?;
    let reference = object.get("reference")?.as_str()?;
    DiagnosticRef::new(reference.to_owned()).ok()?;
    let scope =
        allowlisted_diagnostic_value(object.get("scope")?.as_str()?, &["parent", "subagent"])?;
    let component = allowlisted_diagnostic_value(
        object.get("component")?.as_str()?,
        &[
            "responses",
            "chat_completions",
            "oauth_refresh",
            "subagent",
            "agent",
            "session",
            "mcp",
        ],
    )?;
    let event = allowlisted_diagnostic_value(
        object.get("event")?.as_str()?,
        &[
            "attempt",
            "retry_scheduled",
            "terminal",
            "agent_unavailable",
            "agent_fallback",
            "continuation_limit_exceeded",
            "provider_state_invalid",
            "replay_item_rejected",
            "replay_limit_exceeded",
            "tool_output_correlation_rejected",
            "turn_started",
            "turn_ended",
            "tool_failed",
            "permission_blocked",
            "working_directory_changed",
            "context_exhausted",
            "compaction_started",
            "compaction_ended",
        ],
    )?;
    let attempt = object
        .get("attempt")?
        .as_u64()
        .filter(|attempt| *attempt <= u64::from(u8::MAX))?;
    let max_attempts = object
        .get("max_attempts")?
        .as_u64()
        .filter(|attempts| *attempts <= u64::from(u8::MAX))?;
    let delay = optional_bounded_u64(object.get("delay_ms"), 5_000)?;
    let status = optional_bounded_u64(object.get("status"), 599)?;
    let class = match object.get("class") {
        Some(serde_json::Value::String(class)) => Some(allowlisted_diagnostic_value(
            class,
            &[
                "authentication",
                "cancelled",
                "context",
                "deadline",
                "model_unavailable",
                "network",
                "permission",
                "provider",
                "protocol",
                "rate_limited",
                "rejected",
                "replay_budget",
                "runtime",
                "server",
                "store",
                "tool",
            ],
        )?),
        Some(serde_json::Value::Null) | None => None,
        Some(_) => return None,
    };
    let input_class = match object.get("input_class") {
        Some(serde_json::Value::String(input_class)) => Some(allowlisted_diagnostic_value(
            input_class,
            &[
                "authentication",
                "cancelled",
                "context",
                "deadline",
                "model_unavailable",
                "network",
                "permission",
                "provider",
                "protocol",
                "rate_limited",
                "rejected",
                "replay_budget",
                "runtime",
                "server",
                "store",
                "tool",
            ],
        )?),
        Some(serde_json::Value::Null) | None => None,
        Some(_) => return None,
    };
    let class_label = match (input_class, class) {
        (Some(input), Some(output)) if input != output => format!("{input}→{output}"),
        _ => class.unwrap_or("success").to_owned(),
    };
    let status_label = status.map_or_else(|| "none".into(), |status| status.to_string());
    let delay_label = delay.map_or_else(|| "none".into(), |delay| format!("{delay}ms"));
    let attempt_label = if attempt == 0 && max_attempts == 0 {
        "not applicable".into()
    } else if max_attempts == 0 {
        format!("{attempt}/unbounded")
    } else {
        format!("{attempt}/{max_attempts}")
    };
    let label = format!("[ref: {reference}] {scope} · {component} · {event} · {class_label}");
    let detail = format!(
        "Source: {relative_path}\nTimestamp: {timestamp}\nAttempt: {attempt_label}\nHTTP status: {status_label}\nRetry delay: {delay_label}"
    );
    Some(DialogEntry::read_only(
        label.clone(),
        format!("{reference} {scope} {component} {event} {class_label}"),
        detail,
    ))
}

fn allowlisted_diagnostic_value<'a>(value: &'a str, allowed: &[&str]) -> Option<&'a str> {
    allowed.contains(&value).then_some(value)
}

fn optional_bounded_u64(value: Option<&serde_json::Value>, maximum: u64) -> Option<Option<u64>> {
    match value {
        Some(serde_json::Value::Number(number)) => {
            Some(Some(number.as_u64().filter(|value| *value <= maximum)?))
        }
        Some(serde_json::Value::Null) | None => Some(None),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use agens_config::McpTransport;
    use agens_providers::ProviderDiagnosticComponent;
    use agens_tools::{
        CommandCatalog, McpRegistry, McpServerDescriptor, McpServerSource, McpServerTransport,
        SkillCatalog,
    };
    use agens_tui::Tui;

    use super::*;
    use crate::engine::ProductionTuiEngine;
    use crate::router::TuiRuntimeRouter;
    use crate::test_support::{
        render_tui_test_backend, tui_session_bootstrap, tui_session_directory,
    };
    use agens_session::context::SessionContext;

    #[test]
    fn tui_mcp_overlay_reports_shared_state_reconnects_on_refresh_and_hides_secrets() {
        let temporary = tui_session_directory("mcp-overlay");
        let mut bootstrap = tui_session_bootstrap(&temporary, &[]);
        bootstrap.mcp_servers = vec![
            agens_config::McpServerConfig {
                name: "files".into(),
                disabled: false,
                transport: McpTransport::Stdio,
                command: Some("/private/bin/files-server".into()),
                args: vec!["SENTINEL_ARG_SECRET".into()],
                environment: BTreeMap::from([("TOKEN".into(), "SENTINEL_ENV_SECRET".into())]),
                cwd: None,
                url: None,
                headers: BTreeMap::new(),
                max_retries: 0,
                timeout_ms: 250,
            },
            agens_config::McpServerConfig {
                name: "disabled".into(),
                disabled: true,
                transport: McpTransport::Sse,
                command: None,
                args: Vec::new(),
                environment: BTreeMap::new(),
                cwd: None,
                url: Some("https://user:SENTINEL_URL_SECRET@example.test/mcp?token=secret".into()),
                headers: BTreeMap::from([(
                    "Authorization".into(),
                    "SENTINEL_HEADER_SECRET".into(),
                )]),
                max_retries: 0,
                timeout_ms: 500,
            },
        ];
        let session = Arc::new(Mutex::new(SessionContext::fresh()));
        let router = TuiRuntimeRouter::new(
            bootstrap,
            Arc::clone(&session),
            Arc::new(Mutex::new(None)),
            Arc::new(CommandCatalog::default()),
            Arc::new(SkillCatalog::default()),
        );
        let mut tui = Tui::new(ProductionTuiEngine {
            cancellation: Arc::new(Mutex::new(None)),
        });

        assert!(
            tui.apply_submission_outcome(router.route("/mcp".into()))
                .is_none()
        );
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Char('/')));
        for character in "idle".chars() {
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char(character)));
        }
        let filtered = render_tui_test_backend(&tui, 90, 24);
        assert!(filtered.contains("files") && !filtered.contains("disabled  sse"));
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Escape));
        tui.apply_submission_outcome(router.open_dialog("mcp").unwrap());
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Down));
        tui.handle(agens_tui::Event::Key(agens_tui::Key::Enter));
        let text = render_tui_test_backend(&tui, 90, 24);
        assert!(text.contains("stdio"), "{text:?}");
        assert!(text.contains("enabled/idle"), "{text:?}");
        assert!(text.contains("disabled"), "{text:?}");
        assert!(text.contains("Source: global"), "{text:?}");
        assert!(text.contains("files-server"), "{text:?}");
        assert!(text.contains("250ms"), "{text:?}");
        for secret in [
            "SENTINEL_ARG_SECRET",
            "SENTINEL_ENV_SECRET",
            "SENTINEL_URL_SECRET",
            "SENTINEL_HEADER_SECRET",
        ] {
            assert!(!text.contains(secret), "{secret}: {text:?}");
        }

        let mut live = McpRegistry::with_status_handle(router.mcp_status.clone());
        live.register_disabled_server(McpServerDescriptor::new(
            "later",
            McpServerSource::Global,
            McpServerTransport::Stdio,
            false,
            std::time::Duration::from_secs(10),
            None,
        ))
        .unwrap();
        let agens_tui::Action::OpenDialog(route_id) =
            tui.handle(agens_tui::Event::Key(agens_tui::Key::Char('r')))
        else {
            panic!("pressing r on a refreshable dialog must reopen its route");
        };
        assert_eq!(route_id, "mcp:reload");
        let refreshed = router.open_dialog(&route_id).unwrap();
        assert!(
            matches!(refreshed, agens_tui::TuiSubmissionOutcome::SafeDialog(_)),
            "a real reconnect must not disturb the `running` flag of an in-flight turn"
        );
        tui.apply_submission_outcome(refreshed);
        let text = render_tui_test_backend(&tui, 90, 24);
        assert!(text.contains("later"), "{text:?}");
        assert!(text.contains("enabled/failed"), "{text:?}");
        for secret in [
            "SENTINEL_ARG_SECRET",
            "SENTINEL_ENV_SECRET",
            "SENTINEL_URL_SECRET",
            "SENTINEL_HEADER_SECRET",
        ] {
            assert!(!text.contains(secret), "{secret}: {text:?}");
        }
        assert!(session.lock().unwrap().messages.is_empty());
        assert!(tui.transcript().is_empty());
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn mcp_status_dialog_renders_the_closed_reason_label_exactly_once() {
        let status = agens_tools::McpStatusHandle::default();
        let mut registry = McpRegistry::with_status_handle(status.clone());
        registry
            .register_failed_server(
                McpServerDescriptor::new(
                    "flaky",
                    McpServerSource::Global,
                    McpServerTransport::Http,
                    true,
                    std::time::Duration::from_secs(5),
                    None,
                ),
                agens_tools::McpErrorCategory::Transport,
                "transport: http status 406",
            )
            .unwrap();

        let dialog = mcp_status_dialog(status.snapshot());
        let rendered = format!("{dialog:?}");

        assert!(
            rendered.contains("Last error: transport: http status 406"),
            "{rendered:?}"
        );
        assert!(!rendered.contains("transport: transport:"), "{rendered:?}");
    }

    #[test]
    fn safe_diagnostic_entry_allowlists_every_provider_diagnostic_component() {
        fn entry_for_component(component: &str) -> Option<DialogEntry> {
            let value = serde_json::json!({
                "timestamp_ms": 1,
                "reference": "abc12345",
                "scope": "parent",
                "component": component,
                "event": "terminal",
                "attempt": 1,
                "max_attempts": 3,
                "delay_ms": null,
                "status": 429,
                "class": "rate_limited",
            });
            safe_diagnostic_entry(&value, "diagnostics/agens-1.jsonl")
        }

        // Exhaustive match: adding a `ProviderDiagnosticComponent` variant makes this fail to
        // compile until it is handled here too, so the allowlist below cannot silently drift
        // from the real enum the way the previous string-literal list did.
        fn component_as_str(component: ProviderDiagnosticComponent) -> &'static str {
            match component {
                ProviderDiagnosticComponent::Responses => component.as_str(),
                ProviderDiagnosticComponent::ChatCompletions => component.as_str(),
                ProviderDiagnosticComponent::OauthRefresh => component.as_str(),
                ProviderDiagnosticComponent::Subagent => component.as_str(),
                ProviderDiagnosticComponent::Agent => component.as_str(),
                ProviderDiagnosticComponent::Session => component.as_str(),
                ProviderDiagnosticComponent::Mcp => component.as_str(),
            }
        }

        for component in [
            ProviderDiagnosticComponent::Responses,
            ProviderDiagnosticComponent::ChatCompletions,
            ProviderDiagnosticComponent::OauthRefresh,
            ProviderDiagnosticComponent::Subagent,
            ProviderDiagnosticComponent::Agent,
            ProviderDiagnosticComponent::Session,
            ProviderDiagnosticComponent::Mcp,
        ] {
            let component_str = component_as_str(component);
            assert!(
                entry_for_component(component_str).is_some(),
                "component {component_str} should yield a diagnostic entry"
            );
        }

        assert!(entry_for_component("unknown_component").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_dialog_projects_only_safe_fields_and_relative_paths() {
        use std::os::unix::fs::symlink;

        let data_directory = std::env::temp_dir().join(format!(
            "agens-diagnostics-dialog-{}-{}",
            std::process::id(),
            agens_diagnostics::DIAGNOSTIC_REFERENCE_SEQUENCE
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let diagnostics_directory = data_directory.join("diagnostics");
        std::fs::create_dir_all(&diagnostics_directory)
            .expect("diagnostics directory should be created");
        std::fs::write(
            diagnostics_directory.join("agens-42.jsonl"),
            concat!(
                "{\"timestamp_ms\":1,\"reference\":\"abc12345\",\"scope\":\"parent\",",
                "\"component\":\"responses\",\"event\":\"terminal\",\"attempt\":3,",
                "\"max_attempts\":3,\"delay_ms\":null,\"status\":429,",
                "\"class\":\"rate_limited\",\"unknown\":\"SENTINEL_SECRET\"}\n",
                "{\"timestamp_ms\":2,\"reference\":\"def67890\",\"scope\":\"parent\",",
                "\"component\":\"responses\",\"event\":\"replay_item_rejected\",\"attempt\":0,",
                "\"max_attempts\":0,\"delay_ms\":null,\"status\":null,",
                "\"class\":\"protocol\"}\n",
                "{\"timestamp_ms\":3,\"reference\":\"aaa00001\",\"scope\":\"parent\",",
                "\"component\":\"responses\",\"event\":\"continuation_limit_exceeded\",\"attempt\":0,",
                "\"max_attempts\":0,\"delay_ms\":null,\"status\":null,\"class\":\"context\"}\n",
                "{\"timestamp_ms\":4,\"reference\":\"aaa00002\",\"scope\":\"parent\",",
                "\"component\":\"responses\",\"event\":\"provider_state_invalid\",\"attempt\":0,",
                "\"max_attempts\":0,\"delay_ms\":null,\"status\":null,\"class\":\"protocol\"}\n",
                "{\"timestamp_ms\":5,\"reference\":\"aaa00003\",\"scope\":\"parent\",",
                "\"component\":\"responses\",\"event\":\"replay_limit_exceeded\",\"attempt\":0,",
                "\"max_attempts\":0,\"delay_ms\":null,\"status\":null,\"class\":\"replay_budget\"}\n",
                "{\"timestamp_ms\":6,\"reference\":\"aaa00004\",\"scope\":\"parent\",",
                "\"component\":\"responses\",\"event\":\"tool_output_correlation_rejected\",\"attempt\":0,",
                "\"max_attempts\":0,\"delay_ms\":null,\"status\":null,\"class\":\"protocol\"}\n",
                "{\"timestamp_ms\":7,\"reference\":\"aaa00005\",\"scope\":\"subagent\",",
                "\"component\":\"subagent\",\"event\":\"terminal\",\"attempt\":0,",
                "\"max_attempts\":0,\"delay_ms\":null,\"status\":null,\"class\":\"runtime\",",
                "\"input_class\":\"permission\"}\n"
            ),
        )
        .expect("diagnostic fixture should be written");
        let outside = data_directory.join("outside.txt");
        std::fs::write(&outside, "SENTINEL_OUTSIDE").expect("outside fixture should be written");
        symlink(&outside, diagnostics_directory.join("agens-99.jsonl"))
            .expect("diagnostic symlink should be created");

        let rendered = format!("{:?}", diagnostics_dialog(&data_directory));

        assert!(rendered.contains("abc12345"));
        assert!(rendered.contains("diagnostics/agens-42.jsonl"));
        for event in [
            "continuation_limit_exceeded",
            "provider_state_invalid",
            "replay_item_rejected",
            "replay_limit_exceeded",
            "tool_output_correlation_rejected",
        ] {
            assert!(
                rendered.contains(event),
                "missing diagnostic event: {event}"
            );
        }
        assert!(rendered.contains("permission→runtime"));
        assert!(rendered.contains("Attempt: not applicable"));
        assert!(!rendered.contains(&data_directory.display().to_string()));
        assert!(!rendered.contains("SENTINEL_SECRET"));
        assert!(!rendered.contains("SENTINEL_OUTSIDE"));

        std::fs::remove_dir_all(data_directory).expect("test directory should be removed");
    }
}

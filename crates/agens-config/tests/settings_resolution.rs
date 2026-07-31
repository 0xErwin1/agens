use agens_config::{
    ConfiguredValue, McpDefaultSettings, Origin, SETTINGS, SettingValue, SubagentSettings,
    ToolLimitSettings, parse_toml_document, resolve_settings,
};

fn table(input: &str) -> toml::Table {
    parse_toml_document(input).expect("fixture must be syntactically valid TOML")
}

fn empty() -> toml::Table {
    toml::Table::new()
}

#[test]
fn every_setting_falls_back_to_its_catalog_default() {
    let resolved = resolve_settings(&empty(), &empty(), &empty());

    for spec in SETTINGS {
        let expected = match spec.default {
            SettingValue::Bool(value) => ConfiguredValue::Bool(value),
            SettingValue::Integer(value) => ConfiguredValue::Integer(value),
            SettingValue::Text(value) => ConfiguredValue::Text(value.to_owned()),
            SettingValue::Absent => ConfiguredValue::Absent,
        };

        assert_eq!(resolved.origin(spec.path), Origin::Default, "{}", spec.path);
        assert_eq!(resolved.value(spec.path), Some(&expected), "{}", spec.path);
    }
}

#[test]
fn a_global_value_wins_over_the_default() {
    let global = table("[tools]\nmax_search_depth = 8\n");

    let resolved = resolve_settings(&global, &empty(), &global);

    assert_eq!(resolved.origin("tools.max_search_depth"), Origin::Global);
    assert_eq!(resolved.integer("tools.max_search_depth"), Some(8));
}

#[test]
fn a_project_value_wins_over_a_global_value() {
    let global = table("[tools]\nmax_search_depth = 8\n");
    let project = table("[tools]\nmax_search_depth = 4\n");
    let merged = table("[tools]\nmax_search_depth = 4\n");

    let resolved = resolve_settings(&global, &project, &merged);

    assert_eq!(resolved.origin("tools.max_search_depth"), Origin::Project);
    assert_eq!(resolved.integer("tools.max_search_depth"), Some(4));
}

#[test]
fn a_value_changed_by_expansion_is_attributed_to_the_environment() {
    let global = table("[provider]\nmodel = \"$AGENS_MODEL\"\n");
    let expanded = table("[provider]\nmodel = \"gpt-5.5\"\n");

    let resolved = resolve_settings(&global, &empty(), &expanded);

    assert_eq!(resolved.origin("provider.model"), Origin::Environment);
    assert_eq!(resolved.text("provider.model"), Some("gpt-5.5"));
}

#[test]
fn an_unexpanded_value_keeps_its_document_origin() {
    let global = table("[provider]\nmodel = \"gpt-5.5\"\n");

    let resolved = resolve_settings(&global, &empty(), &global);

    assert_eq!(resolved.origin("provider.model"), Origin::Global);
}

#[test]
fn typed_views_match_the_limits_the_runtime_hardcodes_today() {
    let resolved = resolve_settings(&empty(), &empty(), &empty());

    let tools = ToolLimitSettings::from(&resolved);
    assert_eq!(tools.max_list_entries, 1_000);
    assert_eq!(tools.max_search_entries, 10_000);
    assert_eq!(tools.max_search_results, 100);
    assert_eq!(tools.max_search_depth, 32);
    assert_eq!(tools.operation_timeout_ms, 5_000);
    assert_eq!(tools.bash_timeout_ms, 120_000);

    let subagents = SubagentSettings::from(&resolved);
    assert_eq!(subagents.max_iterations, 32);
    assert_eq!(subagents.max_concurrency, 4);
    assert_eq!(subagents.max_output_chars, 65_536);

    let mcp = McpDefaultSettings::from(&resolved);
    assert_eq!(mcp.timeout_ms, 10_000);
    assert_eq!(mcp.max_retries, 0);
}

#[test]
fn typed_views_carry_configured_values() {
    let document = table(
        "[tools]\nmax_search_depth = 4\nbash_timeout_ms = 30000\n\n[subagents]\nmax_concurrency = 2\n\n[mcp_defaults]\ntimeout_ms = 250\nmax_retries = 3\n",
    );

    let resolved = resolve_settings(&document, &empty(), &document);

    assert_eq!(ToolLimitSettings::from(&resolved).max_search_depth, 4);
    assert_eq!(ToolLimitSettings::from(&resolved).bash_timeout_ms, 30_000);
    assert_eq!(SubagentSettings::from(&resolved).max_concurrency, 2);
    assert_eq!(McpDefaultSettings::from(&resolved).timeout_ms, 250);
    assert_eq!(McpDefaultSettings::from(&resolved).max_retries, 3);
}

#[test]
fn text_settings_without_a_default_resolve_to_absent() {
    let resolved = resolve_settings(&empty(), &empty(), &empty());

    assert_eq!(resolved.text("agent.default_agent"), None);
    assert_eq!(resolved.text("agent.reasoning_effort"), None);
    assert_eq!(resolved.integer("agent.max_iterations"), None);
}

#[test]
fn reports_every_catalog_key_exactly_once() {
    let resolved = resolve_settings(&empty(), &empty(), &empty());

    let reported: Vec<&str> = resolved.iter().map(|(path, _)| path).collect();
    let expected: Vec<&str> = SETTINGS.iter().map(|spec| spec.path).collect();

    assert_eq!(reported, expected);
}

use agens_config::{
    McpDefaultSettings, SETTINGS, SettingKind, SettingSpec, SettingValue, mcp_servers,
    mcp_servers_with_defaults,
};
use agens_config::{parse_toml_document, validate_toml_document};

fn document_for(path: &str, literal: &str) -> toml::Table {
    let (table, key) = path
        .split_once('.')
        .expect("every setting path is qualified by its table");
    parse_toml_document(&format!("[{table}]\n{key} = {literal}\n"))
        .expect("generated fixture must be syntactically valid TOML")
}

fn accepts(path: &str, literal: &str) -> bool {
    validate_toml_document(&document_for(path, literal)).is_ok()
}

fn sample_literal(spec: &SettingSpec) -> String {
    match spec.kind {
        SettingKind::Bool => "true".to_owned(),
        SettingKind::Integer { minimum, .. } => minimum.to_string(),
        SettingKind::Text { .. } => "\"sample\"".to_owned(),
        SettingKind::Choice(choices) => format!("\"{}\"", choices[0]),
    }
}

#[test]
fn accepts_every_setting_at_its_documented_default() {
    for spec in SETTINGS {
        let literal = match spec.default {
            SettingValue::Bool(value) => value.to_string(),
            SettingValue::Integer(value) => value.to_string(),
            SettingValue::Text(value) => format!("\"{value}\""),
            SettingValue::Absent => sample_literal(spec),
        };

        assert!(
            accepts(spec.path, &literal),
            "{} must accept its documented default {literal}",
            spec.path
        );
    }
}

#[test]
fn rejects_integers_outside_their_documented_range() {
    for spec in SETTINGS {
        let SettingKind::Integer { minimum, maximum } = spec.kind else {
            continue;
        };

        if let Some(below) = minimum.checked_sub(1) {
            assert!(
                !accepts(spec.path, &below.to_string()),
                "{} must reject {below}, one below its minimum",
                spec.path
            );
        }
        if let Some(above) = maximum.checked_add(1) {
            assert!(
                !accepts(spec.path, &above.to_string()),
                "{} must reject {above}, one above its maximum",
                spec.path
            );
        }
    }
}

#[test]
fn rejects_every_setting_given_the_wrong_type() {
    for spec in SETTINGS {
        let wrong = match spec.kind {
            SettingKind::Bool => "1",
            SettingKind::Integer { .. } => "true",
            SettingKind::Text { .. } | SettingKind::Choice(_) => "1",
        };

        assert!(
            !accepts(spec.path, wrong),
            "{} must reject the wrongly typed value {wrong}",
            spec.path
        );
    }
}

#[test]
fn rejects_text_settings_beyond_their_documented_length() {
    for spec in SETTINGS {
        let SettingKind::Text { max_chars } = spec.kind else {
            continue;
        };
        let Some(overlong) = max_chars.checked_add(1) else {
            continue;
        };

        let literal = format!("\"{}\"", "a".repeat(overlong));
        assert!(
            !accepts(spec.path, &literal),
            "{} must reject a value of {overlong} characters",
            spec.path
        );
    }
}

#[test]
fn rejects_a_choice_outside_its_documented_vocabulary() {
    assert!(accepts("agent.reasoning_effort", "\"high\""));
    assert!(!accepts("agent.reasoning_effort", "\"ultra\""));
}

#[test]
fn rejects_unknown_keys_in_catalog_tables() {
    let document = parse_toml_document("[tools]\nmax_search_dept = 8\n").unwrap();

    assert!(validate_toml_document(&document).is_err());
}

#[test]
fn bypass_permission_prompts_is_a_global_bool_defaulting_off() {
    let spec = SETTINGS
        .iter()
        .find(|spec| spec.path == "agent.bypass_permission_prompts")
        .expect("the catalog must declare agent.bypass_permission_prompts");

    assert!(matches!(spec.kind, SettingKind::Bool));
    assert!(matches!(spec.default, SettingValue::Bool(false)));

    let document = document_for("agent.bypass_permission_prompts", "true");
    assert!(validate_toml_document(&document).is_ok());
}

#[test]
fn rejects_an_unknown_top_level_table() {
    let document = parse_toml_document("[toolz]\nmax_search_depth = 8\n").unwrap();

    assert!(validate_toml_document(&document).is_err());
}

#[test]
fn accepts_a_document_that_sets_every_setting_at_once() {
    let mut document = String::new();
    let mut current = "";

    for spec in SETTINGS {
        let (table, key) = spec.path.split_once('.').unwrap();
        if table != current {
            document.push_str(&format!("\n[{table}]\n"));
            current = table;
        }
        let literal = match spec.default {
            SettingValue::Bool(value) => value.to_string(),
            SettingValue::Integer(value) => value.to_string(),
            SettingValue::Text(value) => format!("\"{value}\""),
            SettingValue::Absent => sample_literal(spec),
        };
        document.push_str(&format!("{key} = {literal}\n"));
    }

    let parsed = parse_toml_document(&document).expect("catalog fixture must parse");
    validate_toml_document(&parsed).expect("catalog fixture must validate");
}

#[test]
fn rejects_an_invalid_per_server_mcp_timeout_instead_of_defaulting() {
    let zero = parse_toml_document(
        "[mcp.files]\ntransport = \"stdio\"\ncommand = \"server\"\ntimeout_ms = 0\n",
    )
    .unwrap();
    let text = parse_toml_document(
        "[mcp.files]\ntransport = \"stdio\"\ncommand = \"server\"\ntimeout_ms = \"10s\"\n",
    )
    .unwrap();

    assert!(validate_toml_document(&zero).is_err());
    assert!(validate_toml_document(&text).is_err());
    assert!(mcp_servers(&zero).is_err());
    assert!(mcp_servers(&text).is_err());
}

#[test]
fn explicit_mcp_defaults_must_stay_inside_catalog_bounds() {
    let document =
        parse_toml_document("[mcp.files]\ntransport = \"http\"\nurl = \"https://example.test\"\n")
            .unwrap();

    for defaults in [
        McpDefaultSettings {
            timeout_ms: 0,
            max_retries: 0,
        },
        McpDefaultSettings {
            timeout_ms: 1,
            max_retries: 9,
        },
    ] {
        assert!(mcp_servers_with_defaults(&document, defaults).is_err());
    }
}

#[test]
fn keeps_a_valid_per_server_mcp_timeout() {
    let document = parse_toml_document(
        "[mcp.files]\ntransport = \"stdio\"\ncommand = \"server\"\ntimeout_ms = 50\n",
    )
    .unwrap();

    validate_toml_document(&document).expect("a positive timeout stays valid");
    let servers = mcp_servers(&document).expect("a positive timeout stays parseable");
    assert_eq!(servers[0].timeout_ms, 50);
}

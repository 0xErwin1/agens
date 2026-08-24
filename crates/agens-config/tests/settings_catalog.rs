use agens_config::{
    DEFAULT_MCP_CONNECT_TIMEOUT_MS, McpDefaultSettings, SETTINGS, SettingKind, SettingSpec,
    SettingValue, TextListEntry, mcp_servers, mcp_servers_with_defaults,
};
use agens_config::{
    TeamSettings, expand_home_prefix, parse_toml_document, resolve_settings, validate_toml_document,
};

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
        SettingKind::TextList { entry, .. } => match entry {
            TextListEntry::Any => "[\"sample\"]".to_owned(),
            TextListEntry::RootedPath => "[\"/sample\"]".to_owned(),
        },
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
            SettingKind::TextList { .. } => "\"sample\"",
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
fn unattended_permission_questions_wait_by_default_and_can_restore_immediate_denial() {
    let wait = SETTINGS
        .iter()
        .find(|spec| spec.path == "agent.unattended_permission_wait_ms")
        .expect("the catalog must declare agent.unattended_permission_wait_ms");
    assert!(matches!(
        wait.kind,
        SettingKind::Integer {
            minimum: 1_000,
            maximum: 600_000
        }
    ));
    assert!(matches!(wait.default, SettingValue::Integer(300_000)));

    let legacy = SETTINGS
        .iter()
        .find(|spec| spec.path == "agent.deny_unattended_permission_prompts")
        .expect("the catalog must declare agent.deny_unattended_permission_prompts");
    assert!(matches!(legacy.kind, SettingKind::Bool));
    assert!(matches!(legacy.default, SettingValue::Bool(false)));

    assert!(accepts("agent.unattended_permission_wait_ms", "1_000"));
    assert!(accepts("agent.deny_unattended_permission_prompts", "true"));
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
            connect_timeout_ms: DEFAULT_MCP_CONNECT_TIMEOUT_MS,
            max_retries: 0,
        },
        McpDefaultSettings {
            timeout_ms: 1,
            connect_timeout_ms: DEFAULT_MCP_CONNECT_TIMEOUT_MS,
            max_retries: 9,
        },
        McpDefaultSettings {
            timeout_ms: 1,
            connect_timeout_ms: 0,
            max_retries: 0,
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

/// A retired key is rejected by name with what replaced it, rather than as an
/// anonymous unknown field: an existing configuration has to be told what to
/// write instead, not only that what it has is wrong.
#[test]
fn rejects_a_retired_setting_by_name_with_its_replacement() {
    let document = parse_toml_document("[provider]\ntype = \"openai-api\"\n").unwrap();

    let error = validate_toml_document(&document).expect_err("a retired setting must not validate");
    let message = error.to_string();

    assert!(message.contains("provider.type"), "{message}");
    assert!(message.contains("provider/model"), "{message}");
}

#[test]
fn rejects_a_list_setting_beyond_its_documented_bounds() {
    for spec in SETTINGS {
        let SettingKind::TextList {
            max_items,
            max_chars,
            entry,
        } = spec.kind
        else {
            continue;
        };

        // An entry has to satisfy the list's own shape, or the bound under test
        // is not the reason the document is refused.
        let prefix = match entry {
            TextListEntry::Any => "",
            TextListEntry::RootedPath => "/",
        };

        let too_many = (0..=max_items)
            .map(|index| format!("\"{prefix}{index}\""))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(
            !accepts(spec.path, &format!("[{too_many}]")),
            "{} must reject {} entries",
            spec.path,
            max_items + 1
        );

        let overlong = format!(
            "[\"{prefix}{}\"]",
            "a".repeat(max_chars + 1 - prefix.chars().count())
        );
        assert!(
            !accepts(spec.path, &overlong),
            "{} must reject an entry of {} characters",
            spec.path,
            max_chars + 1
        );
    }
}

#[test]
fn reads_project_roots_and_hook_exports_as_the_lists_the_daemon_serves() {
    let document = parse_toml_document(
        "[team]\nproject_roots = [\"/srv/checkouts\"]\nhook_exports = [\"PATH\"]\n",
    )
    .expect("the fixture is valid TOML");
    validate_toml_document(&document).expect("a list of strings is what both keys hold");

    let resolved = resolve_settings(&toml::Table::new(), &document, &document);
    let team = TeamSettings::from(&resolved);

    assert_eq!(
        team.project_roots,
        vec![std::path::PathBuf::from("/srv/checkouts")]
    );
    assert_eq!(team.hook_exports, vec!["PATH".to_owned()]);
}

/// A root written against the home directory is the spelling an operator
/// reaches for, and the daemon resolves a checkout by canonicalizing the root.
/// A literal `~/dev` canonicalizes to nothing, so it silently serves no
/// repository at all.
#[test]
fn resolves_a_project_root_written_against_the_home_directory() {
    let home = std::path::Path::new("/home/dev");

    assert_eq!(
        expand_home_prefix("~/dev/checkouts", Some(home)),
        std::path::PathBuf::from("/home/dev/dev/checkouts")
    );
    assert_eq!(
        expand_home_prefix("~", Some(home)),
        std::path::PathBuf::from("/home/dev")
    );

    // Another user's home cannot be looked up, and an unknown home cannot be
    // guessed at. Both stay as written and match nothing.
    assert_eq!(
        expand_home_prefix("~someone/dev", Some(home)),
        std::path::PathBuf::from("~someone/dev")
    );
    assert_eq!(
        expand_home_prefix("~/dev", None),
        std::path::PathBuf::from("~/dev")
    );

    assert_eq!(
        expand_home_prefix("/srv/checkouts", Some(home)),
        std::path::PathBuf::from("/srv/checkouts")
    );
}

/// A relative root names a different checkout for every working directory the
/// daemon might have been started from, so it is refused by name rather than
/// resolved into whichever one that happens to be.
#[test]
fn rejects_a_project_root_that_is_neither_absolute_nor_written_against_the_home_directory() {
    for root in ["dev/checkouts", "./checkouts", "../checkouts", ""] {
        let document =
            parse_toml_document(&format!("[team]\nproject_roots = [\"{root}\"]\n")).unwrap();
        let error = validate_toml_document(&document)
            .expect_err("a relative project root must not validate");

        assert!(
            error.to_string().contains("team.project_roots"),
            "the rejection has to name the key holding {root}: {error}"
        );
    }

    for root in ["/srv/checkouts", "~/dev", "~"] {
        let document =
            parse_toml_document(&format!("[team]\nproject_roots = [\"{root}\"]\n")).unwrap();
        validate_toml_document(&document).unwrap_or_else(|error| panic!("{root}: {error}"));
    }
}

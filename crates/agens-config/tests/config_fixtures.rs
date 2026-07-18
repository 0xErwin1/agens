use agens_config::{
    ConfigPermissionDecision, ConfigPermissionScope, McpServerConfig, McpTransport,
    extract_permission_rules, mcp_servers, mcp_stdio_servers, parse_toml_document,
    validate_toml_document,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[test]
fn parses_a_valid_toml_document() {
    let document = parse_toml_document(
        r#"
            provider = "openai"

            [model]
            name = "gpt-5"
        "#,
    )
    .expect("valid configuration fixture should parse");

    assert_eq!(document["provider"].as_str(), Some("openai"));
    assert_eq!(document["model"]["name"].as_str(), Some("gpt-5"));
}

#[test]
fn rejects_a_malformed_toml_document() {
    let error = parse_toml_document("provider = [").expect_err("malformed TOML must fail");

    assert!(error.to_string().contains("invalid array"));
}

#[test]
fn rejects_semantically_invalid_configuration_fields() {
    let wrong_type = parse_toml_document("[provider]\nmodel = 123")
        .expect("syntactically valid TOML should parse");
    let unknown_field = parse_toml_document("[provider]\nunknown = \"value\"")
        .expect("syntactically valid TOML should parse");

    assert!(validate_toml_document(&wrong_type).is_err());
    assert!(validate_toml_document(&unknown_field).is_err());
}

#[test]
fn extracts_only_safe_configured_stdio_mcp_servers() {
    let document = parse_toml_document("[mcp.files]\ntransport = \"stdio\"\ncommand = \"server\"\nargs = [\"--safe\"]\ntimeout_ms = 50\n[mcp.files.env]\nLANG = \"C\"").unwrap();
    let servers = mcp_stdio_servers(&document).unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "files");
    assert_eq!(servers[0].args, ["--safe"]);
    assert_eq!(servers[0].timeout_ms, 50);
    let unsafe_server =
        parse_toml_document("[mcp.bad]\ntransport = \"stdio\"\ncommand = \" \"\ntimeout_ms = 0")
            .unwrap();
    assert!(mcp_stdio_servers(&unsafe_server).is_err());
}

#[test]
fn parses_legacy_mcp_transport_shapes_with_default_timeout() {
    let document = parse_toml_document(
        r#"
            [mcp.files]
            transport = "stdio"
            command = "server"
            args = ["--safe"]
            cwd = "/workspace"
            env = { LANG = "C" }

            [mcp.docs]
            transport = "http"
            url = "https://mcp.example.test/mcp"
            headers = { Authorization = "Bearer token" }
            max_retries = 2

            [mcp.events]
            transport = "sse"
            url = "https://mcp.example.test/sse"
        "#,
    )
    .expect("legacy MCP fixture should parse");

    let servers = mcp_servers(&document).expect("legacy MCP shapes should extract");

    assert_eq!(servers.len(), 3);
    for server in &servers {
        assert_eq!(server.timeout_ms, 10_000);
    }
    assert_eq!(
        servers
            .iter()
            .map(|server| (server.name.as_str(), server.transport))
            .collect::<Vec<_>>(),
        [
            ("docs", McpTransport::Http),
            ("events", McpTransport::Sse),
            ("files", McpTransport::Stdio),
        ]
    );
}

#[test]
fn mcp_server_config_debug_redacts_environment_values_and_arguments() {
    let config = McpServerConfig {
        name: "files".into(),
        transport: McpTransport::Stdio,
        command: Some(PathBuf::from("server")),
        args: vec!["--token".into(), "SENTINEL_ARGUMENT_SECRET".into()],
        environment: BTreeMap::from([("API_TOKEN".into(), "SENTINEL_ENV_SECRET".into())]),
        cwd: None,
        url: None,
        headers: BTreeMap::new(),
        max_retries: 0,
        timeout_ms: 50,
    };

    let debug = format!("{config:?}");

    assert!(debug.contains("API_TOKEN"));
    assert!(debug.contains("args_count: 2"));
    assert!(debug.contains("environment_count: 1"));
    assert!(!debug.contains("SENTINEL_ARGUMENT_SECRET"));
    assert!(!debug.contains("SENTINEL_ENV_SECRET"));
}

#[test]
fn extracts_ordered_global_and_project_permission_rules() {
    let global = parse_toml_document(
        r#"
            [permissions]
            allow = ["read", "bash(git status *)"]
            deny = ["bash(rm -rf *)"]
        "#,
    )
    .expect("global fixture should parse");
    let project = parse_toml_document(
        r#"
            [permissions]
            allow = ["engram_mem_save(path/*.txt)"]
            deny = ["write(secrets/*)"]
        "#,
    )
    .expect("project fixture should parse");

    let rules = extract_permission_rules(&global, &project)
        .expect("supported global and project rules should extract");

    assert_eq!(rules.len(), 5);
    assert_eq!(rules[0].scope, ConfigPermissionScope::Global);
    assert_eq!(rules[0].decision, ConfigPermissionDecision::Allow);
    assert_eq!(rules[0].tool_pattern, "read");
    assert_eq!(rules[0].target_pattern, None);
    assert_eq!(rules[1].tool_pattern, "bash");
    assert_eq!(rules[1].target_pattern.as_deref(), Some("git status *"));
    assert_eq!(rules[2].decision, ConfigPermissionDecision::Deny);
    assert_eq!(rules[3].scope, ConfigPermissionScope::Project);
    assert_eq!(rules[3].tool_pattern, "engram_mem_save");
    assert_eq!(rules[3].target_pattern.as_deref(), Some("path/*.txt"));
    assert_eq!(rules[4].decision, ConfigPermissionDecision::Deny);
}

#[test]
fn permission_rule_extraction_rejects_invalid_or_unsafe_entries_without_echoing_values() {
    let cases = [
        ("empty", "", "permissions.allow[0]"),
        ("whitespace", "   ", "permissions.allow[0]"),
        (
            "malformed separator",
            "bash(one)(two)",
            "permissions.allow[0]",
        ),
        (
            "missing closing separator",
            "bash(one",
            "permissions.allow[0]",
        ),
        ("unsafe tool traversal", "../read", "permissions.allow[0]"),
        (
            "unsafe target traversal",
            "read(../secret)",
            "permissions.allow[0]",
        ),
        ("ambiguous tool wildcard", "read*", "permissions.allow[0]"),
    ];

    for (name, rule, field) in cases {
        let document = parse_toml_document(&format!("[permissions]\nallow = [{rule:?}]"))
            .expect("fixture should be syntactically valid TOML");

        let error = extract_permission_rules(&document, &toml::Table::new()).expect_err(name);

        assert_eq!(
            error.to_string(),
            format!("invalid configuration field {field}")
        );
        if !rule.is_empty() {
            assert!(!error.to_string().contains(rule));
        }
    }

    let sentinel = "SENTINEL_PERMISSION_SECRET";
    let document = parse_toml_document(&format!(
        "[permissions]\nallow = [\"read({sentinel})(second)\"]"
    ))
    .expect("secret sentinel fixture should be syntactically valid TOML");
    let error = extract_permission_rules(&document, &toml::Table::new())
        .expect_err("malformed secret-bearing rule must fail");

    assert_eq!(
        error.to_string(),
        "invalid configuration field permissions.allow[0]"
    );
    assert!(!error.to_string().contains(sentinel));
}

#[test]
fn permission_rule_extraction_rejects_conflicts_and_semantically_invalid_documents() {
    let conflicting = parse_toml_document(
        r#"
            [permissions]
            allow = ["read(path/*.txt)"]
            deny = ["read(path/*.txt)"]
        "#,
    )
    .expect("conflict fixture should parse");
    let wrong_type = parse_toml_document("[permissions]\nallow = [42]")
        .expect("wrong type fixture should parse");
    let unknown = parse_toml_document("[permissions]\nunknown = [\"read\"]")
        .expect("unknown field fixture should parse");

    assert_eq!(
        extract_permission_rules(&conflicting, &toml::Table::new())
            .expect_err("conflicting global rules must fail")
            .to_string(),
        "invalid configuration field permissions.deny[0]"
    );
    assert_eq!(
        extract_permission_rules(&wrong_type, &toml::Table::new())
            .expect_err("non-string permission entry must fail")
            .to_string(),
        "invalid configuration field permissions.allow"
    );
    assert_eq!(
        extract_permission_rules(&unknown, &toml::Table::new())
            .expect_err("unknown permission field must still fail")
            .to_string(),
        "invalid configuration field permissions.unknown"
    );
}

#[test]
fn permission_rule_extraction_accepts_documented_tool_grammar_and_safe_unicode_targets() {
    let document = parse_toml_document(
        r#"
            [permissions]
            allow = ["read(資料/**/*.txt)", "read(file[0-9].txt)", "list(directory)", "search(directory/**)", "engram_mem_save(**)", "bash(git status *)"]
        "#,
    )
    .expect("fixture should parse");

    let rules = extract_permission_rules(&document, &toml::Table::new())
        .expect("documented rules with safe Unicode targets should extract");

    assert_eq!(rules.len(), 6);
    assert_eq!(rules[0].target_pattern.as_deref(), Some("資料/**/*.txt"));
    assert_eq!(rules[2].tool_pattern, "list");
    assert_eq!(rules[3].tool_pattern, "search");
}

#[test]
fn permission_rule_extraction_rejects_ungrounded_tools_and_invalid_target_globs() {
    let cases = [
        ("mcp separator alias", "mcp::files::read(**)"),
        ("uppercase alias", "Bash(**)"),
        ("unicode confusable", "rеad(**)"),
        ("extra namespace separator", "engram__mem_save(**)"),
        ("unclosed class", "read([)"),
        ("unmatched class close", "read(file])"),
        ("trailing escape", "read(file\\)"),
    ];

    for (name, rule) in cases {
        let document = parse_toml_document(&format!("[permissions]\nallow = [{rule:?}]"))
            .expect("fixture should be syntactically valid TOML");

        let error = extract_permission_rules(&document, &toml::Table::new()).expect_err(name);

        assert_eq!(
            error.to_string(),
            "invalid configuration field permissions.allow[0]"
        );
        assert!(!error.to_string().contains(rule));
    }
}

#[test]
fn permission_rule_extraction_rejects_unicode_separators_and_overlapping_rules() {
    let unsafe_targets = [
        "..∕secret",
        "..／secret",
        "..⁄secret",
        "..＼secret",
        "..⧵secret",
        "..⧶secret",
        "..⧷secret",
        "..⧸secret",
        "..⧹secret",
        "..⹊secret",
        "..⼃secret",
        "..﹨secret",
        "..🙼secret",
        "..🙽secret",
    ];

    for target in unsafe_targets {
        let document = parse_toml_document(&format!("[permissions]\nallow = [\"read({target})\"]"))
            .expect("fixture should be syntactically valid TOML");

        assert_eq!(
            extract_permission_rules(&document, &toml::Table::new())
                .expect_err(target)
                .to_string(),
            "invalid configuration field permissions.allow[0]"
        );
    }

    let conflicts = [
        (
            "same-decision duplicate",
            "allow = [\"read(secret)\", \"read(secret)\"]",
        ),
        (
            "catch-all conflict",
            "allow = [\"read(**)\"]\ndeny = [\"read(secret)\"]",
        ),
        (
            "prefix conflict",
            "allow = [\"read(dir/**)\"]\ndeny = [\"read(dir/secret)\"]",
        ),
        (
            "exact glob duplicate",
            "allow = [\"read(secret)\", \"read(s*)\"]",
        ),
        (
            "zero-segment doublestar conflict",
            "allow = [\"read(dir/**/secret)\"]\ndeny = [\"read(dir/secret)\"]",
        ),
        (
            "nested doublestar conflict",
            "allow = [\"read(dir/**/secret)\"]\ndeny = [\"read(dir/nested/secret)\"]",
        ),
    ];

    for (name, fields) in conflicts {
        let document = parse_toml_document(&format!("[permissions]\n{fields}"))
            .expect("fixture should be syntactically valid TOML");

        let error = extract_permission_rules(&document, &toml::Table::new()).expect_err(name);

        assert!(
            error.to_string() == "invalid configuration field permissions.allow[1]"
                || error.to_string() == "invalid configuration field permissions.deny[0]"
        );
    }
}

#[test]
fn permission_rule_extraction_rejects_cross_scope_duplicates_and_keeps_distinct_rules() {
    let global = parse_toml_document(
        r#"
            [permissions]
            allow = ["read(**)"]
        "#,
    )
    .expect("global fixture should parse");
    let project = parse_toml_document(
        r#"
            [permissions]
            allow = ["read(**)"]
        "#,
    )
    .expect("project fixture should parse");

    assert_eq!(
        extract_permission_rules(&global, &project)
            .expect_err("cross-scope duplicate must fail")
            .to_string(),
        "invalid configuration field permissions.allow[0]"
    );

    let conflicting_project = parse_toml_document(
        r#"
            [permissions]
            deny = ["read(**)"]
        "#,
    )
    .expect("project fixture should parse");

    assert_eq!(
        extract_permission_rules(&global, &conflicting_project)
            .expect_err("cross-scope conflict must fail")
            .to_string(),
        "invalid configuration field permissions.deny[0]"
    );

    let distinct_global = parse_toml_document(
        r#"
            [permissions]
            allow = ["read(global/**)"]
        "#,
    )
    .expect("global fixture should parse");
    let distinct_project = parse_toml_document(
        r#"
            [permissions]
            allow = ["read(project/**)"]
        "#,
    )
    .expect("project fixture should parse");

    let rules = extract_permission_rules(&distinct_global, &distinct_project)
        .expect("distinct scoped rules should extract");

    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].scope, ConfigPermissionScope::Global);
    assert_eq!(rules[1].scope, ConfigPermissionScope::Project);
}

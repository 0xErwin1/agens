use agens_config::{mcp_stdio_servers, parse_toml_document, validate_toml_document};

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

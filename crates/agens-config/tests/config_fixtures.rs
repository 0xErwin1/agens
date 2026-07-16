use agens_config::parse_toml_document;

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

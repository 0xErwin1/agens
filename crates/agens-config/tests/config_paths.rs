use agens_config::{expand_environment, merge_toml_documents, parse_toml_document, resolve_paths};
use std::collections::BTreeMap;
use std::path::Path;

fn environment(values: &[(&str, &str)]) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn project_configuration_overrides_global_values_and_preserves_global_sections() {
    let global = parse_toml_document(
        r#"
            [provider]
            model = "global-model"
            base_url = "https://global.example"

            [agent]
            max_iterations = 8
        "#,
    )
    .expect("global configuration should parse");
    let project = parse_toml_document(
        r#"
            [provider]
            model = "project-model"
        "#,
    )
    .expect("project configuration should parse");

    let merged = merge_toml_documents(global, project);

    assert_eq!(merged["provider"]["model"].as_str(), Some("project-model"));
    assert_eq!(
        merged["provider"]["base_url"].as_str(),
        Some("https://global.example")
    );
    assert_eq!(merged["agent"]["max_iterations"].as_integer(), Some(8));
}

#[test]
fn project_configuration_replaces_a_global_non_table_value() {
    let global =
        parse_toml_document("models = [\"global\"]").expect("global configuration should parse");
    let project =
        parse_toml_document("models = [\"project\"]").expect("project configuration should parse");

    let merged = merge_toml_documents(global, project);

    assert_eq!(merged["models"].as_array().map(Vec::len), Some(1));
    assert_eq!(merged["models"][0].as_str(), Some("project"));
}

#[test]
fn environment_expansion_supports_values_and_fallbacks() {
    let expanded = expand_environment(
        "${ROOT}/cache/$NAME/${MISSING:-default}",
        &environment(&[("ROOT", "/tmp/agens"), ("NAME", "session")]),
    )
    .expect("set values and fallbacks should expand");

    assert_eq!(expanded, "/tmp/agens/cache/session/default");
}

#[test]
fn environment_expansion_rejects_missing_or_malformed_variables() {
    let missing = expand_environment("$MISSING", &BTreeMap::new())
        .expect_err("missing values must not be silently erased");
    let malformed = expand_environment("${MISSING", &BTreeMap::new())
        .expect_err("unterminated expressions must fail");

    assert_eq!(
        missing.to_string(),
        "environment variable \"MISSING\" is not set"
    );
    assert_eq!(malformed.to_string(), "unterminated environment expression");
}

#[test]
fn agens_config_home_determines_config_and_credential_paths() {
    let paths = resolve_paths(
        Path::new("/workspace"),
        Some(Path::new("/home/alice")),
        &environment(&[("AGENS_CONFIG_HOME", "/custom/agens")]),
    );

    assert_eq!(paths.global_config, Path::new("/custom/agens/config.toml"));
    assert_eq!(paths.credentials, Path::new("/custom/agens/auth.json"));
    assert_eq!(
        paths.project_config,
        Path::new("/workspace/.agens/config.toml")
    );
}

#[test]
fn xdg_then_home_fallback_determine_compatible_paths() {
    let xdg_paths = resolve_paths(
        Path::new("/workspace"),
        Some(Path::new("/home/alice")),
        &environment(&[("XDG_CONFIG_HOME", "/xdg")]),
    );
    let home_paths = resolve_paths(
        Path::new("/workspace"),
        Some(Path::new("/home/alice")),
        &BTreeMap::new(),
    );

    assert_eq!(xdg_paths.global_config, Path::new("/xdg/agens/config.toml"));
    assert_eq!(xdg_paths.credentials, Path::new("/xdg/agens/auth.json"));
    assert_eq!(
        home_paths.global_config,
        Path::new("/home/alice/.config/agens/config.toml")
    );
    assert_eq!(
        home_paths.credentials,
        Path::new("/home/alice/.config/agens/auth.json")
    );
}

#[test]
fn unavailable_home_uses_the_legacy_relative_config_directory() {
    let paths = resolve_paths(Path::new("/workspace"), None, &BTreeMap::new());

    assert_eq!(paths.global_config, Path::new(".agens/config.toml"));
    assert_eq!(paths.credentials, Path::new(".agens/auth.json"));
}

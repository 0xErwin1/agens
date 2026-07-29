use agens_config::{
    AgentProfile, AgentProfilePatch, apply_agent_profile_patch, parse_agent_profiles,
    parse_toml_document, validate_toml_document,
};

#[test]
fn accepts_and_parses_named_agent_profiles() {
    let document = parse_toml_document(
        "[agents.explore]\nmodel = \"gpt-5\"\neffort = \"high\"\n\n[agents.review]\neffort = \"low\"\n",
    )
    .expect("fixture must parse");

    validate_toml_document(&document).expect("agent profiles must validate");

    let profiles = parse_agent_profiles(&document).expect("profiles must parse");
    assert_eq!(
        profiles.get("explore"),
        Some(&AgentProfile {
            model: Some("gpt-5".to_owned()),
            effort: Some("high".to_owned()),
        })
    );
    assert_eq!(
        profiles.get("review"),
        Some(&AgentProfile {
            model: None,
            effort: Some("low".to_owned()),
        })
    );
}

#[test]
fn round_trips_comments_and_unrelated_configuration_when_adding_a_profile() {
    let input = "# retain this comment\n[provider]\nmodel = \"session-model\"\n\n# retain this too\n[options]\ndebug = true\n";
    let output = apply_agent_profile_patch(
        input,
        "explore",
        &AgentProfilePatch {
            model: Some(Some("gpt-5".to_owned())),
            effort: Some(Some("high".to_owned())),
        },
    )
    .expect("patch must apply");

    assert!(output.starts_with(input));
    assert_eq!(
        output.strip_prefix(input),
        Some("\n[agents.explore]\nmodel = \"gpt-5\"\neffort = \"high\"\n")
    );
    let document = parse_toml_document(&output).expect("output must parse");
    validate_toml_document(&document).expect("output must validate");
}

#[test]
fn updates_only_the_requested_axis_in_an_existing_profile() {
    let input = "[agents.explore]\n# retain profile comment\nmodel = \"gpt-4\"\neffort = \"low\"\n";
    let output = apply_agent_profile_patch(
        input,
        "explore",
        &AgentProfilePatch {
            model: None,
            effort: Some(Some("max".to_owned())),
        },
    )
    .expect("patch must apply");

    assert_eq!(
        output,
        "[agents.explore]\n# retain profile comment\nmodel = \"gpt-4\"\neffort = \"max\"\n"
    );
}

#[test]
fn rejects_malformed_agent_profile_entries_with_the_offending_key() {
    for (document, key) in [
        ("[agents.x]\nmodel = 42\n", "agents.x.model"),
        ("[agents.x]\neffort = \"ludicrous\"\n", "agents.x.effort"),
        ("[agents.x]\nunknown = \"value\"\n", "agents.x.unknown"),
        ("[agents]\nmodel = \"gpt-5\"\n", "agents.model"),
    ] {
        let document = parse_toml_document(document).expect("fixture must parse");
        let error = validate_toml_document(&document).expect_err("profile must be rejected");

        assert_eq!(
            error.to_string(),
            format!("invalid configuration field {key}")
        );
    }
}

use agens_config::{
    SETTINGS, SettingValue, parse_toml_document, starter_document, validate_toml_document,
};

#[test]
fn the_starter_document_is_valid_as_emitted() {
    let document = parse_toml_document(&starter_document()).expect("starter file must parse");

    validate_toml_document(&document).expect("starter file must validate");
}

#[test]
fn the_starter_document_is_valid_with_every_default_uncommented() {
    let uncommented: String = starter_document()
        .lines()
        .map(|line| match line.strip_prefix("# ") {
            Some(rest) if rest.contains(" = ") => format!("{rest}\n"),
            _ => format!("{line}\n"),
        })
        .collect();

    let document = parse_toml_document(&uncommented).expect("uncommented starter file must parse");

    validate_toml_document(&document).expect("uncommented starter file must validate");
}

#[test]
fn the_starter_document_and_the_catalog_cannot_drift() {
    let rendered = starter_document();

    for spec in SETTINGS {
        assert!(
            rendered.contains(&format!("[{}]", spec.table())),
            "{} is missing its table header",
            spec.path
        );
        assert!(
            rendered.contains(&format!("# {} =", spec.key())),
            "{} is missing from the starter file",
            spec.path
        );
        assert!(
            rendered.contains(spec.doc),
            "{} is missing its documentation line",
            spec.path
        );
    }

    let emitted_keys = rendered
        .lines()
        .filter(|line| line.starts_with("# ") && line.contains(" ="))
        .count();
    assert_eq!(emitted_keys, SETTINGS.len());
}

#[test]
fn every_documented_default_matches_the_catalog() {
    let rendered = starter_document();

    for spec in SETTINGS {
        let expected = match spec.default {
            SettingValue::Bool(value) => format!("# {} = {value}", spec.key()),
            SettingValue::Integer(value) => format!("# {} = {value}", spec.key()),
            SettingValue::Text(value) => format!("# {} = \"{value}\"", spec.key()),
            SettingValue::Absent => format!("# {} =", spec.key()),
        };

        assert!(
            rendered.contains(&expected),
            "{} does not advertise its catalog default",
            spec.path
        );
    }
}

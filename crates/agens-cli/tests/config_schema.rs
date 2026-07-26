//! Public-API tests that exercise the crate's config schema surface without
//! needing any private item from `agens`. Unlike [`cli_contract`], which pins
//! the argv-to-`CommandResult` contract, this file covers config validation
//! behavior reachable only through `agens_config`'s own public API.

use agens_config::{parse_toml_document, validate_toml_document};

#[test]
fn the_removed_tool_output_key_is_no_longer_accepted() {
    let document = parse_toml_document("[ui]\ntruncate_tool_output = true\n").unwrap();

    assert!(validate_toml_document(&document).is_err());
}

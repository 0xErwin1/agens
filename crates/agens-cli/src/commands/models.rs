//! The `models` command: lists every provider's bundled model catalog.

use agens_error::CliError;

/// Every provider rather than one, because nothing declares a provider any more:
/// the listing has to show which one serves each model for the identifier it
/// prints to be usable.
pub(crate) fn run_models() -> Result<String, CliError> {
    let catalog = agens_models::every_source_models();
    if catalog.is_empty() {
        return Err(CliError::unavailable("model registry is unavailable"));
    }

    Ok(agens_models::format_qualified_models(&catalog))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use agens_core::HeadlessTurnCancellation;

    use crate::{CliDependencies, ExitStatus, execute_strings};

    #[test]
    fn models_command_lists_every_provider_under_its_qualified_identifier() {
        let result = execute_strings(
            vec!["models".to_owned()],
            &CliDependencies::for_test(
                PathBuf::from("/workspace"),
                None,
                BTreeMap::new(),
                BTreeMap::new(),
            ),
            &HeadlessTurnCancellation::new(),
        );

        assert_eq!(result.status, ExitStatus::Success);
        assert!(
            result.stdout.starts_with("ID\tNAME\tCONTEXT\tPRICE\n"),
            "{:?}",
            result.stdout
        );
        assert!(
            result
                .stdout
                .contains("openai-api/gpt-4.1\tGPT-4.1\t1047576\t$2.00/$8.00\n"),
            "{:?}",
            result.stdout
        );
        // A model only one provider serves and one two providers serve are both
        // listed under the provider that offers them, never bare.
        for expected in [
            "moonshotai/kimi-k3\t",
            "openai-chatgpt/gpt-5.5\t",
            "openai-api/gpt-5.5\t",
        ] {
            assert!(result.stdout.contains(expected), "{:?}", result.stdout);
        }
        assert!(
            !result.stdout.contains("\ngpt-4.1\t"),
            "{:?}",
            result.stdout
        );
    }
}

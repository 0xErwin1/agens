//! The `models` command: lists the bundled model catalog.

use agens_error::CliError;

pub(crate) fn run_models() -> Result<String, CliError> {
    agens_models::bundled_openai_models()
        .map(|models| agens_models::format_models(&models))
        .map_err(|_| CliError::unavailable("model registry is unavailable"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use agens_core::HeadlessTurnCancellation;

    use crate::{CliDependencies, ExitStatus, execute_strings};

    #[test]
    fn models_command_uses_the_bundled_registry() {
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
        assert_eq!(
            result.stdout,
            "ID\tNAME\tCONTEXT\tPRICE\ngpt-4.1\tGPT-4.1\t1047576\t$2.00/$8.00\ngpt-4.1-mini\tGPT-4.1 mini\t1047576\t$0.40/$1.60\ngpt-4.1-nano\tGPT-4.1 nano\t1047576\t$0.10/$0.40\ngpt-4o\tGPT-4o\t128000\t$2.50/$10.00\ngpt-4o-mini\tGPT-4o mini\t128000\t$0.15/$0.60\no3\to3\t200000\t$2.00/$8.00\no4-mini\to4-mini\t200000\t$1.10/$4.40\n"
        );
    }
}

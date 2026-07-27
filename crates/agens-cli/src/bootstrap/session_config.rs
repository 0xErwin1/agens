//! Configuration whose correctness depends on which root a session is confined to TODAY, not on
//! which root the current process happened to discover at `bootstrap()` time.
//!
//! [`Bootstrap`] captures its `permission_rules` once, from the PROCESS's own discovered root,
//! and never revisits them. That is correct for anything that is not a session-scoped decision,
//! but permission rules feed exactly such a decision — whether a tool call auto-authorizes or
//! prompts a human — so a value captured once from the process root can silently keep applying
//! after a resume moves the live session to a different root than the one the process started
//! at, including re-labelling another root's rules with THIS session's project key.
//!
//! `SessionConfig` closes that gap by never being cached: [`SessionConfig::resolve`] always
//! re-reads the given [`SessionRoot`]'s own `.agens/config.toml` from disk, so the rules it
//! returns can never be older than the root they are being evaluated against. The only
//! constructor takes a `SessionRoot`, not a bare `Path`, which is what removes the compile-time
//! path for a caller to hand this type a value that never went through the session's own root
//! resolution — [`Bootstrap::permission_rules`](super::Bootstrap::permission_rules) itself is
//! visible only inside this module for the same reason `discovered_root` is.
//!
//! What this makes IMPOSSIBLE: reaching the process-captured, potentially wrong-rooted
//! `permission_rules` field from outside `crate::bootstrap` — that identifier is no longer
//! reachable at all outside this module, so a session-scoped caller cannot regress back to it by
//! deleting a call to `SessionConfig::resolve` and reaching for the old field instead; the
//! compiler has nothing left to offer it.
//!
//! What this only makes INCONVENIENT, not impossible: passing the WRONG root into
//! [`SessionRoot::confined_to`]. That constructor accepts any `PathBuf`, so a caller that already
//! (incorrectly) resolved the wrong root before reaching this module can still wrap it and get a
//! `SessionConfig` re-read from that wrong root. Closing that fully would require every
//! session-scoped root in the crate to be produced and threaded exclusively as a `SessionRoot`
//! (never dissolved back to `PathBuf` in between), which is a larger, separate refactor than this
//! change makes — see the two call sites below, which wrap an already-resolved, already-correct
//! `project_root` rather than deriving one themselves.

use agens_config::{ConfigPermissionRule, ConfigPermissionScope, extract_permission_rules};

use crate::bootstrap::Bootstrap;
use crate::bootstrap::session_root::SessionRoot;
use crate::error::CliError;

/// Session-scoped configuration, re-derived fresh every time it is asked for.
pub(crate) struct SessionConfig {
    permission_rules: Vec<ConfigPermissionRule>,
}

impl SessionConfig {
    pub(crate) fn permission_rules(&self) -> &[ConfigPermissionRule] {
        &self.permission_rules
    }

    /// Combines the process's GLOBAL-scope permission rules (global configuration is keyed by
    /// the user's home directory, not by project root, so it does not need to move with a
    /// session's confinement root) with PROJECT-scope rules read fresh from `root`'s own
    /// `.agens/config.toml` — never from `bootstrap`'s own process-captured project document.
    pub(crate) fn resolve(root: &SessionRoot, bootstrap: &Bootstrap) -> Result<Self, CliError> {
        let project_config_path = root.path().join(".agens/config.toml");
        let project_document = match (bootstrap.config_reader)(&project_config_path)? {
            Some(contents) => agens_config::parse_toml_document(&contents)
                .map_err(|_| CliError::configuration("project configuration is invalid"))?,
            None => toml::Table::new(),
        };
        agens_config::validate_toml_document(&project_document)
            .map_err(|_| CliError::configuration("project configuration is invalid"))?;

        let mut permission_rules: Vec<ConfigPermissionRule> = bootstrap
            .permission_rules()
            .iter()
            .filter(|rule| rule.scope == ConfigPermissionScope::Global)
            .cloned()
            .collect();

        permission_rules.extend(
            extract_permission_rules(&toml::Table::new(), &project_document)
                .map_err(|_| CliError::configuration("permission configuration is invalid"))?,
        );

        Ok(Self { permission_rules })
    }
}

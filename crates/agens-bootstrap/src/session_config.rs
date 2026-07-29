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
//! `agent.system_prompt` and `provider.base_url` are the same shape of problem, but this module
//! does not read either of them off `Bootstrap` at all: both are re-read directly from a
//! session's OWN project document, and independently from the global document, never through the
//! process's already-merged, project-precedence [`agens_config::ResolvedSettings`]. That merge
//! only remembers the FINAL winning value and which layer won it for THIS process's own root, so
//! there is no way to recover "what would the global document have said on its own" from it once
//! a project override elsewhere has already replaced it — reading the two source documents
//! directly is what makes a session's own project override and a legitimate home-scoped global
//! default both reachable, independently of whichever root the process itself started at.
//! `agent.system_prompt` in particular is model-facing instruction text, so trusting the wrong
//! root's value would be a direct prompt-injection path, not merely a stale setting;
//! `provider.base_url` selects the endpoint a session's entire conversation is sent to, so
//! trusting the wrong root's value would silently redirect that traffic to an endpoint the
//! operator only ever configured for a different project.
//!
//! `SessionConfig` closes that gap by never being cached: [`SessionConfig::resolve`] always
//! re-reads the given [`SessionRoot`]'s own `.agens/config.toml` from disk, so the values it
//! returns can never be older than the root they are being evaluated against. The only
//! constructor takes a `SessionRoot`, not a bare `Path`, which is what removes the compile-time
//! path for a caller to hand this type a value that never went through the session's own root
//! resolution — [`Bootstrap::permission_rules`](super::Bootstrap::permission_rules) and
//! [`Bootstrap::settings`](super::Bootstrap::settings) are themselves visible only inside
//! `crate::bootstrap` for the same reason `discovered_root` is: `settings()` returns the
//! process's merged configuration under an untyped, string-keyed accessor
//! ([`agens_config::ResolvedSettings::text`]) that reaches every project-settable value,
//! including these two, with no name-level signal that a session-scoped decision is being made
//! from the wrong root.
//!
//! `agent.bypass_permission_prompts` is a THIRD session-scoped, security-relevant value read
//! here, and it goes further than the first two: it is not merely re-read fresh per session, it
//! is GLOBAL-only. [`SessionConfig::resolve`] never passes `project_document` to the helper that
//! reads it, so a project's own `.agens/config.toml` cannot enable this key no matter what it
//! declares — the same "unreachable by construction" discipline `document_text` already applies
//! to scope, applied here to make an entire SOURCE unreachable rather than just a competing root.
//!
//! What this makes IMPOSSIBLE: reaching `Bootstrap`'s process-captured `permission_rules` field,
//! or its merged `settings` (and therefore `agent.system_prompt` / `provider.base_url` through
//! it), from outside `crate::bootstrap` — those identifiers are no longer reachable at all
//! outside this module, so a session-scoped caller cannot regress back to them by deleting a
//! call to `SessionConfig::resolve` and reaching for the old field or the generic settings
//! accessor instead; the compiler has nothing left to offer it.
//!
//! What this only makes INCONVENIENT, not impossible: passing the WRONG root into
//! [`SessionRoot::confined_to`]. That constructor accepts any `PathBuf`, so a caller that already
//! (incorrectly) resolved the wrong root before reaching this module can still wrap it and get a
//! `SessionConfig` re-read from that wrong root. Closing that fully would require every
//! session-scoped root in the crate to be produced and threaded exclusively as a `SessionRoot`
//! (never dissolved back to `PathBuf` in between), which is a larger, separate refactor than this
//! change makes — see this module's call sites, which wrap an already-resolved, already-correct
//! `project_root` rather than deriving one themselves.

use agens_config::{ConfigPermissionRule, ConfigPermissionScope, extract_permission_rules};

use crate::Bootstrap;
use crate::session_root::SessionRoot;
use agens_error::CliError;

/// Session-scoped configuration, re-derived fresh every time it is asked for.
pub struct SessionConfig {
    permission_rules: Vec<ConfigPermissionRule>,
    system_prompt: Option<String>,
    provider_base_url: Option<String>,
    bypass_permission_prompts: bool,
}

impl SessionConfig {
    pub fn permission_rules(&self) -> &[ConfigPermissionRule] {
        &self.permission_rules
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn provider_base_url(&self) -> Option<&str> {
        self.provider_base_url.as_deref()
    }

    /// Whether the session should bypass `Ask` permission prompts. This is a THIRD
    /// session-scoped, security-relevant value re-read fresh from disk (alongside
    /// `system_prompt` and `provider_base_url`), and unlike those two it is deliberately
    /// GLOBAL-only: [`SessionConfig::resolve`] never passes the project document to the helper
    /// that reads this key, so a project's own `.agens/config.toml` cannot enable it no matter
    /// what it declares.
    pub fn bypass_permission_prompts(&self) -> bool {
        self.bypass_permission_prompts
    }

    /// Combines the process's GLOBAL-scope permission rules (global configuration is keyed by
    /// the user's home directory, not by project root, so it does not need to move with a
    /// session's confinement root) with PROJECT-scope rules read fresh from `root`'s own
    /// `.agens/config.toml` — never from `bootstrap`'s own process-captured project document.
    ///
    /// `system_prompt` and `provider_base_url` are re-read the same way, from BOTH the session
    /// root's own project document and the global document, each read fresh from disk here
    /// rather than taken from `bootstrap`'s already-merged settings — see this module's own
    /// documentation for why the merge cannot be trusted for either value.
    pub fn resolve(root: &SessionRoot, bootstrap: &Bootstrap) -> Result<Self, CliError> {
        let project_document = read_toml_document(
            &root.path().join(".agens/config.toml"),
            bootstrap,
            "project",
        )?;
        let global_document =
            read_toml_document(&bootstrap.paths().global_config, bootstrap, "global")?;

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

        let system_prompt = document_text(&project_document, "agent", "system_prompt")
            .or_else(|| document_text(&global_document, "agent", "system_prompt"));

        let provider_base_url = document_text(&project_document, "provider", "base_url")
            .or_else(|| document_text(&global_document, "provider", "base_url"));

        let bypass_permission_prompts =
            document_bool(&global_document, "agent", "bypass_permission_prompts").unwrap_or(false);

        Ok(Self {
            permission_rules,
            system_prompt,
            provider_base_url,
            bypass_permission_prompts,
        })
    }
}

/// Reads and validates a single TOML configuration document fresh from disk, through the same
/// `config_reader` `bootstrap()` itself used, so a session-scoped re-read observes the same
/// document a fresh `bootstrap()` at that path would.
fn read_toml_document(
    path: &std::path::Path,
    bootstrap: &Bootstrap,
    scope: &str,
) -> Result<toml::Table, CliError> {
    let document = match (bootstrap.config_reader)(path)? {
        Some(contents) => agens_config::parse_toml_document(&contents)
            .map_err(|_| CliError::configuration(format!("{scope} configuration is invalid")))?,
        None => toml::Table::new(),
    };
    agens_config::validate_toml_document(&document)
        .map_err(|_| CliError::configuration(format!("{scope} configuration is invalid")))?;
    Ok(document)
}

/// Reads `document.section.key` directly from a single document — never merged with the other
/// scope — so a scope's own setting is honored without also picking up a value that only exists
/// because it happened to be present in the SAME document at some OTHER path.
fn document_text(document: &toml::Table, section: &str, key: &str) -> Option<String> {
    document
        .get(section)
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get(key))
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned)
}

/// Reads `document.section.key` as a boolean directly from a single document, mirroring
/// [`document_text`]'s scope discipline: the caller decides which document to pass, so a value
/// can be made unreachable from a given scope structurally rather than by convention.
fn document_bool(document: &toml::Table, section: &str, key: &str) -> Option<bool> {
    document
        .get(section)
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get(key))
        .and_then(toml::Value::as_bool)
}

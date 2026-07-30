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

use std::collections::BTreeMap;
use std::path::PathBuf;

use agens_config::{
    AgentProfile, ConfigPermissionRule, ConfigPermissionScope, extract_permission_rules,
    parse_agent_profiles,
};
use agens_tools::markdown::{MAX_MARKDOWN_FILE_BYTES, load_instruction_file};

use crate::Bootstrap;
use crate::session_root::SessionRoot;
use agens_error::CliError;

/// Agent profiles read independently from the global and project configuration documents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopedAgentProfiles {
    global: BTreeMap<String, AgentProfile>,
    project: BTreeMap<String, AgentProfile>,
}

impl ScopedAgentProfiles {
    pub fn new(
        global: BTreeMap<String, AgentProfile>,
        project: BTreeMap<String, AgentProfile>,
    ) -> Self {
        Self { global, project }
    }

    pub fn global(&self) -> &BTreeMap<String, AgentProfile> {
        &self.global
    }

    pub fn project(&self) -> &BTreeMap<String, AgentProfile> {
        &self.project
    }
}

/// Session-scoped configuration, re-derived fresh every time it is asked for.
pub struct SessionConfig {
    permission_rules: Vec<ConfigPermissionRule>,
    system_prompt: Option<String>,
    provider_base_url: Option<String>,
    bypass_permission_prompts: bool,
    agent_profiles: ScopedAgentProfiles,
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

    pub fn agent_profiles(&self) -> &ScopedAgentProfiles {
        &self.agent_profiles
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
        let agent_profiles = ScopedAgentProfiles::new(
            parse_agent_profiles(&global_document)
                .map_err(|_| CliError::configuration("global configuration is invalid"))?,
            parse_agent_profiles(&project_document)
                .map_err(|_| CliError::configuration("project configuration is invalid"))?,
        );

        let bypass_permission_prompts =
            document_bool(&global_document, "agent", "bypass_permission_prompts").unwrap_or(false);

        Ok(Self {
            permission_rules,
            system_prompt,
            provider_base_url,
            bypass_permission_prompts,
            agent_profiles,
        })
    }
}

/// The composed `AGENTS.md` instruction text for a session's own root, re-derived fresh every
/// time it is asked for — the same shape of problem [`SessionConfig`] closes for
/// `agent.system_prompt` and `provider.base_url`, and for the same reason: this text is
/// model-facing, so trusting the wrong root's file would be a direct prompt-injection path, not
/// merely a stale setting.
///
/// The only constructor takes a [`SessionRoot`], never a bare `&Path`, and nothing is stored on
/// [`Bootstrap`]: those two properties are what make the session-root confinement invariant
/// structural rather than conventional, mirroring `SessionConfig`'s own constructor shape.
///
/// Both candidate files are read through the real filesystem via
/// [`load_instruction_file`](agens_tools::markdown::load_instruction_file), deliberately NOT
/// through `Bootstrap`'s injected `config_reader`: that reader hands back an already-decoded
/// `String`, which makes the symlink, non-regular-file, oversized, and invalid-UTF-8 rejection
/// modes unreachable through it.
///
/// [`SessionInstructions::resolve`] is infallible: every rejection (missing, symlink, non-regular
/// file, oversized, invalid UTF-8, unreadable, or blank content) is a deliberate, silent skip, so
/// a broken candidate on one path never prevents the other from being appended.
pub struct SessionInstructions {
    text: Option<String>,
}

impl SessionInstructions {
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Reads GLOBAL (`bootstrap.paths().global_config.with_file_name("AGENTS.md")`) then PROJECT
    /// (`root.path().join("AGENTS.md")`), composing accepted, non-blank, not-already-seen
    /// content into provenance-labelled blocks joined with `"\n\n"`. A block that would push the
    /// composed text over [`MAX_MARKDOWN_FILE_BYTES`] is dropped WHOLE — content is never
    /// truncated mid-file.
    pub fn resolve(root: &SessionRoot, bootstrap: &Bootstrap) -> Self {
        let global_path = bootstrap.paths().global_config.with_file_name("AGENTS.md");
        let project_path = root.path().join("AGENTS.md");

        let mut accumulated = String::new();
        let mut seen_sources: Vec<PathBuf> = Vec::new();

        for path in [global_path, project_path] {
            let Ok(Some(file)) = load_instruction_file(&path) else {
                continue;
            };
            if file.contents().trim().is_empty() {
                continue;
            }
            if seen_sources.contains(&file.source().to_path_buf()) {
                continue;
            }

            let block = format!(
                "## Instructions from {}\n{}",
                file.source().display(),
                file.contents()
            );
            if accumulated.len() + 2 + block.len() > MAX_MARKDOWN_FILE_BYTES {
                continue;
            }

            if !accumulated.is_empty() {
                accumulated.push_str("\n\n");
            }
            accumulated.push_str(&block);
            seen_sources.push(file.source().to_path_buf());
        }

        Self {
            text: (!accumulated.is_empty()).then_some(accumulated),
        }
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

#[cfg(test)]
mod session_instructions_tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::SessionInstructions;
    use crate::HostEnvironment;
    use crate::session_root::SessionRoot;

    static NEXT_CASE: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        root: PathBuf,
        config_home: PathBuf,
        project_root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).ok();
        }
    }

    /// Roots the session under a fresh temp directory AND points `AGENS_CONFIG_HOME` at a
    /// distinct temp directory, so a broken test can never accidentally read the developer's
    /// own `~/.config/agens/AGENTS.md` (this repo's own root `AGENTS.md` is 21 KB).
    fn fixture() -> Fixture {
        let suffix = NEXT_CASE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "agens-session-instructions-{}-{suffix}",
            std::process::id()
        ));
        let config_home = root.join("config");
        let project_root = root.join("project");
        fs::create_dir_all(&config_home).unwrap();
        fs::create_dir_all(&project_root).unwrap();
        Fixture {
            root,
            config_home,
            project_root,
        }
    }

    fn bootstrap(fixture: &Fixture) -> crate::Bootstrap {
        let host = HostEnvironment::fixed(
            fixture.project_root.clone(),
            Some(fixture.root.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                fixture.config_home.display().to_string(),
            )]),
            BTreeMap::new(),
        );
        crate::resolve(&host).expect("bootstrap resolves against an empty configuration")
    }

    fn global_path(fixture: &Fixture) -> PathBuf {
        fixture.config_home.join("AGENTS.md")
    }

    fn project_path(fixture: &Fixture) -> PathBuf {
        fixture.project_root.join("AGENTS.md")
    }

    fn session_root(fixture: &Fixture) -> SessionRoot {
        SessionRoot::confined_to(fixture.project_root.clone())
    }

    #[test]
    fn global_instructions_precede_project_instructions() {
        let fixture = fixture();
        fs::write(global_path(&fixture), "GLOBAL-TEXT").unwrap();
        fs::write(project_path(&fixture), "PROJECT-TEXT").unwrap();
        let bootstrap = bootstrap(&fixture);

        let instructions = SessionInstructions::resolve(&session_root(&fixture), &bootstrap);
        let text = instructions.text().expect("both files are present");

        let global_index = text.find("GLOBAL-TEXT").expect("global text is present");
        let project_index = text.find("PROJECT-TEXT").expect("project text is present");
        assert!(
            global_index < project_index,
            "global instructions must precede project instructions: {text:?}"
        );
    }

    #[test]
    fn label_uses_the_exact_provenance_format() {
        let fixture = fixture();
        fs::write(project_path(&fixture), "ONLY-PROJECT-TEXT").unwrap();
        let bootstrap = bootstrap(&fixture);

        let instructions = SessionInstructions::resolve(&session_root(&fixture), &bootstrap);
        let text = instructions.text().expect("the project file is present");

        let canonical = fs::canonicalize(project_path(&fixture)).unwrap();
        let expected = format!(
            "## Instructions from {}\nONLY-PROJECT-TEXT",
            canonical.display()
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn identical_canonical_paths_are_deduplicated() {
        let fixture = fixture();
        // Redirect the config home to the project root itself, so GLOBAL and PROJECT are
        // literally the same path (`project_root/AGENTS.md`) once canonicalized — a hard link
        // would only make them the same INODE at two different paths, and canonicalization does
        // not collapse hard links back to one path.
        let host = HostEnvironment::fixed(
            fixture.project_root.clone(),
            Some(fixture.root.join("home")),
            BTreeMap::from([(
                "AGENS_CONFIG_HOME".to_owned(),
                fixture.project_root.display().to_string(),
            )]),
            BTreeMap::new(),
        );
        let bootstrap = crate::resolve(&host).expect("bootstrap resolves");
        fs::write(fixture.project_root.join("AGENTS.md"), "SHARED-TEXT").unwrap();

        let instructions = SessionInstructions::resolve(&session_root(&fixture), &bootstrap);
        let text = instructions.text().expect("the shared file is present");

        assert_eq!(
            text.matches("SHARED-TEXT").count(),
            1,
            "identical canonical paths must contribute their content exactly once: {text:?}"
        );
    }

    fn block(path: &Path, content: &str) -> String {
        format!("## Instructions from {}\n{}", path.display(), content)
    }

    #[test]
    fn combined_text_at_the_cap_boundary_keeps_both_blocks() {
        let fixture = fixture();
        let global_canonical_len_estimate = block(&global_path(&fixture), "").len();
        let project_canonical_len_estimate = block(&project_path(&fixture), "").len();

        // Reserve exactly enough room so `global_block.len() + 2 + project_block.len()` equals
        // the cap, using the canonicalized paths (temp-dir paths already are absolute).
        let global_canonical = fixture.config_home.join("AGENTS.md");
        let project_canonical = fixture.project_root.join("AGENTS.md");
        let label_overhead = global_canonical_len_estimate + project_canonical_len_estimate;
        let budget = agens_tools::markdown::MAX_MARKDOWN_FILE_BYTES - label_overhead - 2;
        let global_content = "g".repeat(budget / 2);
        let project_content = "p".repeat(budget - budget / 2);

        fs::write(global_path(&fixture), &global_content).unwrap();
        fs::write(project_path(&fixture), &project_content).unwrap();
        let bootstrap = bootstrap(&fixture);

        let instructions = SessionInstructions::resolve(&session_root(&fixture), &bootstrap);
        let text = instructions.text().expect("both files fit exactly");

        let expected = format!(
            "{}\n\n{}",
            block(&global_canonical, &global_content),
            block(&project_canonical, &project_content)
        );
        assert_eq!(text, expected);
        assert_eq!(text.len(), agens_tools::markdown::MAX_MARKDOWN_FILE_BYTES);
    }

    #[test]
    fn one_byte_over_the_cap_drops_the_whole_offending_file() {
        let fixture = fixture();
        let global_canonical = fixture.config_home.join("AGENTS.md");
        let project_canonical = fixture.project_root.join("AGENTS.md");
        let label_overhead =
            block(&global_canonical, "").len() + block(&project_canonical, "").len();
        let budget = agens_tools::markdown::MAX_MARKDOWN_FILE_BYTES - label_overhead - 2;
        let global_content = "g".repeat(budget / 2);
        let project_content = "p".repeat(budget - budget / 2 + 1);

        fs::write(global_path(&fixture), &global_content).unwrap();
        fs::write(project_path(&fixture), &project_content).unwrap();
        let bootstrap = bootstrap(&fixture);

        let instructions = SessionInstructions::resolve(&session_root(&fixture), &bootstrap);
        let text = instructions
            .text()
            .expect("the global file alone still fits");

        assert_eq!(
            text,
            block(&global_canonical, &global_content),
            "the global block must be present in full and no partial project content may leak in"
        );
    }

    #[test]
    fn only_global_present() {
        let fixture = fixture();
        fs::write(global_path(&fixture), "GLOBAL-ONLY").unwrap();
        let bootstrap = bootstrap(&fixture);

        let instructions = SessionInstructions::resolve(&session_root(&fixture), &bootstrap);

        assert_eq!(
            instructions.text(),
            Some(block(&fixture.config_home.join("AGENTS.md"), "GLOBAL-ONLY")).as_deref()
        );
    }

    #[test]
    fn only_project_present() {
        let fixture = fixture();
        fs::write(project_path(&fixture), "PROJECT-ONLY").unwrap();
        let bootstrap = bootstrap(&fixture);

        let instructions = SessionInstructions::resolve(&session_root(&fixture), &bootstrap);

        assert_eq!(
            instructions.text(),
            Some(block(
                &fixture.project_root.join("AGENTS.md"),
                "PROJECT-ONLY"
            ))
            .as_deref()
        );
    }

    #[test]
    fn an_ancestor_file_is_not_discovered() {
        let fixture = fixture();
        // `fixture.root` is the PARENT of `fixture.project_root`, and is a temp directory this
        // test controls — not a real ancestor of the repository. Discovery must consider only
        // the global path and the project root's own path, with no walk up the directory tree.
        fs::write(fixture.root.join("AGENTS.md"), "ANCESTOR-TEXT").unwrap();
        let bootstrap = bootstrap(&fixture);

        let instructions = SessionInstructions::resolve(&session_root(&fixture), &bootstrap);

        assert_eq!(
            instructions.text(),
            None,
            "an AGENTS.md in a parent directory of the session root must be ignored"
        );
    }

    #[test]
    fn neither_present_leaves_text_absent() {
        let fixture = fixture();
        let bootstrap = bootstrap(&fixture);

        let instructions = SessionInstructions::resolve(&session_root(&fixture), &bootstrap);

        assert_eq!(instructions.text(), None);
    }

    fn break_with_symlink(path: &Path) {
        let target = path.with_file_name("elsewhere.md");
        fs::write(&target, "unused").unwrap();
        std::os::unix::fs::symlink(target, path).unwrap();
    }

    fn break_with_unreadable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, "unreadable").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    }

    fn break_with_invalid_utf8(path: &Path) {
        fs::write(path, [0xff, 0xfe, 0xfd]).unwrap();
    }

    fn break_with_empty(path: &Path) {
        fs::write(path, "").unwrap();
    }

    type BreakFn = fn(&Path);

    #[test]
    fn a_broken_global_file_does_not_suppress_a_valid_project_file() {
        let cases: [(&str, BreakFn); 4] = [
            ("symlink", break_with_symlink),
            ("unreadable", break_with_unreadable),
            ("invalid utf-8", break_with_invalid_utf8),
            ("empty", break_with_empty),
        ];

        for (name, break_global) in cases {
            let fixture = fixture();
            break_global(&global_path(&fixture));
            fs::write(project_path(&fixture), "VALID-PROJECT-TEXT").unwrap();
            let bootstrap = bootstrap(&fixture);

            let instructions = SessionInstructions::resolve(&session_root(&fixture), &bootstrap);

            if name == "unreadable" {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(global_path(&fixture), fs::Permissions::from_mode(0o644))
                    .unwrap();
            }

            assert_eq!(
                instructions.text(),
                Some(block(
                    &fixture.project_root.join("AGENTS.md"),
                    "VALID-PROJECT-TEXT"
                ))
                .as_deref(),
                "case {name}: a broken global file must not suppress a valid project file"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use crate::host::HostEnvironment;
    use crate::session_root::SessionRoot;
    use crate::{resolve, session_config::SessionConfig};

    #[test]
    fn agent_profiles_are_read_from_each_scope_without_merging() {
        let home = PathBuf::from("/home/tester");
        let session_root = PathBuf::from("/session");
        let files = BTreeMap::from([
            (
                home.join(".config/agens/config.toml"),
                "[agents.research]\nmodel = \"global-model\"\n".to_owned(),
            ),
            (
                session_root.join(".agens/config.toml"),
                "[agents.research]\neffort = \"high\"\n".to_owned(),
            ),
        ]);
        let bootstrap = resolve(&HostEnvironment::fixed(
            PathBuf::from("/process"),
            Some(home),
            BTreeMap::new(),
            files,
        ))
        .expect("bootstrap configuration resolves");

        let config = SessionConfig::resolve(&SessionRoot::confined_to(session_root), &bootstrap)
            .expect("session configuration resolves");
        let profiles = config.agent_profiles();

        assert_eq!(
            profiles
                .global()
                .get("research")
                .and_then(|profile| profile.model.as_deref()),
            Some("global-model")
        );
        assert_eq!(
            profiles
                .project()
                .get("research")
                .and_then(|profile| profile.effort.as_deref()),
            Some("high")
        );
        assert_eq!(
            profiles
                .project()
                .get("research")
                .and_then(|profile| profile.model.as_deref()),
            None
        );
    }

    #[test]
    fn agent_profiles_are_re_read_for_each_session_resolution() {
        let home = PathBuf::from("/home/tester");
        let session_root = PathBuf::from("/session");
        let global_config = home.join(".config/agens/config.toml");
        let document = Arc::new(Mutex::new(
            "[agents.research]\nmodel = \"first\"\n".to_owned(),
        ));
        let reader_document = Arc::clone(&document);
        let host = HostEnvironment {
            current_directory: Box::new(|| Ok(PathBuf::from("/process"))),
            home_directory: Box::new({
                let home = home.clone();
                move || Some(home.clone())
            }),
            environment: Box::new(BTreeMap::new),
            read_file: Arc::new(move |path: &Path| {
                Ok((path == global_config)
                    .then(|| reader_document.lock().expect("document lock").clone()))
            }),
        };
        let bootstrap = resolve(&host).expect("bootstrap configuration resolves");
        let root = SessionRoot::confined_to(session_root);

        assert_eq!(
            SessionConfig::resolve(&root, &bootstrap)
                .expect("first session configuration resolves")
                .agent_profiles()
                .global()
                .get("research")
                .and_then(|profile| profile.model.as_deref()),
            Some("first")
        );

        *document.lock().expect("document lock") =
            "[agents.research]\nmodel = \"second\"\n".to_owned();

        assert_eq!(
            SessionConfig::resolve(&root, &bootstrap)
                .expect("second session configuration resolves")
                .agent_profiles()
                .global()
                .get("research")
                .and_then(|profile| profile.model.as_deref()),
            Some("second")
        );
    }
}

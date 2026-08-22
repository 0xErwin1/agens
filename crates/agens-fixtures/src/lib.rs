//! Test fixtures shared across crates: an isolated project directory, a
//! `Bootstrap` resolved from a fixed host, and a deadline-based wait.
//!
//! These exist as a crate rather than a module because more than one crate's
//! tests need them, and the alternative — keeping them next to the binary —
//! forced every crate that wanted a configured `Bootstrap` to stay in the
//! binary crate with them. Nothing here touches a user interface, so depending
//! on it does not pull a surface into a logic crate's test build.

pub mod network_tripwire;
pub mod scripted_provider;

pub use network_tripwire::{NetworkTripwire, PROVIDER_CREDENTIAL_VARIABLES};
pub use scripted_provider::{
    ObservedRequest, Script, ScriptedDialect, ScriptedProvider, ScriptedTurn,
};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agens_bootstrap::{Bootstrap, HostEnvironment};
use agens_models::{ModelSelection, ModelSource};
use agens_tools::AgentModelValidator;

/// Waits for a condition rather than for a fixed number of polls.
///
/// A count-based wait is a bet on how fast the machine is. Under a loaded gate
/// it loses, and the test then fails for a reason that has nothing to do with
/// what it asserts, which trains people to rerun the gate instead of reading it.
pub fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

    loop {
        if let Some(value) = probe() {
            return value;
        }

        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn resolve(
    current_directory: PathBuf,
    home_directory: Option<PathBuf>,
    environment: BTreeMap<String, String>,
    files: BTreeMap<PathBuf, String>,
) -> Result<Bootstrap, agens_error::CliError> {
    agens_bootstrap::resolve(&HostEnvironment::fixed(
        current_directory,
        home_directory,
        environment,
        files,
    ))
}

fn config_home_environment(config_home: &Path) -> BTreeMap<String, String> {
    BTreeMap::from([(
        "AGENS_CONFIG_HOME".to_owned(),
        config_home.display().to_string(),
    )])
}

/// The provider a configuration fragment names, defaulting to the OpenAI API
/// the way the untouched fixtures always have.
fn configured_provider(global: Option<&str>) -> &'static str {
    let Some(global) = global else {
        return "openai-api";
    };

    // A provider is named by a qualified model identifier now, so the prefix is
    // what a fragment carries.
    ["openai-chatgpt", "moonshotai", "openai-api"]
        .into_iter()
        .find(|provider| global.contains(&format!("\"{provider}/")))
        .unwrap_or("openai-api")
}

/// The credentials a fixture provider needs to authenticate.
///
/// A run now picks its provider from the model it was given, and only from
/// providers it can actually reach, so a fixture that names a provider has to
/// supply that provider's credentials the way a real machine would.
fn provider_credentials(provider: &str) -> String {
    match provider {
        "openai-chatgpt" => concat!(
            r#"{"openai-chatgpt": {"access_token": "fixture", "refresh_token": "fixture", "#,
            r#""account_id": "fixture", "expires_at": "2099-01-01T00:00:00Z"}}"#
        )
        .to_owned(),
        other => format!(r#"{{"{other}": {{"api_key": "fixture"}}}}"#),
    }
}

/// A configuration naming the provider the only way one can be named: through
/// the model identifier.
fn provider_configuration(provider: &str, model: &str, data_directory: &Path) -> String {
    format!(
        "[provider]\nmodel = \"{provider}/{model}\"\n\n[options]\ndata_dir = \"{}\"\n",
        data_directory.display()
    )
}

/// A `Bootstrap` fixture from optional global and project TOML fragments,
/// isolated under a unique temporary directory named after `label`.
pub fn bootstrap_from_configuration(
    label: &str,
    global: Option<&str>,
    project: Option<&str>,
) -> Bootstrap {
    try_bootstrap_from_configuration(label, global, project)
        .expect("configuration fixture should be valid")
}

/// The same fixture, keeping the rejection so a test can assert how invalid
/// configuration is refused rather than only that valid configuration resolves.
pub fn try_bootstrap_from_configuration(
    label: &str,
    global: Option<&str>,
    project: Option<&str>,
) -> Result<Bootstrap, agens_error::CliError> {
    let temporary = std::env::temp_dir().join(format!("agens-{label}-{}", std::process::id()));
    let config_home = temporary.join("config");
    let project_root = temporary.join("project");

    // Credentials for the provider this configuration actually names: a run can
    // only select a provider it can authenticate, and authenticating a second
    // one would make a bare model identifier ambiguous.
    let provider = configured_provider(global);
    let credentials = config_home.join("auth.json");
    std::fs::create_dir_all(&config_home).expect("fixture config home should be created");
    std::fs::write(&credentials, provider_credentials(provider))
        .expect("fixture credentials should be written");

    let mut files = BTreeMap::new();
    files.insert(credentials, provider_credentials(provider));
    if let Some(global) = global {
        files.insert(config_home.join("config.toml"), global.to_owned());
    }
    if let Some(project) = project {
        files.insert(project_root.join(".agens/config.toml"), project.to_owned());
    }

    resolve(
        project_root,
        Some(temporary.join("home")),
        config_home_environment(&config_home),
        files,
    )
}

/// A second bootstrap sharing `origin`'s data directory (and therefore its
/// sessions database) but discovering its own project root from a completely
/// different, unrelated working directory — simulating a process restart from
/// elsewhere on disk.
pub fn bootstrap_from_a_different_working_directory(origin: &Path, label: &str) -> Bootstrap {
    let elsewhere = session_directory(label);
    let config_home = origin.join("config");

    resolve(
        elsewhere.join("project"),
        Some(elsewhere.join("home")),
        config_home_environment(&config_home),
        BTreeMap::from([
            (
                config_home.join("config.toml"),
                provider_configuration("openai-api", "gpt-4.1", &origin.join("data")),
            ),
            (
                config_home.join("auth.json"),
                provider_credentials("openai-api"),
            ),
        ]),
    )
    .expect("relocated bootstrap fixture should be valid")
}

/// A fresh, uniquely named temporary directory with a project marker
/// (`project/.git`) already created, isolating one test's filesystem state from
/// every other test running concurrently.
pub fn session_directory(label: &str) -> PathBuf {
    let temporary = std::env::temp_dir().join(format!(
        "agens-session-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the fixture clock is after the epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(temporary.join("project/.git"))
        .expect("fixture project directory should be created");
    temporary
}

/// A `Bootstrap` fixture with several providers authenticated, so a bare model
/// identifier several of them serve is genuinely ambiguous.
pub fn session_bootstrap_from_credentials(
    temporary: &Path,
    agents: &[(&str, &str)],
    providers: &[&str],
    model: &str,
) -> Bootstrap {
    let config_home = temporary.join("config");
    let agents_directory = config_home.join("agents");

    std::fs::create_dir_all(&agents_directory).expect("fixture agents directory should be created");
    for (name, contents) in agents {
        std::fs::write(agents_directory.join(format!("{name}.md")), contents)
            .expect("fixture agent definition should be written");
    }

    let entries: Vec<String> = providers
        .iter()
        .map(|provider| {
            let single = provider_credentials(provider);
            let inner = single.trim();
            inner
                .strip_prefix('{')
                .and_then(|value| value.strip_suffix('}'))
                .expect("fixture credentials are a JSON object")
                .to_owned()
        })
        .collect();
    let credentials = format!("{{{}}}", entries.join(", "));
    std::fs::write(config_home.join("auth.json"), &credentials)
        .expect("fixture credentials should be written");

    resolve(
        temporary.join("project"),
        Some(temporary.join("home")),
        config_home_environment(&config_home),
        BTreeMap::from([
            (
                config_home.join("config.toml"),
                format!(
                    "[provider]\nmodel = \"{model}\"\n\n[options]\ndata_dir = \"{}\"\n",
                    temporary.join("data").display()
                ),
            ),
            (config_home.join("auth.json"), credentials),
        ]),
    )
    .expect("session bootstrap fixture should be valid")
}

/// A `Bootstrap` fixture wired for the OpenAI API provider, with the given
/// agent definitions written under the fixture's config directory.
pub fn session_bootstrap(temporary: &Path, agents: &[(&str, &str)]) -> Bootstrap {
    session_bootstrap_for_provider(temporary, agents, "openai-api", "gpt-4.1")
}

/// A `Bootstrap` fixture wired for the given provider and model, with the given
/// agent definitions written under the fixture's config directory.
pub fn session_bootstrap_for_provider(
    temporary: &Path,
    agents: &[(&str, &str)],
    provider: &str,
    model: &str,
) -> Bootstrap {
    let config_home = temporary.join("config");
    let agents_directory = config_home.join("agents");

    std::fs::create_dir_all(&agents_directory).expect("fixture agents directory should be created");
    for (name, contents) in agents {
        std::fs::write(agents_directory.join(format!("{name}.md")), contents)
            .expect("fixture agent definition should be written");
    }

    let credentials = config_home.join("auth.json");
    std::fs::create_dir_all(&config_home).expect("fixture config home should be created");
    // Never clobber credentials a test wrote for itself: some fixtures assert on
    // entries this default would replace.
    if !credentials.exists() {
        std::fs::write(&credentials, provider_credentials(provider))
            .expect("fixture credentials should be written");
    }

    resolve(
        temporary.join("project"),
        Some(temporary.join("home")),
        config_home_environment(&config_home),
        BTreeMap::from([
            (
                config_home.join("config.toml"),
                provider_configuration(provider, model, &temporary.join("data")),
            ),
            (
                credentials.clone(),
                std::fs::read_to_string(&credentials)
                    .unwrap_or_else(|_| provider_credentials(provider)),
            ),
        ]),
    )
    .expect("session bootstrap fixture should be valid")
}

/// A `Bootstrap` fixture with no configured model and no credentials.
///
/// The state a run is in before any provider can be resolved: provider-dependent
/// lookups decline rather than resolve, so a test can exercise that path.
pub fn session_bootstrap_without_provider(temporary: &Path, agents: &[(&str, &str)]) -> Bootstrap {
    let config_home = temporary.join("config");
    let agents_directory = config_home.join("agents");

    std::fs::create_dir_all(&agents_directory).expect("fixture agents directory should be created");
    for (name, contents) in agents {
        std::fs::write(agents_directory.join(format!("{name}.md")), contents)
            .expect("fixture agent definition should be written");
    }

    let configuration = format!(
        "[options]\ndata_dir = \"{}\"\n",
        temporary.join("data").display()
    );

    resolve(
        temporary.join("project"),
        Some(temporary.join("home")),
        config_home_environment(&config_home),
        BTreeMap::from([(config_home.join("config.toml"), configuration)]),
    )
    .expect("session bootstrap fixture should be valid")
}

/// Accepts any model the bundled catalog knows, under either provider.
///
/// Tests that care about agent selection rather than about model availability
/// use this so a catalog change does not reach them.
pub struct BundledModelValidator;

impl AgentModelValidator for BundledModelValidator {
    fn validate_model(&self, model: &str) -> Result<(), agens_tools::AgentModelValidationError> {
        [ModelSource::OpenAiApi, ModelSource::ChatGptSubscription]
            .into_iter()
            .any(|source| {
                ModelSelection::for_source(model, source)
                    .model_values()
                    .is_ok_and(|models| models.iter().any(|candidate| candidate == model))
            })
            .then_some(())
            .ok_or(agens_tools::AgentModelValidationError::Unavailable)
    }
}

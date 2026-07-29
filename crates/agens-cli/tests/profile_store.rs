use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use agens::profile_store::{AgentProfileStore, ProfileScope};
use agens_config::AgentProfilePatch;

fn temporary_directory(label: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agens-profile-store-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("temporary directory must be created");
    path
}

#[test]
fn saves_to_the_selected_scope_and_creates_missing_profile_tables() {
    let root = temporary_directory("scope");
    let global = root.join("global/config.toml");
    let project = root.join("project/.agens/config.toml");
    fs::create_dir_all(global.parent().expect("global parent")).expect("global parent must exist");
    fs::create_dir_all(project.parent().expect("project parent"))
        .expect("project parent must exist");
    fs::write(
        &global,
        "# global comment\n[provider]\nmodel = \"global-model\"\n",
    )
    .expect("global fixture must be written");
    fs::write(&project, "[provider]\nmodel = \"project-model\"\n")
        .expect("project fixture must be written");

    let store = AgentProfileStore::new(global.clone(), project.clone());
    let snapshot = store
        .read(ProfileScope::Global)
        .expect("global snapshot must load");
    store
        .save(
            ProfileScope::Global,
            &snapshot,
            "explore",
            &AgentProfilePatch {
                model: Some(Some("gpt-5".to_owned())),
                effort: None,
            },
        )
        .expect("global profile must save");

    assert_eq!(
        fs::read_to_string(&global).expect("global config must remain readable"),
        "# global comment\n[provider]\nmodel = \"global-model\"\n\n[agents.explore]\nmodel = \"gpt-5\"\n"
    );
    assert_eq!(
        fs::read_to_string(&project).expect("project config must remain unchanged"),
        "[provider]\nmodel = \"project-model\"\n"
    );

    fs::remove_dir_all(root).expect("temporary directory must be removed");
}

#[test]
fn creates_new_profile_files_with_private_permissions() {
    let root = temporary_directory("permissions");
    let global = root.join("global/config.toml");
    let project = root.join("project/.agens/config.toml");
    let store = AgentProfileStore::new(global, project.clone());
    let snapshot = store
        .read(ProfileScope::Project)
        .expect("missing snapshot must load");

    store
        .save(
            ProfileScope::Project,
            &snapshot,
            "explore",
            &AgentProfilePatch {
                model: Some(Some("gpt-5".to_owned())),
                effort: None,
            },
        )
        .expect("project profile must save");

    assert_eq!(
        fs::metadata(&project)
            .expect("project config must exist")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    fs::remove_dir_all(root).expect("temporary directory must be removed");
}

#[test]
fn rejects_a_concurrent_change_without_replacing_the_original_file() {
    let root = temporary_directory("cas");
    let global = root.join("global/config.toml");
    let project = root.join("project/.agens/config.toml");
    fs::create_dir_all(global.parent().expect("global parent")).expect("global parent must exist");
    fs::write(&global, "[provider]\nmodel = \"before\"\n").expect("fixture must be written");

    let store = AgentProfileStore::new(global.clone(), project);
    let snapshot = store
        .read(ProfileScope::Global)
        .expect("snapshot must load");
    fs::write(&global, "[provider]\nmodel = \"external-change\"\n")
        .expect("concurrent change must be written");

    let error = store
        .save(
            ProfileScope::Global,
            &snapshot,
            "explore",
            &AgentProfilePatch {
                model: Some(Some("gpt-5".to_owned())),
                effort: None,
            },
        )
        .expect_err("concurrent modification must be rejected");

    assert_eq!(error.to_string(), "profile config changed concurrently");
    assert_eq!(
        fs::read_to_string(&global).expect("externally changed config must remain readable"),
        "[provider]\nmodel = \"external-change\"\n"
    );

    fs::remove_dir_all(root).expect("temporary directory must be removed");
}

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use agens_tools::SkillCatalog;

#[test]
fn discovers_global_and_project_skills_with_project_shadowing() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    let project = temporary.path.join("project");

    write_skill(
        &global,
        "shared",
        "---\nname: shared\ndescription: global shared skill\n---\nglobal instructions\n",
    );
    write_skill(
        &global,
        "global-only",
        "---\nname: global-only\ndescription: global only skill\n---\nglobal only instructions\n",
    );
    write_skill(
        &project,
        "shared",
        "---\nname: shared\ndescription: project shared skill\n---\nproject instructions\n",
    );

    let discovery = SkillCatalog::discover(&global, &project).expect("discover skills");

    assert_eq!(discovery.catalog().len(), 2);
    assert_eq!(
        discovery
            .catalog()
            .skill("shared")
            .expect("project skill")
            .description(),
        "project shared skill"
    );
    assert_eq!(
        discovery
            .catalog()
            .skill("shared")
            .expect("project skill")
            .body(),
        "project instructions"
    );
    assert!(discovery.catalog().skill("global-only").is_some());
    assert_eq!(discovery.shadowed().len(), 1);
}

#[test]
fn isolates_invalid_and_ambiguous_skills_without_losing_valid_or_global_skills() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    let project = temporary.path.join("project");

    write_skill(
        &global,
        "shared",
        "---\nname: shared\ndescription: global skill\n---\nglobal instructions\n",
    );
    write_skill(
        &project,
        "bad-name",
        "---\nname: bad name\ndescription: invalid name\n---\nbody\n",
    );
    write_skill(
        &project,
        "duplicate-one",
        "---\nname: duplicate\ndescription: one\n---\nbody\n",
    );
    write_skill(
        &project,
        "duplicate-two",
        "---\nname: duplicate\ndescription: two\n---\nbody\n",
    );
    write_skill(
        &project,
        "valid",
        "---\r\nname: valid\r\ndescription: \"quoted description\"\r\n---\r\nvalid body\r\n",
    );

    let discovery = SkillCatalog::discover(&global, &project).expect("discover skills");

    assert_eq!(discovery.catalog().len(), 2);
    assert!(discovery.catalog().skill("shared").is_some());
    assert!(discovery.catalog().skill("valid").is_some());
    assert!(discovery.catalog().skill("duplicate").is_none());
    assert_eq!(
        discovery.catalog().skill("valid").unwrap().body(),
        "valid body"
    );
    assert_eq!(discovery.diagnostics().len(), 3);
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_skill_directories_without_reading_outside_the_root() {
    use std::os::unix::fs::symlink;

    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");
    let outside = temporary.path.join("outside");
    write_skill(
        &outside,
        "escaped",
        "---\nname: escaped\ndescription: outside root\n---\nbody\n",
    );
    fs::create_dir_all(&root).expect("root directory");
    symlink(outside.join("escaped"), root.join("escaped-link")).expect("skill symlink");
    write_skill(
        &root,
        "valid",
        "---\nname: valid\ndescription: in root\n---\nbody\n",
    );

    let discovery =
        SkillCatalog::discover(&root, temporary.path.join("missing")).expect("discover skills");

    assert_eq!(discovery.catalog().len(), 1);
    assert!(discovery.catalog().skill("escaped").is_none());
    assert_eq!(discovery.diagnostics().len(), 1);
    assert!(
        discovery.diagnostics()[0]
            .message()
            .contains("symbolic-link")
    );
}

#[test]
fn bounds_manifest_size_and_ignores_nested_manifests() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");
    write_skill(
        &root,
        "valid",
        "---\nname: valid\ndescription: in root\n---\nbody\n",
    );
    write_skill(
        &root.join("container"),
        "nested",
        "---\nname: nested\ndescription: too deep\n---\nbody\n",
    );
    let oversized = format!(
        "---\nname: oversized\ndescription: too large\n---\n{}",
        "x".repeat(256 * 1024)
    );
    write_skill(&root, "oversized", &oversized);

    let discovery =
        SkillCatalog::discover(&root, temporary.path.join("missing")).expect("discover skills");

    assert_eq!(discovery.catalog().len(), 1);
    assert!(discovery.catalog().skill("valid").is_some());
    assert!(discovery.catalog().skill("nested").is_none());
    assert_eq!(discovery.diagnostics().len(), 1);
    assert!(discovery.diagnostics()[0].message().contains("byte limit"));
}

fn write_skill(root: &Path, directory: &str, contents: &str) {
    let skill_directory = root.join(directory);
    fs::create_dir_all(&skill_directory).expect("skill directory");
    fs::write(skill_directory.join("SKILL.md"), contents).expect("skill manifest");
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("agens-skills-{timestamp}"));
        fs::create_dir_all(&path).expect("temporary directory");
        Self { path }
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

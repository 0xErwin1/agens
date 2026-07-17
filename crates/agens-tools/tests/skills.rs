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

#[test]
fn parses_yaml_quoted_folded_and_literal_descriptions() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");

    write_skill(
        &root,
        "quoted",
        "---\nname: quoted\ndescription: \"quoted: description\"\n---\nbody\n",
    );
    write_skill(
        &root,
        "folded",
        "---\nname: folded\ndescription: >\n  folded description\n  keeps its text\n---\nbody\n",
    );
    write_skill(
        &root,
        "literal",
        "---\nname: literal\ndescription: |\n  literal description\n  keeps its newline\n---\nbody\n",
    );

    let discovery =
        SkillCatalog::discover(&root, temporary.path.join("missing")).expect("discover skills");

    assert_eq!(discovery.catalog().len(), 3);
    assert_eq!(
        discovery.catalog().skill("quoted").unwrap().description(),
        "quoted: description"
    );
    assert_eq!(
        discovery.catalog().skill("folded").unwrap().description(),
        "folded description keeps its text"
    );
    assert_eq!(
        discovery.catalog().skill("literal").unwrap().description(),
        "literal description\nkeeps its newline"
    );
}

#[test]
fn rejects_unsafe_names_and_keeps_valid_siblings() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");

    for (directory, name) in [
        ("traversal", "../escape"),
        ("separator", "nested/name"),
        ("leading-hyphen", "-bad"),
        ("trailing-hyphen", "bad-"),
        ("consecutive-hyphens", "bad--hyphen"),
        ("uppercase", "Bad"),
    ] {
        write_skill(
            &root,
            directory,
            &format!("---\nname: {name}\ndescription: invalid name\n---\nbody\n"),
        );
    }
    write_skill(
        &root,
        "valid",
        "---\nname: valid-name-2\ndescription: valid\n---\nbody\n",
    );

    let discovery =
        SkillCatalog::discover(&root, temporary.path.join("missing")).expect("discover skills");

    assert_eq!(discovery.catalog().len(), 1);
    assert!(discovery.catalog().skill("valid-name-2").is_some());
    assert_eq!(discovery.diagnostics().len(), 6);
}

#[test]
fn ignores_unrelated_entries_without_consuming_the_skill_limit() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");

    for index in 0..128 {
        let name = format!("skill-{index:03}");
        write_skill(
            &root,
            &name,
            &format!("---\nname: {name}\ndescription: valid skill\n---\nbody\n"),
        );
    }
    fs::write(root.join("unrelated-file"), "not a skill").expect("unrelated file");
    fs::create_dir_all(root.join("unrelated-directory")).expect("unrelated directory");

    let discovery =
        SkillCatalog::discover(&root, temporary.path.join("missing")).expect("discover skills");

    assert_eq!(discovery.catalog().len(), 128);
    assert!(discovery.catalog().skill("skill-127").is_some());
    assert!(discovery.diagnostics().is_empty());
}

#[test]
fn isolates_duplicate_critical_fields_and_malformed_frontmatter_delimiters() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");

    write_skill(
        &root,
        "duplicate-name",
        "---\nname: duplicate\nname: repeated\ndescription: invalid\n---\nbody\n",
    );
    write_skill(
        &root,
        "non-string-description",
        "---\nname: typed\ndescription: [not, text]\n---\nbody\n",
    );
    write_skill(
        &root,
        "bad-opening-delimiter",
        "----\nname: malformed\ndescription: invalid\n---\nbody\n",
    );
    write_skill(
        &root,
        "missing-closing-delimiter",
        "---\nname: unclosed\ndescription: invalid\nbody\n",
    );
    write_skill(
        &root,
        "valid",
        "---\nname: valid\ndescription: usable\nmetadata:\n  author: test\n---\nbody\n",
    );

    let discovery =
        SkillCatalog::discover(&root, temporary.path.join("missing")).expect("discover skills");

    assert_eq!(discovery.catalog().len(), 1);
    assert!(discovery.catalog().skill("valid").is_some());
    assert_eq!(discovery.diagnostics().len(), 4);
}

#[test]
fn isolates_semantically_equivalent_quoted_critical_keys() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");

    for (directory, contents) in [
        (
            "double-quoted-name-first",
            "---\n\"name\" : first\nname: second\ndescription: valid\n---\nbody\n",
        ),
        (
            "double-quoted-name-second",
            "---\nname: first\n  \"name\"  : second\ndescription: valid\n---\nbody\n",
        ),
        (
            "single-quoted-name-first",
            "---\n'name' : first\nname: second\ndescription: valid\n---\nbody\n",
        ),
        (
            "single-quoted-name-second",
            "---\nname: first\n  'name'  : second\ndescription: valid\n---\nbody\n",
        ),
        (
            "double-quoted-description-first",
            "---\nname: valid\n\"description\" : >\n  folded first description\ndescription: second description\n---\nbody\n",
        ),
        (
            "double-quoted-description-second",
            "---\nname: valid\ndescription: first description\n  \"description\"  : >\n    folded second description\n---\nbody\n",
        ),
        (
            "single-quoted-description-first",
            "---\nname: valid\n'description' : >\n  folded first description\ndescription: second description\n---\nbody\n",
        ),
        (
            "single-quoted-description-second",
            "---\nname: valid\ndescription: first description\n  'description'  : >\n    folded second description\n---\nbody\n",
        ),
    ] {
        write_skill(&root, directory, contents);
    }
    write_skill(
        &root,
        "valid",
        "---\n\"name\": valid\n'description': >\n  valid folded description\n---\nbody\n",
    );

    let discovery =
        SkillCatalog::discover(&root, temporary.path.join("missing")).expect("discover skills");

    assert_eq!(discovery.catalog().len(), 1);
    assert_eq!(
        discovery.catalog().skill("valid").unwrap().description(),
        "valid folded description"
    );
    assert_eq!(discovery.diagnostics().len(), 8);
}

#[test]
fn rejects_non_string_critical_metadata_values_and_keeps_yaml_string_scalars() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");

    for (directory, value) in [
        ("numeric-name", "123"),
        ("boolean-name", "true"),
        ("null-name", "null"),
        ("sequence-name", "[not, text]"),
        ("mapping-name", "{value: not-text}"),
    ] {
        write_skill(
            &root,
            directory,
            &format!("---\nname: {value}\ndescription: valid\n---\nbody\n"),
        );
    }

    for (directory, value) in [
        ("numeric-description", "123"),
        ("boolean-description", "true"),
        ("null-description", "null"),
        ("sequence-description", "[not, text]"),
        ("mapping-description", "{value: not-text}"),
    ] {
        write_skill(
            &root,
            directory,
            &format!("---\nname: {directory}\ndescription: {value}\n---\nbody\n"),
        );
    }

    write_skill(
        &root,
        "quoted",
        "---\nname: \"quoted\"\ndescription: 'quoted description'\n---\nbody\n",
    );
    write_skill(
        &root,
        "folded",
        "---\nname: folded\ndescription: >\n  folded description\n  remains text\n---\nbody\n",
    );
    write_skill(
        &root,
        "literal",
        "---\nname: literal\ndescription: |\n  literal description\n  remains text\n---\nbody\n",
    );

    let discovery =
        SkillCatalog::discover(&root, temporary.path.join("missing")).expect("discover skills");

    assert_eq!(discovery.catalog().len(), 3);
    assert!(discovery.catalog().skill("quoted").is_some());
    assert_eq!(
        discovery.catalog().skill("folded").unwrap().description(),
        "folded description remains text"
    );
    assert_eq!(
        discovery.catalog().skill("literal").unwrap().description(),
        "literal description\nremains text"
    );
    assert_eq!(discovery.diagnostics().len(), 10);
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_roots_and_manifests_while_loading_valid_siblings() {
    use std::os::unix::fs::symlink;

    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");
    let outside = temporary.path.join("outside");
    write_skill(
        &outside,
        "escaped",
        "---\nname: escaped\ndescription: outside root\n---\nbody\n",
    );
    fs::create_dir_all(root.join("manifest-link")).expect("linked manifest directory");
    symlink(
        outside.join("escaped").join("SKILL.md"),
        root.join("manifest-link").join("SKILL.md"),
    )
    .expect("manifest symlink");
    write_skill(
        &root,
        "valid",
        "---\nname: valid\ndescription: in root\n---\nbody\n",
    );

    let discovery =
        SkillCatalog::discover(&root, temporary.path.join("missing")).expect("discover skills");

    assert_eq!(discovery.catalog().len(), 1);
    assert!(discovery.catalog().skill("valid").is_some());
    assert_eq!(discovery.diagnostics().len(), 1);
    assert!(
        discovery.diagnostics()[0]
            .message()
            .contains("symbolic-link")
    );

    let root_link = temporary.path.join("root-link");
    symlink(&root, &root_link).expect("root symlink");
    assert!(SkillCatalog::discover(&root_link, temporary.path.join("missing")).is_err());
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

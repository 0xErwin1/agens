use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use agens_core::{AgentDefinition, AgentMode};
use agens_tools::{
    AgentCatalog,
    markdown::{self, FrontmatterValue, MarkdownRoot},
};

#[test]
fn parses_scalar_and_list_frontmatter_without_changing_the_body() {
    let document =
        markdown::parse("---\nname: example\nskills:\n  - read\n  - write\n---\n  body remains\n")
            .expect("parse markdown");

    assert_eq!(document.body(), "  body remains\n");
    assert_eq!(
        document.field("name"),
        Some(&FrontmatterValue::Scalar("example".into()))
    );
    assert_eq!(
        document.field("skills"),
        Some(&FrontmatterValue::List(vec!["read".into(), "write".into()]))
    );
}

#[test]
fn rejects_malformed_frontmatter_and_unsafe_definition_names() {
    assert!(markdown::parse("name: missing delimiters\n").is_err());
    assert!(markdown::parse("---\nname: {not: text}\n---\nbody\n").is_err());
    assert!(markdown::canonical_filename("bad--name").is_err());
    assert!(markdown::canonical_filename("../escape").is_err());
    assert_eq!(
        markdown::canonical_filename("valid-name-2").unwrap(),
        "valid-name-2.md"
    );
}

#[test]
fn bounds_and_confines_root_reads_with_isolated_diagnostics() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("valid.md"), "---\nname: valid\n---\nbody\n").unwrap();
    fs::write(root.join("bad.md"), "---\nname: {bad: value}\n---\nbody\n").unwrap();
    fs::write(
        root.join("large.md"),
        "x".repeat(markdown::MAX_MARKDOWN_FILE_BYTES + 1),
    )
    .unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(temporary.path.join("outside.md"), root.join("escape.md")).unwrap();

    let MarkdownRoot {
        documents,
        diagnostics,
    } = markdown::load_root(&root).unwrap();
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0].name(), "valid");
    assert_eq!(diagnostics.len(), if cfg!(unix) { 3 } else { 2 });
}

#[test]
fn stops_at_root_and_accepted_definition_limits() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");
    fs::create_dir_all(&root).unwrap();
    for index in 0..=markdown::MAX_MARKDOWN_ROOT_ENTRIES {
        let name = format!("entry-{index:04}");
        fs::write(
            root.join(format!("{name}.md")),
            format!("---\nname: {name}\n---\nbody\n"),
        )
        .unwrap();
    }

    let MarkdownRoot {
        documents,
        diagnostics,
    } = markdown::load_root(&root).unwrap();
    assert_eq!(documents.len(), markdown::MAX_MARKDOWN_DEFINITIONS);
    assert_eq!(diagnostics.len(), 2);
}

#[test]
fn accepts_exact_definition_limit_while_reporting_later_invalid_entries() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");
    fs::create_dir_all(&root).unwrap();

    for index in 0..markdown::MAX_MARKDOWN_DEFINITIONS {
        let name = format!("definition-{index:03}");
        fs::write(
            root.join(format!("{name}.md")),
            format!("---\nname: {name}\n---\nbody\n"),
        )
        .unwrap();
    }
    fs::write(root.join("unrelated.txt"), "ignored").unwrap();
    fs::write(
        root.join("z-malformed.md"),
        "---\nname: {not: text}\n---\nbody\n",
    )
    .unwrap();
    fs::write(root.join("z-not-utf8.md"), [0xff, 0xfe]).unwrap();

    let MarkdownRoot {
        documents,
        diagnostics,
    } = markdown::load_root(&root).unwrap();

    assert_eq!(documents.len(), markdown::MAX_MARKDOWN_DEFINITIONS);
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message())
            .collect::<Vec<_>>(),
        vec![
            "frontmatter field name must be a string or string list",
            "file is not UTF-8: invalid utf-8 sequence of 1 bytes from index 0",
        ]
    );
}

#[test]
fn rejects_extra_definitions_once_without_hiding_later_diagnostics() {
    let temporary = TemporaryDirectory::new();
    let root = temporary.path.join("root");
    fs::create_dir_all(&root).unwrap();

    for index in 0..=markdown::MAX_MARKDOWN_DEFINITIONS {
        let name = format!("definition-{index:03}");
        fs::write(
            root.join(format!("{name}.md")),
            format!("---\nname: {name}\n---\nbody\n"),
        )
        .unwrap();
    }
    fs::write(root.join("z-invalid.md"), [0xff]).unwrap();

    let MarkdownRoot {
        documents,
        diagnostics,
    } = markdown::load_root(&root).unwrap();

    assert_eq!(documents.len(), markdown::MAX_MARKDOWN_DEFINITIONS);
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message())
            .collect::<Vec<_>>(),
        vec![
            "accepted definition limit exceeded",
            "file is not UTF-8: invalid utf-8 sequence of 1 bytes from index 0",
        ]
    );
}

#[cfg(unix)]
#[test]
fn rejects_a_symbolic_link_root_even_when_it_points_outside_the_confinement() {
    let temporary = TemporaryDirectory::new();
    let outside = temporary.path.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(
        outside.join("outside.md"),
        "---\nname: outside\n---\nbody\n",
    )
    .unwrap();
    let root = temporary.path.join("root");
    std::os::unix::fs::symlink(&outside, &root).unwrap();

    assert_eq!(
        markdown::load_root(&root),
        Err("markdown root must be a non-symbolic-link directory".into())
    );
}

#[test]
fn discovers_agents_with_deterministic_precedence_modes_and_diagnostics() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    let project = temporary.path.join("project");
    fs::create_dir_all(&global).unwrap();
    fs::create_dir_all(&project).unwrap();
    write_agent(&global, "shared", "global", "all");
    write_agent(&global, "worker", "worker", "subagent");
    write_agent(&project, "shared", "project", "primary");
    fs::write(project.join("broken.md"), "---\nname: broken\n---\n").unwrap();

    let built_in = AgentDefinition {
        name: "shared".into(),
        description: "built-in".into(),
        mode: AgentMode::Primary,
        model: None,
        system_prompt: "built-in".into(),
        permission_rules: vec![],
        skills: vec![],
    };
    let discovery = AgentCatalog::discover(&[built_in], &global, &project).unwrap();

    assert_eq!(
        discovery.catalog().agent("shared").unwrap().description,
        "project"
    );
    assert_eq!(discovery.catalog().primary_or_all().count(), 1);
    assert_eq!(discovery.catalog().subagents().count(), 1);
    assert_eq!(discovery.shadowed().len(), 2);
    assert_eq!(discovery.diagnostics().len(), 1);
}

#[test]
fn isolates_unsafe_mismatched_and_oversized_agent_documents() {
    let temporary = TemporaryDirectory::new();
    let global = temporary.path.join("global");
    let project = temporary.path.join("project");
    fs::create_dir_all(&global).unwrap();
    fs::create_dir_all(&project).unwrap();
    write_agent(&global, "duplicate", "first", "primary");
    fs::write(
        global.join("duplicate-copy.md"),
        "---\nname: duplicate\ndescription: second\nmode: primary\n---\nbody\n",
    )
    .unwrap();
    fs::write(global.join("model.md"), "---\nname: model\ndescription: model\nmode: all\nmodel: unknown\nskills:\n  - skill\npermissions:\n  - allow native::read\n---\nbody\n").unwrap();
    fs::write(
        project.join("large.md"),
        "x".repeat(markdown::MAX_MARKDOWN_FILE_BYTES + 1),
    )
    .unwrap();

    let duplicate = AgentDefinition {
        name: "duplicate".into(),
        description: "built-in".into(),
        mode: AgentMode::Primary,
        model: None,
        system_prompt: "built-in".into(),
        permission_rules: vec![],
        skills: vec![],
    };
    let discovery =
        AgentCatalog::discover(&[duplicate.clone(), duplicate], &global, &project).unwrap();

    assert_eq!(
        discovery.catalog().agent("duplicate").unwrap().description,
        "first"
    );
    let model = discovery.catalog().agent("model").unwrap();
    assert_eq!(model.model.as_deref(), Some("unknown"));
    assert_eq!(model.system_prompt, "body");
    assert_eq!(model.permission_rules.len(), 1);
    assert_eq!(discovery.diagnostics().len(), 4);
}

fn write_agent(root: &std::path::Path, name: &str, description: &str, mode: &str) {
    fs::write(root.join(format!("{name}.md")), format!("---\nname: {name}\ndescription: {description}\nmode: {mode}\npermissions: []\n---\nbody\n")).unwrap();
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new() -> Self {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("agens-extensions-{name}"));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

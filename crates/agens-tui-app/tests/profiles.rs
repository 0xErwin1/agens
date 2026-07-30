use agens_agents::ProfileOrigin;
use agens_tui_app::profiles::{CycleDirection, ProfileEditor, ProfileEditorRow, ProfileScope};

fn row(name: &str, effort: Option<&str>) -> ProfileEditorRow {
    ProfileEditorRow::new(
        name,
        "session-model",
        ProfileOrigin::SessionInherited,
        effort,
        ProfileOrigin::Frontmatter,
        false,
    )
}

#[test]
fn toggle_scope_switches_between_global_and_project() {
    let mut editor = ProfileEditor::new(vec![row("explore", None)]);
    assert_eq!(editor.scope(), ProfileScope::Global);
    editor.toggle_scope();
    assert_eq!(editor.scope(), ProfileScope::Project);
    editor.toggle_scope();
    assert_eq!(editor.scope(), ProfileScope::Global);
}

#[test]
fn effort_after_none_cycles_to_each_end() {
    let editor = ProfileEditor::new(vec![row("explore", None)]);
    assert_eq!(
        editor.effort_after("explore", CycleDirection::Next),
        Some(Some("none".into()))
    );
    assert_eq!(
        editor.effort_after("explore", CycleDirection::Prev),
        Some(Some("max".into()))
    );
}

#[test]
fn effort_after_wraps_and_steps_through_the_sequence() {
    let editor = ProfileEditor::new(vec![
        row("first", Some("max")),
        row("last", Some("none")),
        row("middle", Some("medium")),
    ]);
    assert_eq!(
        editor.effort_after("first", CycleDirection::Next),
        Some(Some("none".into()))
    );
    assert_eq!(
        editor.effort_after("last", CycleDirection::Prev),
        Some(Some("max".into()))
    );
    assert_eq!(
        editor.effort_after("middle", CycleDirection::Next),
        Some(Some("high".into()))
    );
    assert_eq!(
        editor.effort_after("middle", CycleDirection::Prev),
        Some(Some("low".into()))
    );
    assert_eq!(editor.effort_after("unknown", CycleDirection::Next), None);
}

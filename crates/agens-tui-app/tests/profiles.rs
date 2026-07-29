use agens_agents::ProfileOrigin;
use agens_tui_app::profiles::{ProfileEditor, ProfileEditorRow, ProfileScope};

fn row(name: &str, model: &str, effort: Option<&str>, unavailable: bool) -> ProfileEditorRow {
    ProfileEditorRow::new(
        name,
        model,
        ProfileOrigin::SessionInherited,
        effort,
        ProfileOrigin::Frontmatter,
        unavailable,
    )
}

#[test]
fn staged_edits_are_scoped_and_cancel_discards_them() {
    let mut editor = ProfileEditor::new(vec![row("explore", "session-model", Some("low"), false)]);
    editor.set_scope(ProfileScope::Project);
    editor.set_model("explore", "catalog-model");
    editor.set_effort("explore", "high");

    assert_eq!(editor.rows()[0].model.value, "catalog-model");
    assert_eq!(editor.rows()[0].model.origin, ProfileOrigin::ProjectProfile);
    assert_eq!(editor.rows()[0].effort.value.as_deref(), Some("high"));
    assert_eq!(editor.scope(), ProfileScope::Project);

    editor.cancel();
    assert_eq!(editor.rows()[0].model.value, "session-model");
    assert_eq!(editor.rows()[0].effort.value.as_deref(), Some("low"));
    assert!(editor.patches().is_empty());
}

#[test]
fn edits_in_each_scope_remain_independently_staged() {
    let mut editor = ProfileEditor::new(vec![row("explore", "session-model", None, false)]);
    editor.set_model("explore", "global-model");
    editor.set_scope(ProfileScope::Project);
    editor.set_effort("explore", "high");

    assert_eq!(editor.patches_for(ProfileScope::Global).count(), 1);
    assert_eq!(editor.patches_for(ProfileScope::Project).count(), 1);
}

#[test]
fn reset_previews_the_next_precedence_value_and_keeps_unavailable_rows_editable() {
    let mut editor = ProfileEditor::new(vec![row("review", "missing-model", None, true)]);
    editor.set_scope(ProfileScope::Global);
    editor.set_model("review", "available-model");
    editor.reset_model("review");

    assert!(editor.rows()[0].unavailable);
    assert_eq!(editor.rows()[0].model.value, "missing-model");
    assert_eq!(editor.patches()[0].model, Some(None));
}

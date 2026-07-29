use agens_tui::{Action, Event, Key, Tui};

struct Engine;
impl agens_tui::Engine for Engine {
    fn cancel(&mut self) {}
}

#[test]
fn ctrl_shift_p_opens_the_subagent_profiles_route() {
    let mut tui = Tui::new(Engine);
    assert_eq!(
        tui.handle(Event::Key(Key::CtrlShiftP)),
        Action::OpenDialog("subagent-profiles".into())
    );
}

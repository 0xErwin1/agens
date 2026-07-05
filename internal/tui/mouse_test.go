package tui

import "testing"

func TestModel_SelectTogglesMouseReporting(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	if !m.mouseEnabled {
		t.Fatal("mouse should start enabled so the wheel scrolls the conversation")
	}

	cmd := m.ToggleMouse()
	if m.mouseEnabled {
		t.Fatal("ToggleMouse should turn mouse off so native text selection works")
	}
	if cmd == nil {
		t.Fatal("ToggleMouse should return the command that disables mouse reporting")
	}

	cmd = m.ToggleMouse()
	if !m.mouseEnabled {
		t.Fatal("a second ToggleMouse should turn mouse back on")
	}
	if cmd == nil {
		t.Fatal("ToggleMouse should return the command that re-enables mouse reporting")
	}
}

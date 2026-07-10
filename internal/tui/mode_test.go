package tui

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/0xErwin1/agens/internal/permission"
)

func modeModel(t *testing.T, initial permission.Mode) (*Model, *permission.ModeState) {
	t.Helper()
	state := permission.NewModeState(initial)
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5", Mode: state})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})
	return m, state
}

func TestModel_ToggleModeFlipsEditToChatAndBack(t *testing.T) {
	m, state := modeModel(t, permission.ModeEdit)

	m.ToggleMode("")
	if state.Get() != permission.ModeChat {
		t.Fatalf("state.Get() = %v after toggle, want ModeChat", state.Get())
	}
	if m.status.mode != "chat" {
		t.Fatalf("status.mode = %q, want %q", m.status.mode, "chat")
	}

	m.ToggleMode("")
	if state.Get() != permission.ModeEdit {
		t.Fatalf("state.Get() = %v after second toggle, want ModeEdit", state.Get())
	}
	if m.status.mode != "" {
		t.Fatalf("status.mode = %q, want it hidden in edit mode", m.status.mode)
	}
}

func TestModel_ToggleModeAcceptsExplicitArgument(t *testing.T) {
	m, state := modeModel(t, permission.ModeEdit)

	m.ToggleMode("chat")
	if state.Get() != permission.ModeChat {
		t.Fatalf("state.Get() = %v, want ModeChat after /mode chat", state.Get())
	}

	m.ToggleMode("edit")
	if state.Get() != permission.ModeEdit {
		t.Fatalf("state.Get() = %v, want ModeEdit after /mode edit", state.Get())
	}
}

func TestModel_ToggleModeRejectsUnknownArgument(t *testing.T) {
	m, state := modeModel(t, permission.ModeEdit)

	m.ToggleMode("bogus")
	if state.Get() != permission.ModeEdit {
		t.Fatalf("state.Get() = %v, want unchanged ModeEdit for an unrecognized argument", state.Get())
	}
}

func TestModel_ToggleModeWithoutModeStateNotesUnavailable(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	m.ToggleMode("")

	view := stripANSI(m.messages.View())
	if !strings.Contains(view, "not available") {
		t.Fatalf("view = %q, want a note that mode switching is unavailable", view)
	}
}

func TestModel_ModeCommandTogglesLiveMode(t *testing.T) {
	m, state := modeModel(t, permission.ModeEdit)

	c, ok := m.commands.Lookup("/mode chat")
	if !ok {
		t.Fatal("/mode command not registered")
	}
	c.Run(m, "/mode chat")

	if state.Get() != permission.ModeChat {
		t.Fatalf("state.Get() = %v after /mode chat command, want ModeChat", state.Get())
	}
}

func TestModel_StatusShowsInitialModeFromDeps(t *testing.T) {
	m, _ := modeModel(t, permission.ModeChat)
	if m.status.mode != "chat" {
		t.Fatalf("status.mode = %q, want %q for a session started in chat mode", m.status.mode, "chat")
	}
}

package tui

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/0xErwin1/agens/internal/permission"
)

func bypassModel(t *testing.T, initial bool) (*Model, *permission.BypassState) {
	t.Helper()

	state := permission.NewBypassState(initial)
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5", Bypass: state})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})

	return m, state
}

func TestModel_SetBypassChangesStateAndNotifies(t *testing.T) {
	m, state := bypassModel(t, false)

	m.SetBypass("on")
	if !state.Enabled() {
		t.Fatal("bypass state is disabled after /bypass on")
	}
	if !m.status.bypass {
		t.Fatal("status bypass warning is hidden after /bypass on")
	}
	if !strings.Contains(stripANSI(m.messages.View()), "bypass enabled") {
		t.Fatalf("messages = %q, want an enabled notification", stripANSI(m.messages.View()))
	}

	m.SetBypass("off")
	if state.Enabled() {
		t.Fatal("bypass state is enabled after /bypass off")
	}
	if m.status.bypass {
		t.Fatal("status bypass warning is visible after /bypass off")
	}
	if !strings.Contains(stripANSI(m.messages.View()), "bypass disabled") {
		t.Fatalf("messages = %q, want a disabled notification", stripANSI(m.messages.View()))
	}
}

func TestModel_SetBypassRejectsInvalidArgument(t *testing.T) {
	m, state := bypassModel(t, false)

	m.SetBypass("maybe")

	if state.Enabled() {
		t.Fatal("invalid bypass argument changed the state")
	}
	if !strings.Contains(stripANSI(m.messages.View()), "usage: /bypass on|off") {
		t.Fatalf("messages = %q, want bypass usage", stripANSI(m.messages.View()))
	}
}

func TestModel_BypassCommandIsRegisteredAndDocumented(t *testing.T) {
	m, state := bypassModel(t, false)

	c, ok := m.commands.Lookup("/bypass on")
	if !ok {
		t.Fatal("/bypass command is not registered")
	}
	if c.SafeWhileRunning {
		t.Fatal("/bypass must not be safe while a turn is running")
	}
	if !strings.Contains(m.CommandHelp(), "/bypass") {
		t.Fatalf("help = %q, want /bypass", m.CommandHelp())
	}

	c.Run(m, "/bypass on")
	if !state.Enabled() {
		t.Fatal("/bypass on command did not enable the shared state")
	}
}

func TestModel_BypassCommandIsRejectedWhileRunning(t *testing.T) {
	for _, tc := range []struct {
		name      string
		initial   bool
		requested string
	}{
		{name: "off to on", initial: false, requested: "on"},
		{name: "on to off", initial: true, requested: "off"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			m, state := bypassModel(t, tc.initial)
			m.running = true

			typeString(m, "/bypass "+tc.requested)
			sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

			if state.Enabled() != tc.initial {
				t.Fatalf("bypass state = %t after rejected command, want %t", state.Enabled(), tc.initial)
			}
			if !strings.Contains(stripANSI(m.messages.View()), "not available while a turn is running") {
				t.Fatalf("messages = %q, want a running-turn rejection", stripANSI(m.messages.View()))
			}
		})
	}
}

func TestStatus_ViewShowsBypassWarningOnlyWhenEnabled(t *testing.T) {
	s := NewStatus("gpt-5.5")
	s.SetSize(80, 1)

	s.SetBypass(true)
	if !strings.Contains(stripANSI(s.View()), "BYPASS") {
		t.Fatalf("View() = %q, want active bypass warning", stripANSI(s.View()))
	}

	s.SetBypass(false)
	if strings.Contains(stripANSI(s.View()), "BYPASS") {
		t.Fatalf("View() = %q, must hide inactive bypass warning", stripANSI(s.View()))
	}
}

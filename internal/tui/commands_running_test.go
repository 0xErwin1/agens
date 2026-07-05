package tui

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
)

func TestRegistryMatchRunnableFiltersToSafeWhileRunning(t *testing.T) {
	r := defaultCommands()

	idle := r.MatchRunnable("/", false)
	if len(idle) != len(r.All()) {
		t.Fatalf("MatchRunnable(idle) = %d, want all %d", len(idle), len(r.All()))
	}

	running := r.MatchRunnable("/", true)
	if len(running) == 0 || len(running) >= len(idle) {
		t.Fatalf("MatchRunnable(running) = %d, want a nonempty strict subset of %d", len(running), len(idle))
	}
	for _, c := range running {
		if !c.SafeWhileRunning {
			t.Fatalf("MatchRunnable(running) offered %q, want only safe-while-running commands", c.Name)
		}
	}
}

func TestModel_UnsafeCommandBlockedWhileRunning(t *testing.T) {
	m := runningModel(t)

	typeString(m, "/model")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if m.modelPickerOpen {
		t.Fatal("/model must not open the selector while a turn runs")
	}
	if !strings.Contains(stripANSI(m.View()), "not available while a turn is running") {
		t.Fatalf("View() = %q, want a note that the command is blocked mid-turn", stripANSI(m.View()))
	}
}

func TestModel_SafeCommandRunsWhileRunning(t *testing.T) {
	m := runningModel(t)

	typeString(m, "/select")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if m.mouseEnabled {
		t.Fatal("/select should run while a turn is running and toggle mouse off")
	}
}

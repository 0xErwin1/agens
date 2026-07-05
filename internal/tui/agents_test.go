package tui

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/iperez/agens/internal/agentdef"
	"github.com/iperez/agens/internal/provider"
)

// agentsModel builds a model wired with the built-in agents, a two-model
// catalog, and projectRoot as the destination for saved definition files.
func agentsModel(t *testing.T, projectRoot string) *Model {
	t.Helper()
	defs, err := agentdef.Load("", "")
	if err != nil {
		t.Fatalf("agentdef.Load() error = %v", err)
	}
	m := New(Deps{
		Loop:    &scriptedLoopRunner{},
		Model:   "gpt-5.5",
		Models:  fakeLister{models: []provider.ModelInfo{{ID: "gpt-5.5"}, {ID: "gpt-4.1"}}},
		Agents:  defs,
		Project: projectRoot,
	})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})
	return m
}

func TestModel_AgentsMenuTogglesAndSavesModels(t *testing.T) {
	root := t.TempDir()
	m := agentsModel(t, root)

	cmd := m.OpenAgentMenu()
	if !m.agentMenuOpen {
		t.Fatal("OpenAgentMenu did not open the menu")
	}
	if cmd != nil {
		m.Update(cmd()) // deliver the model catalog
	}

	list := stripANSI(m.View())
	if !strings.Contains(list, "build") || !strings.Contains(list, "plan") {
		t.Fatalf("agent list = %q, want the built-in agents", list)
	}

	// Enter the highlighted agent (build) → the model editor.
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})
	if m.agentMenuEditing != "build" {
		t.Fatalf("editing = %q, want build", m.agentMenuEditing)
	}
	editor := stripANSI(m.View())
	if !strings.Contains(editor, "gpt-5.5") || !strings.Contains(editor, "gpt-4.1") {
		t.Fatalf("model editor = %q, want the served models as toggles", editor)
	}

	// Toggle the highlighted model (gpt-5.5) on and save.
	sendKey(m, tea.KeyMsg{Type: tea.KeySpace})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if m.agentMenuOpen {
		t.Fatal("menu still open after save, want it closed")
	}

	path := filepath.Join(root, ".agens", "agents", "build.md")
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read saved agent file: %v", err)
	}
	if !strings.Contains(string(data), "gpt-5.5") {
		t.Fatalf("saved file = %q, want the toggled model persisted", string(data))
	}
	if strings.Contains(string(data), "gpt-4.1") {
		t.Fatalf("saved file = %q, want only the checked model, not the unchecked one", string(data))
	}

	def, _ := m.agents.ByName("build")
	if len(def.Models) != 1 || def.Models[0] != "gpt-5.5" {
		t.Fatalf("in-memory build.Models = %v, want [gpt-5.5]", def.Models)
	}
}

func TestModel_AgentsMenuEscNavigatesLevels(t *testing.T) {
	m := agentsModel(t, t.TempDir())

	if cmd := m.OpenAgentMenu(); cmd != nil {
		m.Update(cmd())
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter}) // into the model editor
	if m.agentMenuEditing == "" {
		t.Fatal("Enter did not enter the model editor")
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyEsc}) // back to the list
	if m.agentMenuEditing != "" {
		t.Fatal("Esc in the editor did not return to the agent list")
	}
	if !m.agentMenuOpen {
		t.Fatal("Esc in the editor closed the whole menu, want it back at the list")
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyEsc}) // close
	if m.agentMenuOpen {
		t.Fatal("Esc at the list did not close the menu")
	}
}

func TestModel_AgentsMenuUnavailableWithoutAgents(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5") // no Agents wired

	if cmd := m.OpenAgentMenu(); cmd != nil {
		t.Fatal("OpenAgentMenu returned a command with no agents, want a no-op note")
	}
	if m.agentMenuOpen {
		t.Fatal("menu opened with no agents available")
	}
}

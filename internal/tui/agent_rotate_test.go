package tui

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/iperez/agens/internal/agentdef"
	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
	"github.com/iperez/agens/internal/session"
)

// rotateModel builds a model wired with the built-in primary agents (default,
// build, plan) and an agent-prompt builder that encodes the persona and model it
// was called with, so a test can assert which persona reached the loop.
func rotateModel(t *testing.T) (*Model, *scriptedLoopRunner) {
	t.Helper()
	defs, err := agentdef.Load("", "")
	if err != nil {
		t.Fatalf("agentdef.Load() error = %v", err)
	}
	loop := &scriptedLoopRunner{}
	m := New(Deps{
		Loop:   loop,
		Model:  "gpt-5.5",
		Agents: defs,
		AgentPrompt: func(persona, model string) (string, bool) {
			return "PROMPT[" + persona + "|" + model + "]", true
		},
	})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})
	return m, loop
}

func textOf(msg message.Message) string {
	var b strings.Builder
	for _, part := range msg.Parts {
		if text, ok := part.(message.TextPart); ok {
			b.WriteString(text.Text)
		}
	}
	return b.String()
}

func TestModel_TabCyclesActiveAgentAndPersona(t *testing.T) {
	m, loop := rotateModel(t)

	if m.activeAgentName() != defaultAgentName {
		t.Fatalf("initial agent = %q, want default", m.activeAgentName())
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyTab})
	if m.activeAgentName() != "build" {
		t.Fatalf("after one tab = %q, want build", m.activeAgentName())
	}
	if !strings.Contains(loop.systemPrompt, "hands-on engineering") {
		t.Fatalf("loop prompt = %q, want it rebuilt with the build persona", loop.systemPrompt)
	}
	if m.status.agent != "build" {
		t.Fatalf("status agent = %q, want build", m.status.agent)
	}
	if m.pendingAgentNote == "" {
		t.Fatal("no pending switch note queued after a tab")
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyTab})
	if m.activeAgentName() != "plan" {
		t.Fatalf("after two tabs = %q, want plan", m.activeAgentName())
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyTab})
	if m.activeAgentName() != defaultAgentName {
		t.Fatalf("after three tabs = %q, want it wrapped back to default", m.activeAgentName())
	}
	if m.status.agent != "" {
		t.Fatalf("status agent = %q, want it hidden on the default agent", m.status.agent)
	}
}

func TestModel_SwitchNotePrependedToNextMessageOnly(t *testing.T) {
	m, _ := rotateModel(t)

	sendKey(m, tea.KeyMsg{Type: tea.KeyTab}) // → build, queues the note
	m.submitText("do the thing")

	last := m.history[len(m.history)-1]
	text := textOf(last)
	if !strings.Contains(text, "system-reminder") || !strings.Contains(text, "build") {
		t.Fatalf("model message = %q, want the synthetic switch note prepended", text)
	}
	if !strings.Contains(text, "do the thing") {
		t.Fatalf("model message = %q, want the user's text kept", text)
	}
	if m.pendingAgentNote != "" {
		t.Fatal("pending note was not cleared after it was sent")
	}

	// The conversation shows the user's text, not the synthetic note.
	view := stripANSI(m.messages.View())
	if strings.Contains(view, "system-reminder") {
		t.Fatalf("view = %q, want the switch note hidden from the visible bubble", view)
	}
}

func TestModel_AgentPickerSwitches(t *testing.T) {
	m, _ := rotateModel(t)

	if cmd := m.OpenAgentPicker(); cmd != nil {
		t.Fatal("OpenAgentPicker returned a command, want a synchronous open")
	}
	if !m.agentPickerOpen {
		t.Fatal("picker did not open")
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyDown})  // default → build
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter}) // switch
	if m.activeAgentName() != "build" {
		t.Fatalf("picker selection = %q, want build", m.activeAgentName())
	}
	if m.agentPickerOpen {
		t.Fatal("picker stayed open after a selection")
	}
}

func TestModel_ModelSwitchKeepsActiveAgentPersona(t *testing.T) {
	m, loop := rotateModel(t)

	sendKey(m, tea.KeyMsg{Type: tea.KeyTab}) // → build
	m.selectModel(provider.ModelInfo{ID: "gpt-4.1"})

	if !strings.Contains(loop.systemPrompt, "hands-on engineering") {
		t.Fatalf("prompt after model switch = %q, want the build persona kept", loop.systemPrompt)
	}
	if !strings.Contains(loop.systemPrompt, "gpt-4.1") {
		t.Fatalf("prompt after model switch = %q, want the new model id", loop.systemPrompt)
	}
}

func TestModel_ResumeRestoresActiveAgent(t *testing.T) {
	m, loop := rotateModel(t)

	m.applyResumedSession(session.Session{
		ID:       "s1",
		Agent:    "plan",
		Messages: []message.Message{message.NewMessage(message.RoleUser, message.TextPart{Text: "hi"})},
	})

	if m.activeAgentName() != "plan" {
		t.Fatalf("resumed agent = %q, want plan", m.activeAgentName())
	}
	if m.status.agent != "plan" {
		t.Fatalf("status agent = %q, want plan after resume", m.status.agent)
	}
	if !strings.Contains(loop.systemPrompt, "planning subagent") {
		t.Fatalf("prompt after resume = %q, want the plan persona applied", loop.systemPrompt)
	}
}

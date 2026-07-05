package tui

import (
	"encoding/json"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/message"
)

func TestModel_CtrlOInFocusExpandsSubagentConversation(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	m.messages.StartSubagent("a", "", "explore", "gpt-5.5", "task")
	td := message.NewMessage(message.RoleAssistant, message.ToolUsePart{ID: "t1", Name: "read", Input: json.RawMessage(`{"path":"x.go"}`)})
	m.messages.ApplySubagentStream("a", agentloop.LoopEvent{Kind: agentloop.LoopMessageDone, Message: &td})
	m.messages.ApplySubagentStream("a", agentloop.LoopEvent{Kind: agentloop.LoopToolResult, ToolResult: message.ToolResultPart{ToolUseID: "t1", Content: message.Parts{message.TextPart{Text: "UNIQUE_RESULT"}}}})
	m.subagentFocusID = "a"

	if strings.Contains(stripANSI(m.View()), "UNIQUE_RESULT") {
		t.Fatal("the subagent tool result should be collapsed before Ctrl+O")
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlO})

	if !strings.Contains(stripANSI(m.View()), "UNIQUE_RESULT") {
		t.Fatal("Ctrl+O while entered did not expand the subagent's tool result (must behave like the main thread)")
	}
}

func TestMessages_FinishedCollapsedSubagentIsJustDone(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartSubagent("a", "", "subagent", "gpt-5.5", "Investiga el proyecto y devuelve un informe")
	m.CompleteSubagent("a", false, 0)

	view := stripANSI(m.View())
	if !strings.Contains(view, "done") {
		t.Fatalf("view = %q, want the done marker on the finished panel", view)
	}
	if strings.Contains(view, "task:") {
		t.Fatalf("view = %q, want no task prompt on a finished, collapsed panel", view)
	}

	m.ToggleDetails()
	if !strings.Contains(stripANSI(m.View()), "Investiga el proyecto") {
		t.Fatal("expanding a finished panel should bring back the task prompt")
	}
}

func TestMessages_ResumeRebuildsTaskAsSubagentPanel(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	history := []message.Message{
		message.NewMessage(message.RoleUser, message.TextPart{Text: "lanza un subagente"}),
		message.NewMessage(message.RoleAssistant, message.ToolUsePart{ID: "call_1", Name: "task", Input: json.RawMessage(`{"description":"Explora el repositorio"}`)}),
		message.NewMessage(message.RoleUser, message.ToolResultPart{ToolUseID: "call_1", Content: message.Parts{message.TextPart{Text: "El proyecto es un CLI en Go."}}}),
	}
	m.SetHistory(history)

	sub := m.findSubagent("call_1")
	if sub == nil {
		t.Fatal("SetHistory did not rebuild the task delegation as a subagent panel")
	}
	if sub.status != subagentDone {
		t.Fatalf("resumed subagent status = %v, want done", sub.status)
	}
	if sub.result != "El proyecto es un CLI en Go." {
		t.Fatalf("resumed subagent result = %q, want the saved report", sub.result)
	}

	view := stripANSI(m.View())
	if strings.Contains(view, "description") {
		t.Fatalf("view = %q, want no raw task tool block (JSON) on resume", view)
	}
	if !strings.Contains(view, subagentGlyph) {
		t.Fatalf("view = %q, want the reconstructed subagent panel", view)
	}

	// A resumed (already-finished) subagent does not rejoin the active list.
	if n := len(m.treeSubagents()); n != 0 {
		t.Fatalf("treeSubagents = %d, want a resumed subagent excluded from the active list", n)
	}
}

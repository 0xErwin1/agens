package tui

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/message"
)

func subagentEvent(kind agentloop.LoopEventKind, sub agentloop.Subagent) StreamMsg {
	return StreamMsg{Event: agentloop.LoopEvent{Kind: kind, Subagent: sub}}
}

func TestModel_SubagentEventsDriveTheLivePanel(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendMsg(m, subagentEvent(agentloop.LoopSubagentStarted, agentloop.Subagent{ID: "s1", Name: "explore", Model: "gpt-5.5"}))
	sendMsg(m, subagentEvent(agentloop.LoopSubagentActivity, agentloop.Subagent{ID: "s1", Activity: "read"}))
	sendMsg(m, subagentEvent(agentloop.LoopSubagentActivity, agentloop.Subagent{ID: "s1", Tokens: 1200}))

	// While running, the panel shows the subagent's name, model, token total and
	// its latest activity.
	running := stripANSI(m.View())
	for _, want := range []string{"explore", "gpt-5.5", "1.2K tok", "read"} {
		if !strings.Contains(running, want) {
			t.Fatalf("running panel = %q, want it to contain %q", running, want)
		}
	}

	sendMsg(m, subagentEvent(agentloop.LoopSubagentFinished, agentloop.Subagent{ID: "s1", Result: "the final report"}))

	// Finished and collapsed: just the header (report folded away like any block).
	finished := stripANSI(m.View())
	if strings.Contains(finished, "the final report") {
		t.Fatalf("finished panel = %q, want the report folded until expanded", finished)
	}

	m.messages.ToggleDetails()
	expanded := stripANSI(m.View())
	if !strings.Contains(expanded, "the final report") {
		t.Fatalf("expanded panel = %q, want the subagent's report shown", expanded)
	}
}

func TestModel_TaskToolCallIsNotRenderedAsAToolBlock(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	done := message.NewMessage(message.RoleAssistant, message.ToolUsePart{
		ID:    "call_1",
		Name:  "task",
		Input: json.RawMessage(`{"description":"go do it"}`),
	})
	sendMsg(m, StreamMsg{Event: agentloop.LoopEvent{Kind: agentloop.LoopMessageDone, Message: &done}})

	view := stripANSI(m.View())
	if strings.Contains(view, "task") {
		t.Fatalf("View() = %q, want the task delegation shown by the panel, not as a tool block", view)
	}
}

func TestModel_FailedSubagentShowsFailedInPanel(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendMsg(m, subagentEvent(agentloop.LoopSubagentStarted, agentloop.Subagent{ID: "s1", Name: "worker", Model: "gpt-5.5"}))
	sendMsg(m, subagentEvent(agentloop.LoopSubagentFinished, agentloop.Subagent{ID: "s1", Failed: true}))

	if !strings.Contains(stripANSI(m.View()), "failed") {
		t.Fatalf("View() = %q, want a failed marker for a failed subagent", stripANSI(m.View()))
	}
}

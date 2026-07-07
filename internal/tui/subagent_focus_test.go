package tui

import (
	"encoding/json"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/0xErwin1/agens/internal/agentloop"
	"github.com/0xErwin1/agens/internal/message"
)

func TestSubagentFocus_ShowsHeaderAndSubagentConversation(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	m.messages.StartSubagent("a", "", "explore", "gpt-5.5", "investigate the parser")
	// Feed the subagent's own conversation a tool call so its chat has content.
	toolMsg := message.NewMessage(message.RoleAssistant, message.ToolUsePart{
		ID:    "t1",
		Name:  "read",
		Input: json.RawMessage(`{"path":"parser.go"}`),
	})
	m.messages.ApplySubagentStream("a", agentloop.LoopEvent{Kind: agentloop.LoopMessageDone, Message: &toolMsg})

	m.subagentFocusID = "a"
	view := stripANSI(m.View())

	// The header names the subagent and its task; the body is its own conversation
	// (a real tool block, not a muted summary); the footer is the breadcrumb.
	for _, want := range []string{"explore", "gpt-5.5", "investigate the parser", "read", "parser.go", "esc back"} {
		if !strings.Contains(view, want) {
			t.Fatalf("focus view = %q, want it to contain %q", view, want)
		}
	}
}

func TestSubagentFocus_FillsTheScreenHeight(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5") // sized at 80x24
	m.messages.StartSubagent("a", "", "explore", "gpt-5.5", "task")
	m.subagentFocusID = "a"

	if lines := strings.Count(m.View(), "\n") + 1; lines != m.height {
		t.Fatalf("focus view height = %d lines, want the full screen height %d", lines, m.height)
	}
}

func TestMessages_SubagentSiblingsSharesParent(t *testing.T) {
	m := NewMessages()
	m.SetSize(72, 16)

	m.StartSubagent("p", "", "explore", "gpt-5.5", "")
	m.StartSubagent("c1", "p", "grep", "gpt-5.5", "")
	m.StartSubagent("c2", "p", "read", "gpt-5.5", "")
	m.StartSubagent("top2", "", "review", "gpt-5.5", "")

	siblings, idx := m.subagentSiblings("c2")
	ids := make([]string, len(siblings))
	for i, s := range siblings {
		ids[i] = s.id
	}

	if strings.Join(ids, ",") != "c1,c2" {
		t.Fatalf("siblings of c2 = %v, want [c1 c2] (same parent only)", ids)
	}
	if idx != 1 {
		t.Fatalf("index of c2 among siblings = %d, want 1", idx)
	}
}

func TestModel_EnterOnTreeFocusesAndEscExits(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	m.messages.StartSubagent("a", "", "explore", "gpt-5.5", "")
	m.messages.StartSubagent("b", "", "review", "gpt-5.5", "")

	m.OpenSubagentTree()
	sendKey(m, tea.KeyMsg{Type: tea.KeyDown}) // select "b"
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if m.subagentTreeOpen {
		t.Fatal("Enter did not close the tree overlay")
	}
	if m.subagentFocusID != "b" {
		t.Fatalf("subagentFocusID = %q, want the selected subagent \"b\" focused", m.subagentFocusID)
	}

	// The conversation area is replaced by the focus view.
	if !strings.Contains(stripANSI(m.View()), "esc back") {
		t.Fatalf("View() = %q, want the focus breadcrumb while focused", stripANSI(m.View()))
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyEsc})
	if m.subagentFocusID != "" {
		t.Fatalf("subagentFocusID = %q after Esc, want focus cleared", m.subagentFocusID)
	}
}

func TestModel_FocusPrevNextCyclesSiblings(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	m.messages.StartSubagent("a", "", "explore", "gpt-5.5", "")
	m.messages.StartSubagent("b", "", "review", "gpt-5.5", "")
	m.messages.StartSubagent("c", "", "worker", "gpt-5.5", "")
	m.subagentFocusID = "a"

	sendKey(m, tea.KeyMsg{Type: tea.KeyRight})
	if m.subagentFocusID != "b" {
		t.Fatalf("after Right, focus = %q, want \"b\"", m.subagentFocusID)
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyLeft})
	sendKey(m, tea.KeyMsg{Type: tea.KeyLeft})
	if m.subagentFocusID != "c" {
		t.Fatalf("after wrapping Left past the start, focus = %q, want \"c\"", m.subagentFocusID)
	}
}

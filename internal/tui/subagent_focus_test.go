package tui

import (
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
)

func TestSubagentFocus_ShowsHeaderActivityAndBreadcrumb(t *testing.T) {
	m := NewMessages()
	m.SetSize(72, 16)

	m.StartSubagent("a", "", "explore", "gpt-5.5", "investigate X")
	m.AddSubagentTool("a", "t1", "read", "a.go")
	m.AddSubagentTool("a", "t2", "grep", "needle")
	m.UpdateSubagentProgress("a", 1500, 900*time.Millisecond)

	siblings, idx := m.subagentSiblings("a")
	view := stripANSI(renderSubagentFocus(m.findSubagent("a"), siblings, idx, m.width, m.height))

	for _, want := range []string{"explore", "gpt-5.5", "Task", "investigate X", "Activity", "read a.go", "grep needle", "esc back"} {
		if !strings.Contains(view, want) {
			t.Fatalf("focus view = %q, want it to contain %q", view, want)
		}
	}
}

func TestSubagentFocus_HeightIsExact(t *testing.T) {
	m := NewMessages()
	m.SetSize(72, 16)
	m.StartSubagent("a", "", "explore", "gpt-5.5", "")

	got := renderSubagentFocus(m.findSubagent("a"), []*subagentState{m.findSubagent("a")}, 0, 72, 16)
	if lines := strings.Count(got, "\n") + 1; lines != 16 {
		t.Fatalf("focus view height = %d lines, want exactly 16 so the input/footer stay put", lines)
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

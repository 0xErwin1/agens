package tui

import (
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
)

func TestMessages_OrderedSubagentsIsPreOrderTraversal(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	// Two top-level subagents; the first has a child added after the second
	// top-level started, so creation order alone would misplace it.
	m.StartSubagent("a", "", "explore", "gpt-5.5", "")
	m.StartSubagent("b", "", "review", "gpt-5.5", "")
	m.StartSubagent("a1", "a", "grep", "gpt-5.5", "")

	rows := m.orderedSubagents()
	got := make([]string, len(rows))
	depths := make([]int, len(rows))
	for i, r := range rows {
		got[i] = r.state.id
		depths[i] = r.depth
	}

	wantOrder := []string{"a", "a1", "b"}
	for i := range wantOrder {
		if got[i] != wantOrder[i] {
			t.Fatalf("orderedSubagents() ids = %v, want pre-order %v", got, wantOrder)
		}
	}
	if depths[0] != 0 || depths[1] != 1 || depths[2] != 0 {
		t.Fatalf("orderedSubagents() depths = %v, want [0 1 0]", depths)
	}
}

func TestMessages_OrphanSubagentIsReRootedNotDropped(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	// A subagent whose parent was never started must still appear (as a root),
	// never silently dropped.
	m.StartSubagent("orphan", "ghost", "worker", "gpt-5.5", "")

	rows := m.orderedSubagents()
	if len(rows) != 1 || rows[0].state.id != "orphan" || rows[0].depth != 0 {
		t.Fatalf("orderedSubagents() = %+v, want the orphan re-rooted at depth 0", rows)
	}
}

func TestSubagentTree_ShowsRunningCountAndRows(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartSubagent("a", "", "explore", "gpt-5.5", "high")
	m.UpdateSubagentProgress("a", 1200, 800*time.Millisecond)
	m.StartSubagent("b", "", "review", "gpt-5.5", "")
	m.CompleteSubagent("b", false, time.Second)

	view := stripANSI(renderSubagentTree(m.orderedSubagents(), 0, 60))

	if !strings.Contains(view, "1 active") {
		t.Fatalf("tree = %q, want a running count of 1 active", view)
	}
	for _, want := range []string{"explore", "review", "gpt-5.5", "1.2K tok"} {
		if !strings.Contains(view, want) {
			t.Fatalf("tree = %q, want it to contain %q", view, want)
		}
	}
}

func TestSubagentTree_EmptyState(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	view := stripANSI(renderSubagentTree(m.orderedSubagents(), 0, 60))
	if !strings.Contains(view, "no active subagents") {
		t.Fatalf("tree = %q, want the empty-state note", view)
	}
}

func TestModel_SubagentTreeKeyNavigationAndClose(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	m.messages.StartSubagent("a", "", "explore", "gpt-5.5", "")
	m.messages.StartSubagent("b", "", "review", "gpt-5.5", "")

	m.OpenSubagentTree()
	if !m.subagentTreeOpen {
		t.Fatal("OpenSubagentTree() did not open the overlay")
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyDown})
	if m.subagentIdx != 1 {
		t.Fatalf("after Down, subagentIdx = %d, want 1", m.subagentIdx)
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyEsc})
	if m.subagentTreeOpen {
		t.Fatal("Esc did not close the subagent tree overlay")
	}
}

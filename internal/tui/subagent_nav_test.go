package tui

import (
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
)

func TestModel_CtrlUpOpensSubagentTree(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")
	m.messages.StartSubagent("a", "", "explore", "gpt-5.5", "task")

	sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlUp})

	if !m.subagentTreeOpen {
		t.Fatal("Ctrl+Up did not open the subagent tree")
	}
}

func TestModel_EscFromFocusReturnsToTreeThenToChat(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")
	m.messages.StartSubagent("a", "", "explore", "gpt-5.5", "task")

	m.OpenSubagentTree()
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter}) // enter the subagent
	if m.subagentFocusID != "a" || m.subagentTreeOpen {
		t.Fatalf("after Enter: focus=%q treeOpen=%v, want focus on 'a' with the tree closed", m.subagentFocusID, m.subagentTreeOpen)
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyEsc}) // step back to the list
	if m.subagentFocusID != "" {
		t.Fatal("Esc from focus did not clear the focus")
	}
	if !m.subagentTreeOpen {
		t.Fatal("Esc from focus did not return to the subagent list")
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyEsc}) // leave the list to the chat
	if m.subagentTreeOpen {
		t.Fatal("Esc from the list did not close it")
	}
}

func TestMessages_FinishedSubagentExpiresFromTreeAfterLinger(t *testing.T) {
	now := time.Unix(1000, 0)
	m := NewMessages()
	m.SetSize(80, 20)
	m.SetClock(func() time.Time { return now })

	m.StartSubagent("a", "", "explore", "gpt-5.5", "task")
	m.CompleteSubagent("a", false, 0)

	if len(m.treeSubagents()) != 1 {
		t.Fatalf("treeSubagents = %d right after finishing, want it still listed", len(m.treeSubagents()))
	}

	now = now.Add(subagentListLinger + time.Second)
	if len(m.treeSubagents()) != 0 {
		t.Fatalf("treeSubagents = %d past the linger window, want the finished subagent dropped", len(m.treeSubagents()))
	}

	// A running subagent never expires, however long it runs.
	m.StartSubagent("b", "", "worker", "gpt-5.5", "still going")
	now = now.Add(time.Hour)
	rows := m.treeSubagents()
	if len(rows) != 1 || rows[0].state.id != "b" {
		t.Fatalf("treeSubagents = %+v, want only the still-running 'b'", rows)
	}
}

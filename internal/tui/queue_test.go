package tui

import (
	"context"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
)

// runningModel returns a sized model in the "a turn is in progress" state
// without starting a real turn goroutine, so the queue behavior can be exercised
// deterministically. In production only one turn runs at a time; simulating the
// running flag avoids overlapping the test double's single Run.
func runningModel(t *testing.T) *Model {
	t.Helper()
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")
	m.running = true
	return m
}

func TestModel_QueuesMessageWhileRunningAndSendsOnDone(t *testing.T) {
	m := runningModel(t)

	typeString(m, "second")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if len(m.queued) != 1 {
		t.Fatalf("queued = %d, want 1 while a turn runs", len(m.queued))
	}
	if !m.running {
		t.Fatal("queueing a message must not end the running turn")
	}
	if len(m.history) != 0 {
		t.Fatal("a queued message must not be appended to history until it is sent")
	}

	// Completing the turn successfully sends the queued message as a new turn.
	m.handleDone(TurnDoneMsg{})

	if len(m.queued) != 0 {
		t.Fatalf("queued = %d after a successful turn, want it drained", len(m.queued))
	}
	if !m.running {
		t.Fatal("the queued message did not start a new turn on completion")
	}
	if len(m.history) != 1 {
		t.Fatalf("history len = %d, want the queued message appended", len(m.history))
	}
}

func TestModel_CancelDropsQueuedMessages(t *testing.T) {
	m := runningModel(t)

	typeString(m, "second")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})
	if len(m.queued) != 1 {
		t.Fatalf("queued = %d, want 1 before cancel", len(m.queued))
	}

	m.handleDone(TurnDoneMsg{Err: context.Canceled})

	if len(m.queued) != 0 {
		t.Fatalf("queued = %d after cancel, want the queue dropped", len(m.queued))
	}
	if m.running {
		t.Fatal("cancel must not start a queued turn")
	}
}

func TestModel_ErrorKeepsQueuedMessages(t *testing.T) {
	m := runningModel(t)

	typeString(m, "second")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	m.handleDone(TurnDoneMsg{Err: context.DeadlineExceeded})

	if len(m.queued) != 1 {
		t.Fatalf("queued = %d after an error, want it kept for a later successful turn", len(m.queued))
	}
	if m.running {
		t.Fatal("an errored turn must not auto-send the queued message")
	}
}

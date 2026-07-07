package tui

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/0xErwin1/agens/internal/agentloop"
)

func streamEvent(m *Model, kind agentloop.LoopEventKind, text string) {
	m.handleStream(StreamMsg{Event: agentloop.LoopEvent{Kind: kind, Text: text}})
}

func TestModel_ReasoningShowsThinkingThenAnswer(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	streamEvent(m, agentloop.LoopReasoningDelta, "let me think about it")

	view := stripANSI(m.View())
	if !strings.Contains(view, "Thinking") {
		t.Fatalf("View() = %q, want a Thinking label while reasoning", view)
	}
	if !strings.Contains(view, "let me think about it") {
		t.Fatalf("View() = %q, want the streamed reasoning text", view)
	}

	streamEvent(m, agentloop.LoopTextDelta, "the answer")

	view = stripANSI(m.View())
	if !strings.Contains(view, "the answer") {
		t.Fatalf("View() = %q, want the answer once text streams", view)
	}
	// By default a finished reasoning block is not collapsed: its text stays
	// visible under the "Thinking" header rather than being folded away.
	if !strings.Contains(view, "Thinking") {
		t.Fatalf("View() = %q, want the Thinking header kept", view)
	}
	if !strings.Contains(view, "let me think about it") {
		t.Fatalf("View() = %q, want the finished reasoning shown in full by default", view)
	}
}

func TestModel_ReasoningCollapsesWhenConfigured(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")
	m.messages.SetDisplayOptions(true, false) // collapse_thinking = true

	streamEvent(m, agentloop.LoopReasoningDelta, "let me think about it")
	streamEvent(m, agentloop.LoopTextDelta, "the answer")

	view := stripANSI(m.View())
	if !strings.Contains(view, "Thinking") {
		t.Fatalf("View() = %q, want the collapsed Thinking header kept", view)
	}
	if strings.Contains(view, "let me think about it") {
		t.Fatalf("View() = %q, want the finished reasoning folded when collapse is configured", view)
	}

	m.messages.ToggleDetails()
	if !strings.Contains(stripANSI(m.View()), "let me think about it") {
		t.Fatalf("View() = %q, want the reasoning shown after expanding details", stripANSI(m.View()))
	}
}

var testEffortLevels = []string{"minimal", "low", "medium", "high", "xhigh"}

func sizedWithEffort() *Model {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")
	m.effortLevels = testEffortLevels
	return m
}

func TestModel_EffortSelectorSetsEffortEverywhere(t *testing.T) {
	m := sizedWithEffort()

	typeString(m, "/effort")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter}) // run /effort → open picker

	if !m.effortPickerOpen {
		t.Fatal("/effort did not open the effort selector")
	}
	if m.effortIdx != indexOfEffort(testEffortLevels, "") {
		t.Fatalf("selector opened on idx %d, want the default %d", m.effortIdx, indexOfEffort(testEffortLevels, ""))
	}

	// medium (default) → high
	sendKey(m, tea.KeyMsg{Type: tea.KeyDown})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if m.effortPickerOpen {
		t.Fatal("selector still open after choosing")
	}
	if m.effort != "high" {
		t.Fatalf("effort = %q, want high", m.effort)
	}
	runner, ok := m.loop.(*scriptedLoopRunner)
	if !ok || runner.effort != "high" {
		t.Fatal("SetEffort was not propagated to the loop")
	}
	if !strings.Contains(stripANSI(m.status.View()), "high") {
		t.Fatal("footer does not show the selected effort")
	}
}

func TestModel_EffortSelectorEscKeepsDefault(t *testing.T) {
	m := sizedWithEffort()

	typeString(m, "/effort")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})
	sendKey(m, tea.KeyMsg{Type: tea.KeyDown})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEsc})

	if m.effortPickerOpen {
		t.Fatal("Esc did not close the effort selector")
	}
	if m.effort != "" {
		t.Fatalf("effort = %q, want it unchanged after Esc", m.effort)
	}
}

func TestModel_InlineActivityIndicator(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	m.running = true
	m.workLabel = stateThinking
	if !strings.Contains(stripANSI(m.View()), "thinking…") {
		t.Fatal("inline activity indicator not shown while running")
	}

	streamEvent(m, agentloop.LoopTextDelta, "hello")
	if m.workLabel != stateWriting {
		t.Fatalf("workLabel = %q, want %q once the answer streams", m.workLabel, stateWriting)
	}

	m.handleDone(TurnDoneMsg{})
	if strings.Contains(stripANSI(m.View()), stateWriting) {
		t.Fatal("activity indicator still shown after the turn finished")
	}
}

func TestIndexOfEffort(t *testing.T) {
	// minimal, low, medium, high, xhigh → medium is index 2.
	if got := indexOfEffort(testEffortLevels, ""); got != 2 {
		t.Fatalf("indexOfEffort(\"\") = %d, want 2 (medium default)", got)
	}
	if got := indexOfEffort(testEffortLevels, "xhigh"); got != 4 {
		t.Fatalf("indexOfEffort(xhigh) = %d, want 4", got)
	}
}

func TestModel_EffortUnavailableWithoutLevels(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5") // no effort levels

	typeString(m, "/effort")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if m.effortPickerOpen {
		t.Fatal("selector opened for a provider with no effort levels")
	}
	if !strings.Contains(stripANSI(m.View()), "not available") {
		t.Fatalf("View() = %q, want an unavailable note", stripANSI(m.View()))
	}
}

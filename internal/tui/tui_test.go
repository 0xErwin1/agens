package tui

import (
	"context"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/message"
)

func sendKey(m *Model, k tea.KeyMsg) tea.Cmd {
	_, cmd := m.Update(k)
	return cmd
}

func sendMsg(m *Model, msg tea.Msg) tea.Cmd {
	_, cmd := m.Update(msg)
	return cmd
}

func sized(loop LoopRunner, modelName string) *Model {
	m := New(loop, modelName, nil, nil)
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})
	return m
}

func TestModel_SpinnerTicksOnlyWhileRunning(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	m.running = true
	cmd := sendMsg(m, m.spinner.Tick())
	if cmd == nil {
		t.Fatal("spinner tick while running returned no continuation command, want the animation to keep going")
	}
	if m.status.spinner == "" {
		t.Fatal("status spinner frame is empty while running, want an animated frame")
	}

	m.running = false
	if cmd := sendMsg(m, m.spinner.Tick()); cmd != nil {
		t.Fatal("spinner kept ticking after the turn ended, want it to stop")
	}

	m.handleDone(TurnDoneMsg{})
	if m.status.spinner != "" {
		t.Fatalf("status spinner = %q after the turn finished, want it cleared", m.status.spinner)
	}
}

func TestModel_PageUpScrollsConversation(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	for i := 0; i < 40; i++ {
		m.messages.AppendUser("a conversation line")
	}
	if !m.messages.vp.AtBottom() {
		t.Fatal("precondition failed: viewport should sit at the bottom after appends")
	}

	sendMsg(m, tea.KeyMsg{Type: tea.KeyPgUp})

	if m.messages.vp.AtBottom() {
		t.Fatal("PgUp did not scroll the conversation up; scroll is not wired to the messages view")
	}
}

func TestModel_WindowSizeSizesChildrenAndRenders(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	view := m.View()
	if strings.TrimSpace(view) == "" {
		t.Fatal("View() is empty after a WindowSizeMsg, want rendered children")
	}
	if !strings.Contains(view, "gpt-5.5") {
		t.Fatalf("View() = %q, want it to contain the status model name", view)
	}
}

func TestModel_EnterSubmitsStartsTurnAndShowsUserBlock(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi there")})
	cmd := sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if !m.running {
		t.Fatal("running = false after Enter submit, want true")
	}
	if cmd == nil {
		t.Fatal("Enter submit returned a nil cmd, want a waitFor command")
	}

	view := m.View()
	if !strings.Contains(view, "hi there") {
		t.Fatalf("View() = %q, want a user block containing the prompt", view)
	}
	if !strings.Contains(view, "thinking") {
		t.Fatalf("View() = %q, want the status to show a thinking state", view)
	}
	if m.input.Value() != "" {
		t.Fatalf("input.Value() = %q, want it reset after submit", m.input.Value())
	}
}

func TestModel_EnterWithEmptyInputDoesNotSubmit(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if m.running {
		t.Fatal("running = true after Enter with empty input, want false (no submit)")
	}
}

func TestModel_StreamTextDeltaAppearsInView(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	sendMsg(m, StreamMsg{Event: agentloop.LoopEvent{Kind: agentloop.LoopIterationStart, Iteration: 1}})
	sendMsg(m, StreamMsg{Event: agentloop.LoopEvent{Kind: agentloop.LoopTextDelta, Iteration: 1, Text: "streamed answer"}})

	if view := m.View(); !strings.Contains(view, "streamed answer") {
		t.Fatalf("View() = %q, want the streamed assistant text", view)
	}
}

func TestModel_ToolCallEventUpdatesStatusAndView(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	sendMsg(m, StreamMsg{Event: agentloop.LoopEvent{
		Kind:     agentloop.LoopToolCallStarted,
		ToolCall: message.ToolUsePart{ID: "t1", Name: "read"},
	}})

	view := m.View()
	if !strings.Contains(view, "→ read") {
		t.Fatalf("View() = %q, want a tool-call block for read", view)
	}
	if !strings.Contains(view, "running read") {
		t.Fatalf("View() = %q, want the status to show the running tool", view)
	}
}

func TestModel_TurnDoneStopsRunningAndAdoptsHistory(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	grown := []message.Message{{ID: "user-1"}, {ID: "asst-1"}}
	sendMsg(m, TurnDoneMsg{History: grown, Err: nil})

	if m.running {
		t.Fatal("running = true after TurnDoneMsg, want false")
	}
	if len(m.history) != len(grown) {
		t.Fatalf("history len = %d, want %d (adopted grown history)", len(m.history), len(grown))
	}
	if m.history[1].ID != "asst-1" {
		t.Fatalf("history[1].ID = %q, want the adopted assistant id", m.history[1].ID)
	}
	if view := m.View(); !strings.Contains(view, "ready") {
		t.Fatalf("View() = %q, want the status back to ready", view)
	}
}

func TestModel_TurnDoneWithErrorShowsErrorBlock(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	sendMsg(m, TurnDoneMsg{History: []message.Message{{ID: "user-1"}}, Err: errorString("provider exploded")})

	view := m.View()
	if !strings.Contains(view, "provider exploded") {
		t.Fatalf("View() = %q, want the error surfaced in the conversation", view)
	}
	if !strings.Contains(view, "error") {
		t.Fatalf("View() = %q, want the status to reflect an error", view)
	}
}

func TestModel_TurnDoneCanceledIsNotTreatedAsError(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	sendMsg(m, TurnDoneMsg{History: m.history, Err: context.Canceled})

	if m.running {
		t.Fatal("running = true after a canceled TurnDoneMsg, want false")
	}

	statusView := stripANSI(m.status.View())
	if !strings.Contains(statusView, "ready") {
		t.Fatalf("status View() = %q, want the ready state after a cancel", statusView)
	}

	messagesView := stripANSI(m.messages.View())
	if strings.Contains(messagesView, "error") {
		t.Fatalf("messages View() = %q, want no error block for a canceled turn", messagesView)
	}
}

func TestModel_EnterWhileRunningDoesNotStartConcurrentTurn(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if !m.running {
		t.Fatal("precondition: model should be running after the first submit")
	}
	historyLen := len(m.history)

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("second message")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if !m.running {
		t.Fatal("running = false after a second Enter while a turn is running, want it to stay true")
	}
	if len(m.history) != historyLen {
		t.Fatalf("history len = %d after a second Enter while running, want unchanged %d (no concurrent turn started)", len(m.history), historyLen)
	}
}

func TestModel_CtrlCIdleQuits(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	cmd := sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlC})
	if cmd == nil {
		t.Fatal("ctrl+c while idle returned nil cmd, want tea.Quit")
	}
	if _, ok := cmd().(tea.QuitMsg); !ok {
		t.Fatal("ctrl+c while idle did not return a quit command")
	}
}

func TestModel_CtrlCWhileRunningDoesNotQuit(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})
	if !m.running {
		t.Fatal("precondition: model should be running after submit")
	}

	cmd := sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlC})
	if cmd != nil {
		if _, ok := cmd().(tea.QuitMsg); ok {
			t.Fatal("ctrl+c while running returned a quit command, want it to cancel instead")
		}
	}
}

// errorString is a minimal error used to drive TurnDoneMsg error handling
// without pulling in the errors package for a single sentinel.
type errorString string

func (e errorString) Error() string { return string(e) }

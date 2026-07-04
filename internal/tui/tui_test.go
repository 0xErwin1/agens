package tui

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
	"time"

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
	m := New(Deps{Loop: loop, Model: modelName})
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

func TestModel_ReservesGapBetweenChatAndInput(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5") // sized at 80x24

	want := 24 - inputHeight - statusHeight - inputGap - topPad
	if m.messages.height != want {
		t.Fatalf("messages height = %d, want %d (blank rows reserved before the input and at the top)", m.messages.height, want)
	}
}

func TestModel_CentersContentWithPadding(t *testing.T) {
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5"})
	m.Update(tea.WindowSizeMsg{Width: 200, Height: 24})

	if m.contentWidth != maxContentWidth {
		t.Fatalf("contentWidth = %d, want it capped at %d on a wide terminal", m.contentWidth, maxContentWidth)
	}
	wantPad := (200 - maxContentWidth) / 2
	if m.leftPad != wantPad {
		t.Fatalf("leftPad = %d, want %d (centered)", m.leftPad, wantPad)
	}

	lines := strings.Split(m.View(), "\n")
	if strings.TrimSpace(lines[0]) != "" {
		t.Fatalf("first line = %q, want a blank top-padding row", lines[0])
	}
	for _, line := range lines {
		if strings.TrimSpace(line) == "" {
			continue
		}
		if !strings.HasPrefix(line, strings.Repeat(" ", wantPad)) {
			t.Fatalf("content line %q is not left-padded by %d spaces", line, wantPad)
		}
		break
	}
}

func TestModel_ScrollStaysPutWhileStreaming(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")
	for i := 0; i < 40; i++ {
		m.messages.AppendUser("a conversation line")
	}

	// Scroll up, then simulate the agent appending more content.
	sendKey(m, tea.KeyMsg{Type: tea.KeyPgUp})
	offsetBefore := m.messages.vp.YOffset

	m.messages.AppendUser("another line while the user is reading up")

	if m.messages.vp.AtBottom() {
		t.Fatal("appending content snapped the view to the bottom while scrolled up")
	}
	if m.messages.vp.YOffset != offsetBefore {
		t.Fatalf("scroll offset moved from %d to %d on append, want it to stay", offsetBefore, m.messages.vp.YOffset)
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

func TestModel_StrayTickAfterDoneDoesNotRestoreSpinner(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	m.running = true
	sendMsg(m, m.spinner.Tick()) // spinner set while running
	m.handleDone(TurnDoneMsg{})  // clears spinner, running = false

	sendMsg(m, m.spinner.Tick()) // a stray tick arriving after the turn ended

	if m.status.spinner != "" {
		t.Fatalf("status spinner = %q after a stray tick, want it to stay cleared", m.status.spinner)
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

	// Live markdown splits words into separate style spans, so strip ANSI
	// before matching the contiguous phrase.
	if view := stripANSI(m.View()); !strings.Contains(view, "streamed answer") {
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
	if !strings.Contains(m.View(), "running read") {
		t.Fatalf("View() = %q, want the status to show the running tool", m.View())
	}

	// The call block, with its argument detail, appears once the message with
	// the assembled input is finalized.
	done := message.NewMessage(message.RoleAssistant, message.ToolUsePart{
		ID: "t1", Name: "read", Input: json.RawMessage(`{"path":"internal/foo.go"}`),
	})
	sendMsg(m, StreamMsg{Event: agentloop.LoopEvent{Kind: agentloop.LoopMessageDone, Message: &done}})

	view := stripANSI(m.View())
	if !strings.Contains(view, "▸ read") {
		t.Fatalf("View() = %q, want a tool-call block for read", view)
	}
	if !strings.Contains(view, "internal/foo.go") {
		t.Fatalf("View() = %q, want the read path shown as the tool detail", view)
	}
}

func TestModel_ToolResultCompletesBlockWithDuration(t *testing.T) {
	start := time.Unix(5000, 0)
	m := New(Deps{
		Loop:  &scriptedLoopRunner{},
		Model: "gpt-5.5",
		Now:   fakeClock(start, start, start.Add(1200*time.Millisecond)),
	})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	done := message.NewMessage(message.RoleAssistant, message.ToolUsePart{
		ID: "t1", Name: "bash", Input: json.RawMessage(`{"command":"ls"}`),
	})
	sendMsg(m, StreamMsg{Event: agentloop.LoopEvent{Kind: agentloop.LoopMessageDone, Message: &done}}) // toolClock = start

	result := message.ToolResultPart{ToolUseID: "t1", Content: message.Parts{message.TextPart{Text: "a.go b.go"}}}
	sendMsg(m, StreamMsg{Event: agentloop.LoopEvent{Kind: agentloop.LoopToolResult, ToolResult: result}}) // now = start+1.2s

	view := stripANSI(m.View())
	if !strings.Contains(view, "1.2s") {
		t.Fatalf("View() = %q, want the tool block completed with its 1.2s duration", view)
	}
	// Folded by default: the result body is hidden until Ctrl+O.
	if strings.Contains(view, "a.go b.go") {
		t.Fatalf("View() = %q, want the tool result folded by default", view)
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlO})
	if !strings.Contains(stripANSI(m.View()), "a.go b.go") {
		t.Fatalf("View() = %q, want the result shown after Ctrl+O expands tools", stripANSI(m.View()))
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

func TestModel_TurnErrorDisplayStripsInternalPackagePrefixes(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	sendMsg(m, TurnDoneMsg{Err: errorString("agentloop: open stream: chatgpt: HTTP 503: overloaded")})

	view := stripANSI(m.messages.View())
	if !strings.Contains(view, "HTTP 503: overloaded") {
		t.Fatalf("View() = %q, want the innermost error message kept", view)
	}
	if strings.Contains(view, "agentloop:") || strings.Contains(view, "chatgpt:") {
		t.Fatalf("View() = %q, want the internal package prefixes stripped", view)
	}
}

func TestModel_TurnDoneWithAuthErrorShowsReloginHint(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	sendMsg(m, TurnDoneMsg{Err: authErrorString("chatgpt: HTTP 401: invalid credentials")})

	view := stripANSI(m.messages.View())
	if !strings.Contains(view, "invalid credentials") {
		t.Fatalf("View() = %q, want the raw auth error still shown", view)
	}
	if !strings.Contains(view, "agens auth login") {
		t.Fatalf("View() = %q, want the actionable re-login hint for an auth failure", view)
	}
}

func TestModel_TurnDoneWithNonAuthErrorHasNoReloginHint(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	sendMsg(m, TurnDoneMsg{Err: errorString("provider exploded")})

	view := stripANSI(m.messages.View())
	if strings.Contains(view, "agens auth login") {
		t.Fatalf("View() = %q, want no re-login hint for a non-auth error", view)
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

func TestModel_CtrlCIdleDoubleQuits(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	// First Ctrl+C only arms the quit prompt.
	if cmd := sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlC}); cmd != nil {
		if _, ok := cmd().(tea.QuitMsg); ok {
			t.Fatal("a single ctrl+c while idle quit, want it to arm a confirmation first")
		}
	}
	if !m.quitArmed {
		t.Fatal("first ctrl+c did not arm the quit prompt")
	}
	if !strings.Contains(stripANSI(m.View()), "press ctrl+c again to quit") {
		t.Fatalf("View() = %q, want the quit hint", stripANSI(m.View()))
	}

	// Second consecutive Ctrl+C quits.
	cmd := sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlC})
	if cmd == nil {
		t.Fatal("second ctrl+c returned nil cmd, want tea.Quit")
	}
	if _, ok := cmd().(tea.QuitMsg); !ok {
		t.Fatal("second consecutive ctrl+c did not quit")
	}
}

func TestModel_CtrlCDisarmedByOtherKey(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlC}) // arm
	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("x")})
	if m.quitArmed {
		t.Fatal("typing a key did not disarm the quit prompt")
	}
}

func TestModel_CtrlCClearsInput(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	typeString(m, "some draft")
	cmd := sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlC})

	if m.input.Value() != "" {
		t.Fatalf("input = %q, want ctrl+c to clear it", m.input.Value())
	}
	if cmd != nil {
		if _, ok := cmd().(tea.QuitMsg); ok {
			t.Fatal("ctrl+c cleared the input but also quit, want only the clear")
		}
	}
	if m.quitArmed {
		t.Fatal("clearing the input should not arm quit")
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

// authErrorString is an errorString classified as an authentication failure,
// satisfying provider.AuthError so the model surfaces the re-login hint.
type authErrorString string

func (e authErrorString) Error() string     { return string(e) }
func (e authErrorString) IsAuthError() bool { return true }

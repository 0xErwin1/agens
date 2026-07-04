package tui

import (
	"context"
	"errors"
	"reflect"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/message"
)

// scriptedLoopRunner is a LoopRunner double that replays a fixed sequence of
// LoopEvent values through the sink it is given, then returns a fixed grown
// history and error.
type scriptedLoopRunner struct {
	events  []agentloop.LoopEvent
	history []message.Message
	err     error

	receivedHistory []message.Message
	model           string
	systemPrompt    string
}

var _ LoopRunner = (*scriptedLoopRunner)(nil)

func (r *scriptedLoopRunner) SetModel(id string)            { r.model = id }
func (r *scriptedLoopRunner) SetSystemPrompt(prompt string) { r.systemPrompt = prompt }

func (r *scriptedLoopRunner) Run(_ context.Context, history []message.Message, sink func(agentloop.LoopEvent)) ([]message.Message, error) {
	r.receivedHistory = history
	for _, ev := range r.events {
		sink(ev)
	}
	return r.history, r.err
}

// drainCmd calls cmd and returns the tea.Msg it produces, failing the test
// if cmd is nil.
func drainCmd(t *testing.T, cmd tea.Cmd) tea.Msg {
	t.Helper()
	if cmd == nil {
		t.Fatal("waitFor returned a nil tea.Cmd")
	}
	return cmd()
}

func TestRunTurn_DeliversScriptedEventsThenDone(t *testing.T) {
	scripted := []agentloop.LoopEvent{
		{Kind: agentloop.LoopIterationStart, Iteration: 1},
		{Kind: agentloop.LoopTextDelta, Iteration: 1, Text: "hello"},
		{Kind: agentloop.LoopTextDelta, Iteration: 1, Text: " world"},
		{Kind: agentloop.LoopMessageDone, Iteration: 1, Message: &message.Message{ID: "asst-1"}},
	}
	wantHistory := []message.Message{{ID: "user-1"}, {ID: "asst-1"}}
	runner := &scriptedLoopRunner{events: scripted, history: wantHistory}

	history := []message.Message{{ID: "user-1"}}
	ch := runTurn(context.Background(), runner, history)

	for i, want := range scripted {
		msg := drainCmd(t, waitFor(ch))
		got, ok := msg.(StreamMsg)
		if !ok {
			t.Fatalf("event %d: got %T, want StreamMsg", i, msg)
		}
		if !reflect.DeepEqual(got.Event, want) {
			t.Fatalf("event %d: got %+v, want %+v", i, got.Event, want)
		}
	}

	doneMsg := drainCmd(t, waitFor(ch))
	done, ok := doneMsg.(TurnDoneMsg)
	if !ok {
		t.Fatalf("final message: got %T, want TurnDoneMsg", doneMsg)
	}
	if done.Err != nil {
		t.Fatalf("done.Err = %v, want nil", done.Err)
	}
	if len(done.History) != len(wantHistory) {
		t.Fatalf("done.History = %+v, want %+v", done.History, wantHistory)
	}
	for i := range wantHistory {
		if done.History[i].ID != wantHistory[i].ID {
			t.Fatalf("done.History[%d] = %+v, want %+v", i, done.History[i], wantHistory[i])
		}
	}

	if _, stillOpen := <-ch; stillOpen {
		t.Fatal("channel was not closed after TurnDoneMsg")
	}
}

func TestRunTurn_PassesHistoryToLoopRunner(t *testing.T) {
	runner := &scriptedLoopRunner{}
	history := []message.Message{{ID: "user-1"}, {ID: "asst-1"}}

	ch := runTurn(context.Background(), runner, history)
	drainCmd(t, waitFor(ch)) // TurnDoneMsg, since no scripted events

	if len(runner.receivedHistory) != len(history) {
		t.Fatalf("Run received history len %d, want %d", len(runner.receivedHistory), len(history))
	}
	for i := range history {
		if runner.receivedHistory[i].ID != history[i].ID {
			t.Fatalf("Run received history[%d] = %+v, want %+v", i, runner.receivedHistory[i], history[i])
		}
	}
}

func TestRunTurn_SurfacesLoopError(t *testing.T) {
	wantErr := errors.New("boom")
	runner := &scriptedLoopRunner{err: wantErr}

	ch := runTurn(context.Background(), runner, nil)
	doneMsg := drainCmd(t, waitFor(ch))

	done, ok := doneMsg.(TurnDoneMsg)
	if !ok {
		t.Fatalf("got %T, want TurnDoneMsg", doneMsg)
	}
	if !errors.Is(done.Err, wantErr) {
		t.Fatalf("done.Err = %v, want %v", done.Err, wantErr)
	}
}

func TestWaitFor_OnClosedChannelReturnsNil(t *testing.T) {
	ch := make(chan tea.Msg)
	close(ch)

	cmd := waitFor(ch)
	if cmd == nil {
		t.Fatal("waitFor returned a nil tea.Cmd")
	}
	if msg := cmd(); msg != nil {
		t.Fatalf("waitFor on closed channel = %v, want nil", msg)
	}
}

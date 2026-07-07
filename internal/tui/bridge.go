package tui

import (
	"context"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/0xErwin1/agens/internal/agentloop"
	"github.com/0xErwin1/agens/internal/message"
)

// turnChannelBuffer is the capacity of the channel runTurn returns, sized so
// the loop goroutine rarely blocks waiting for the UI to drain a StreamMsg.
const turnChannelBuffer = 64

// StreamMsg carries one incremental agentloop.LoopEvent from a running turn
// into the Bubble Tea Update loop.
type StreamMsg struct {
	Event agentloop.LoopEvent
}

// TurnDoneMsg signals that the turn goroutine has finished. History is the
// grown history LoopRunner.Run returned, to be adopted as the base for the
// next turn. Err is non-nil if the turn failed or was canceled.
type TurnDoneMsg struct {
	History []message.Message
	Err     error
}

// LoopRunner is the seam the bridge drives; *agentloop.Loop satisfies it.
// Depending on this interface, rather than a concrete *agentloop.Loop, keeps
// the bridge testable with a fake and free of a hard dependency on
// agentloop's construction.
type LoopRunner interface {
	Run(ctx context.Context, history []message.Message, sink func(agentloop.LoopEvent)) ([]message.Message, error)
	// SetModel switches the model used for subsequent turns.
	SetModel(id string)
	// SetSystemPrompt replaces the system prompt for subsequent turns.
	SetSystemPrompt(prompt string)
	// SetEffort changes the reasoning effort for subsequent turns.
	SetEffort(effort string)
}

// runTurn starts loop.Run on a goroutine and returns a channel that
// delivers a StreamMsg for every LoopEvent the sink receives, followed by a
// single final TurnDoneMsg, after which the channel is closed. history must
// already include the new user message; it is passed through to Run
// unmodified.
//
// The returned channel is buffered so the loop goroutine rarely blocks on a
// slow UI consumer. The goroutine always terminates: Run is synchronous and
// blocking, and once it returns, runTurn sends TurnDoneMsg and closes the
// channel unconditionally, so no goroutine is ever leaked.
func runTurn(ctx context.Context, loop LoopRunner, history []message.Message) <-chan tea.Msg {
	ch := make(chan tea.Msg, turnChannelBuffer)

	go func() {
		sink := func(ev agentloop.LoopEvent) {
			ch <- StreamMsg{Event: ev}
		}

		hist, err := loop.Run(ctx, history, sink)
		ch <- TurnDoneMsg{History: hist, Err: err}
		close(ch)
	}()

	return ch
}

// waitFor returns a tea.Cmd that blocks on the next message from ch and
// returns it, or returns nil once ch is closed. The consuming model's
// Update is expected to call waitFor(ch) again after receiving a StreamMsg,
// to keep listening for the rest of the turn, and to stop re-subscribing
// once it receives a TurnDoneMsg (at which point ch is about to close, if
// it hasn't already).
func waitFor(ch <-chan tea.Msg) tea.Cmd {
	return func() tea.Msg {
		msg, ok := <-ch
		if !ok {
			return nil
		}
		return msg
	}
}

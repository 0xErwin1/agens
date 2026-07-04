package tui

import (
	"context"
	"errors"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/message"
)

// Layout dimensions. The input and status bars have fixed heights; the
// conversation view takes the remaining vertical space.
const (
	inputHeight  = 3
	statusHeight = 1
)

// State labels shown in the status bar.
const (
	stateThinking = "thinking…"
	stateRunning  = "running "
	stateReady    = "ready"
	stateError    = "error"
)

// Model is the root Bubble Tea model. It composes the input, status, and
// messages components, owns the conversation history, and bridges a running
// turn's LoopEvents into component mutations.
type Model struct {
	input    *Input
	status   *Status
	messages *Messages

	loop      LoopRunner
	modelName string
	history   []message.Message

	running bool
	events  <-chan tea.Msg
	cancel  context.CancelFunc

	// prompter routes tool-permission decisions into this event loop; it is
	// nil when the caller pre-approved every call (--dangerously-allow-all),
	// in which case no modal is ever shown. pending holds the request whose
	// modal is currently on screen, or nil when none is.
	prompter *Prompter
	pending  *PermissionRequest

	width, height int
}

var _ tea.Model = (*Model)(nil)

// New constructs the root model for the given loop and display model name. A
// non-nil prompter installs the interactive permission modal; pass nil when
// permission decisions are resolved without prompting.
func New(loop LoopRunner, modelName string, prompter *Prompter) *Model {
	return &Model{
		input:     NewInput(),
		status:    NewStatus(modelName),
		messages:  NewMessages(),
		loop:      loop,
		modelName: modelName,
		prompter:  prompter,
	}
}

// Init focuses the input and, when an interactive prompter is installed,
// starts listening for permission requests.
func (m *Model) Init() tea.Cmd {
	cmds := []tea.Cmd{m.input.Focus()}
	if m.prompter != nil {
		cmds = append(cmds, waitForPermission(m.prompter.Requests()))
	}
	return tea.Batch(cmds...)
}

// Update runs in two phases: it first dispatches on the message kind (layout,
// global keys, and turn events), then forwards the message to the input so the
// textarea cursor keeps updating — except for the global keys it already
// consumed (Enter submit and Ctrl+C).
func (m *Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmds []tea.Cmd
	swallow := false

	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		m.width = msg.Width
		m.height = msg.Height
		m.layout()

	case PermissionRequestMsg:
		req := msg.Request
		m.pending = &req
		m.layout()

	case tea.KeyMsg:
		if m.pending != nil {
			swallow = true
			cmds = append(cmds, m.handleModalKey(msg))
			break
		}
		switch msg.Type {
		case tea.KeyCtrlC:
			swallow = true
			if m.running {
				m.abort()
			} else {
				return m, tea.Quit
			}
		case tea.KeyEnter:
			swallow = true
			if !m.running && strings.TrimSpace(m.input.Value()) != "" {
				cmds = append(cmds, m.submit())
			}
		}

	case StreamMsg:
		cmds = append(cmds, m.handleStream(msg))

	case TurnDoneMsg:
		m.handleDone(msg)
	}

	if !swallow {
		_, cmd := m.input.Update(msg)
		cmds = append(cmds, cmd)
	}

	return m, tea.Batch(cmds...)
}

// View stacks the conversation and status bar, then either the prompt input
// or, while a tool awaits approval, the permission modal in its place.
func (m *Model) View() string {
	bottom := m.input.View()
	if m.pending != nil {
		bottom = renderPermission(m.pending.Call, m.width)
	}

	return lipgloss.JoinVertical(lipgloss.Left,
		m.messages.View(),
		m.status.View(),
		bottom,
	)
}

// layout distributes the current window size across the children: the status
// bar is a fixed row, the bottom area is the input (or, while a permission
// modal is shown, its taller footprint), and the messages view takes the
// remainder.
func (m *Model) layout() {
	bottomHeight := inputHeight
	if m.pending != nil {
		bottomHeight = modalHeight
	}

	msgHeight := m.height - bottomHeight - statusHeight
	if msgHeight < 0 {
		msgHeight = 0
	}

	m.messages.SetSize(m.width, msgHeight)
	m.status.SetSize(m.width, statusHeight)
	m.input.SetSize(m.width, inputHeight)
}

// handleModalKey resolves the on-screen permission modal from a keypress.
// Ctrl+C cancels the whole turn (the loop goroutine's ctx cancellation
// unblocks the prompter on its own); a bound answer key is sent back to the
// waiting Prompt; unbound keys are ignored so the modal stays up. Either way
// the messages view is restored to full height and the listener re-armed for
// the next request.
func (m *Model) handleModalKey(msg tea.KeyMsg) tea.Cmd {
	if msg.Type == tea.KeyCtrlC {
		m.abort()
		m.pending = nil
		m.layout()
		return waitForPermission(m.prompter.Requests())
	}

	answer, ok := answerForModalKey(msg)
	if !ok {
		return nil
	}

	m.pending.Reply <- answer // buffered channel; never blocks
	m.pending = nil
	m.layout()

	return waitForPermission(m.prompter.Requests())
}

// submit consumes the current input as a new user turn: it records the user
// message, shows it, marks the model busy, and starts the turn goroutine,
// returning the command that waits for the first event.
func (m *Model) submit() tea.Cmd {
	text := m.input.Value()
	m.input.Reset()

	m.history = append(m.history, message.NewMessage(message.RoleUser, message.TextPart{Text: text}))
	m.messages.AppendUser(text)
	m.status.SetState(stateThinking)
	m.running = true

	ctx, cancel := context.WithCancel(context.Background())
	m.cancel = cancel
	m.events = runTurn(ctx, m.loop, m.history)

	return waitFor(m.events)
}

// abort cancels the in-flight turn without quitting the program. The turn's
// goroutine observes the canceled context, and the resulting TurnDoneMsg
// clears the running state.
func (m *Model) abort() {
	if m.cancel != nil {
		m.cancel()
	}
}

// handleStream applies one turn event to the components and returns the
// command that continues listening for the rest of the turn.
func (m *Model) handleStream(msg StreamMsg) tea.Cmd {
	switch msg.Event.Kind {
	case agentloop.LoopIterationStart:
		m.messages.StartAssistant()

	case agentloop.LoopTextDelta:
		m.messages.AppendAssistantDelta(msg.Event.Text)

	case agentloop.LoopToolCallStarted:
		m.messages.FinishAssistant()
		m.messages.AddToolCall(msg.Event.ToolCall.Name)
		m.status.SetState(stateRunning + msg.Event.ToolCall.Name)

	case agentloop.LoopToolResult:
		m.messages.AddToolResult(toolResultText(msg.Event.ToolResult), msg.Event.ToolResult.IsError)

	case agentloop.LoopMessageDone:
		m.messages.FinishAssistant()

	case agentloop.LoopUsage:
		// Usage is not surfaced in this batch.
	}

	return waitFor(m.events)
}

// handleDone finalizes a completed turn: it clears the running state, adopts
// the grown history, and reflects success or failure in the status bar. A
// canceled turn is treated as a clean stop rather than an error.
func (m *Model) handleDone(msg TurnDoneMsg) {
	m.running = false
	if m.cancel != nil {
		m.cancel()
		m.cancel = nil
	}
	m.events = nil

	if msg.History != nil {
		m.history = msg.History
	}

	if msg.Err != nil && !errors.Is(msg.Err, context.Canceled) {
		m.messages.SetError(msg.Err.Error())
		m.status.SetState(stateError)
		return
	}

	m.status.SetState(stateReady)
}

// toolResultText flattens a tool result's parts into a single string by
// concatenating its TextPart contents, ignoring any other part kind.
func toolResultText(result message.ToolResultPart) string {
	var b strings.Builder
	for _, p := range result.Content {
		if text, ok := p.(message.TextPart); ok {
			b.WriteString(text.Text)
		}
	}
	return b.String()
}

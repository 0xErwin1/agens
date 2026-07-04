package tui

import (
	"context"
	"errors"
	"strings"

	"github.com/charmbracelet/bubbles/spinner"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// Layout dimensions. The input and status bars have fixed heights; the
// conversation view takes the remaining vertical space.
const (
	inputHeight  = 3
	statusHeight = 1

	// inputGap is a blank row between the conversation and the input, so the
	// last message does not sit flush against the prompt.
	inputGap = 1
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
	spinner  spinner.Model

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

	// commands is the slash-command registry the palette draws from. showPalette
	// is set while the input holds a slash command; paletteItems are the
	// commands currently matching it and paletteIdx the highlighted one.
	commands     *CommandRegistry
	showPalette  bool
	paletteItems []Command
	paletteIdx   int

	// lister fetches the model catalog for the selector; nil disables it. The
	// remaining fields hold the selector's state while it is open.
	lister          ModelLister
	modelPickerOpen bool
	modelItems      []provider.ModelInfo
	modelIdx        int
	modelLoading    bool
	modelErr        error

	width, height int
}

var (
	_ tea.Model      = (*Model)(nil)
	_ CommandContext = (*Model)(nil)
)

// New constructs the root model for the given loop and display model name. A
// non-nil prompter installs the interactive permission modal; pass nil when
// permission decisions are resolved without prompting. A non-nil lister
// enables the /model selector; pass nil to disable it.
func New(loop LoopRunner, modelName string, prompter *Prompter, lister ModelLister) *Model {
	sp := spinner.New(spinner.WithSpinner(spinner.MiniDot))
	sp.Style = lipgloss.NewStyle().Foreground(CurrentTheme().Accent())

	return &Model{
		input:     NewInput(),
		status:    NewStatus(modelName),
		messages:  NewMessages(),
		spinner:   sp,
		loop:      loop,
		modelName: modelName,
		prompter:  prompter,
		commands:  defaultCommands(),
		lister:    lister,
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

	case tea.MouseMsg:
		swallow = true
		_, cmd := m.messages.Update(msg)
		cmds = append(cmds, cmd)

	case tea.KeyMsg:
		if isScrollKey(msg) {
			swallow = true
			_, cmd := m.messages.Update(msg)
			cmds = append(cmds, cmd)
			break
		}
		if m.pending != nil {
			swallow = true
			cmds = append(cmds, m.handleModalKey(msg))
			break
		}
		if m.modelPickerOpen {
			swallow = true
			m.handleModelPickerKey(msg)
			break
		}
		if m.showPalette {
			if cmd, consumed := m.handlePaletteKey(msg); consumed {
				swallow = true
				cmds = append(cmds, cmd)
				break
			}
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
			cmds = append(cmds, m.onEnter())
		}

	case modelsLoadedMsg:
		m.modelLoading = false
		m.modelErr = msg.err
		m.modelItems = msg.models
		m.modelIdx = indexOfModel(msg.models, m.modelName)

	case spinner.TickMsg:
		var cmd tea.Cmd
		m.spinner, cmd = m.spinner.Update(msg)
		m.status.SetSpinner(m.spinner.View())
		if m.running {
			cmds = append(cmds, cmd)
		}

	case StreamMsg:
		cmds = append(cmds, m.handleStream(msg))

	case TurnDoneMsg:
		m.handleDone(msg)
	}

	if !swallow {
		_, cmd := m.input.Update(msg)
		cmds = append(cmds, cmd)

		if _, ok := msg.(tea.KeyMsg); ok {
			m.refreshPalette()
		}
	}

	return m, tea.Batch(cmds...)
}

// View composes the fixed frame — conversation, prompt input, footer — and
// then floats any active overlay (permission modal or command palette) just
// above the input. Overlays are composited on top of the conversation rather
// than inserted into the layout, so the chat never resizes or scrolls when one
// appears.
func (m *Model) View() string {
	base := lipgloss.JoinVertical(lipgloss.Left,
		m.messages.View(),
		"", // inputGap: blank row between the conversation and the input
		m.input.View(),
		m.status.View(),
	)

	// The input begins after the conversation view and the gap row; overlays
	// end on the row just above it.
	inputRow := m.messages.height + inputGap

	switch {
	case m.pending != nil:
		return overlayAbove(base, renderPermission(m.pending.Call, m.width), inputRow)
	case m.modelPickerOpen:
		overlay := renderModelSelector(m.modelItems, m.modelIdx, m.modelLoading, m.modelErr, m.modelName, m.width)
		return overlayAbove(base, overlay, inputRow)
	case m.showPalette:
		return overlayAbove(base, renderPalette(m.paletteItems, m.paletteIdx, m.width), inputRow)
	default:
		return base
	}
}

// overlayAbove composites overlay onto base so that overlay's last line lands
// on the row just above inputRow, leaving the rest of base (and its line count)
// unchanged. Overlay lines that would fall outside base are dropped.
func overlayAbove(base, overlay string, inputRow int) string {
	baseLines := strings.Split(base, "\n")
	overlayLines := strings.Split(overlay, "\n")

	top := inputRow - len(overlayLines)
	for i, line := range overlayLines {
		row := top + i
		if row < 0 || row >= len(baseLines) {
			continue
		}
		baseLines[row] = line
	}

	return strings.Join(baseLines, "\n")
}

// layout gives the conversation view all the vertical space left by the fixed
// input and footer rows. Overlays float on top of it (see View) and never
// reduce this height.
func (m *Model) layout() {
	msgHeight := m.height - inputHeight - statusHeight - inputGap
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

	return tea.Batch(waitFor(m.events), m.spinner.Tick)
}

// onEnter handles the Enter key when the palette has not already consumed it:
// a slash command is run (unknown ones report an error note), an empty or
// in-flight input is ignored, and anything else is submitted as a chat turn.
func (m *Model) onEnter() tea.Cmd {
	value := strings.TrimSpace(m.input.Value())
	if value == "" || m.running {
		return nil
	}

	if strings.HasPrefix(value, "/") {
		if c, ok := m.commands.Lookup(value); ok {
			return m.runCommand(c)
		}

		m.input.Reset()
		m.closePalette()
		m.messages.AddInfo("unknown command: " + value)
		m.layout()
		return nil
	}

	return m.submit()
}

// refreshPalette recomputes the command palette from the current input after a
// keystroke: it is shown only while idle and while the input holds a slash
// command. It relayouts when visibility changes so the messages view shrinks
// or grows to make room.
func (m *Model) refreshPalette() {
	previous := m.showPalette

	if m.running {
		m.paletteItems = nil
	} else {
		m.paletteItems = m.commands.Match(m.input.Value())
	}

	m.showPalette = len(m.paletteItems) > 0
	if m.paletteIdx >= len(m.paletteItems) {
		m.paletteIdx = 0
	}

	if previous != m.showPalette {
		m.layout()
	}
}

// handlePaletteKey handles a keypress while the palette is open. It reports
// whether it consumed the key: cycling the selection (Up/Down, Tab/Shift+Tab),
// dismissal (Esc), and running the selection (Enter) are consumed; anything
// else falls through so ordinary typing still edits the input and re-filters
// the palette. Navigation wraps around the ends.
func (m *Model) handlePaletteKey(msg tea.KeyMsg) (tea.Cmd, bool) {
	n := len(m.paletteItems)

	switch msg.Type {
	case tea.KeyUp, tea.KeyShiftTab:
		m.paletteIdx = (m.paletteIdx - 1 + n) % n
		return nil, true

	case tea.KeyDown, tea.KeyTab:
		m.paletteIdx = (m.paletteIdx + 1) % n
		return nil, true

	case tea.KeyEsc:
		m.input.Reset()
		m.closePalette()
		m.layout()
		return nil, true

	case tea.KeyEnter:
		return m.runCommand(m.paletteItems[m.paletteIdx]), true

	default:
		return nil, false
	}
}

// runCommand executes cmd against the model as its CommandContext, clearing the
// input and palette first and relayouting after (a command may replace the
// conversation view). The returned tea.Cmd, if any, is the command's own (for
// example tea.Quit).
func (m *Model) runCommand(cmd Command) tea.Cmd {
	m.input.Reset()
	m.closePalette()

	result := cmd.Run(m)

	m.layout()
	return result
}

// NewConversation implements CommandContext: it discards the history and
// resets the conversation view.
func (m *Model) NewConversation() {
	m.history = nil
	m.messages = NewMessages()
}

// Notify implements CommandContext: it appends a system note to the view.
func (m *Model) Notify(text string) { m.messages.AddInfo(text) }

// OpenModelSelector implements CommandContext: it opens the model selector and
// starts fetching the catalog, or reports that no lister is wired.
func (m *Model) OpenModelSelector() tea.Cmd {
	if m.lister == nil {
		m.messages.AddInfo("model selector unavailable")
		return nil
	}

	m.modelPickerOpen = true
	m.modelLoading = true
	m.modelErr = nil
	m.modelItems = nil
	m.modelIdx = 0

	return loadModelsCmd(m.lister)
}

// handleModelPickerKey handles a keypress while the model selector is open:
// Up/Down and Tab/Shift+Tab cycle the selection (wrapping), Enter switches to
// the highlighted model, and Esc closes without changing anything. Keys are
// ignored while the catalog is still loading or empty.
func (m *Model) handleModelPickerKey(msg tea.KeyMsg) {
	if msg.Type == tea.KeyEsc {
		m.closeModelPicker()
		return
	}

	n := len(m.modelItems)
	if n == 0 {
		return
	}

	switch msg.Type {
	case tea.KeyUp, tea.KeyShiftTab:
		m.modelIdx = (m.modelIdx - 1 + n) % n

	case tea.KeyDown, tea.KeyTab:
		m.modelIdx = (m.modelIdx + 1) % n

	case tea.KeyEnter:
		m.selectModel(m.modelItems[m.modelIdx])
	}
}

// selectModel switches the active model on the loop and status bar, closes the
// selector, and notes the change in the conversation.
func (m *Model) selectModel(info provider.ModelInfo) {
	m.loop.SetModel(info.ID)
	m.modelName = info.ID
	m.status.SetModel(info.ID)

	m.closeModelPicker()
	m.messages.AddInfo("switched model to " + info.ID)
}

// closeModelPicker hides the selector and clears its state.
func (m *Model) closeModelPicker() {
	m.modelPickerOpen = false
	m.modelLoading = false
	m.modelErr = nil
	m.modelItems = nil
	m.modelIdx = 0
}

// indexOfModel returns the index of the model whose ID equals current, or 0
// when there is no match, so the selector opens on the active model.
func indexOfModel(models []provider.ModelInfo, current string) int {
	for i, info := range models {
		if info.ID == current {
			return i
		}
	}
	return 0
}

// CommandHelp implements CommandContext: the command list from the registry
// followed by the static key-binding section.
func (m *Model) CommandHelp() string {
	return "commands:\n" + m.commands.Help() + "\n\n" + keyBindingsHelp()
}

// closePalette hides the palette and resets its selection.
func (m *Model) closePalette() {
	m.showPalette = false
	m.paletteItems = nil
	m.paletteIdx = 0
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
		m.messages.AddToolCall(msg.Event.ToolCall.Name, permissionDetail(msg.Event.ToolCall.Input))
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
	m.status.SetSpinner("")

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

// isScrollKey reports whether msg is a key that scrolls the conversation view
// rather than editing the prompt. Only page keys are claimed so ordinary
// arrows and text still reach the input; the mouse wheel scrolls too, handled
// separately as a MouseMsg.
func isScrollKey(msg tea.KeyMsg) bool {
	switch msg.Type {
	case tea.KeyPgUp, tea.KeyPgDown:
		return true
	default:
		return false
	}
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

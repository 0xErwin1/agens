package tui

import (
	"context"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/spinner"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/iperez/agens/internal/agentdef"
	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
	"github.com/iperez/agens/internal/session"
	"github.com/iperez/agens/internal/tool/task"
)

// Layout dimensions. The input and status bars have fixed heights; the
// conversation view takes the remaining vertical space.
const (
	inputHeight  = 5
	statusHeight = 1

	// inputGap is a blank row between the conversation and the input, so the
	// last message does not sit flush against the prompt.
	inputGap = 1

	// topPad is the blank space above the first row of content.
	topPad = 1

	// maxContentWidth caps the content column; on wider terminals the column
	// is centered rather than stretched edge to edge, which reads as less
	// sparse. minSidePad is the minimum horizontal breathing room on each side
	// when the terminal is narrower than the cap.
	maxContentWidth = 96
	minSidePad      = 2
)

// State labels shown in the status bar.
const (
	stateThinking = "thinking…"
	stateWriting  = "writing…"
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
	// workLabel describes what the running turn is doing (thinking/writing/
	// running a tool), shown by the inline activity indicator above the input.
	workLabel string
	// queued holds plain messages typed while a turn was running; each is sent
	// automatically, in order, one per successful turn completion.
	queued []string
	// mouseEnabled tracks whether mouse reporting is on. It starts on (the wheel
	// scrolls the conversation) and is toggled off via /select so the terminal's
	// native click-drag text selection works, then back on.
	mouseEnabled bool
	// now supplies the current time (injectable for tests); turnStart marks when
	// the in-flight turn began, driving the live elapsed counter and the final
	// per-turn duration shown in the footer.
	now       func() time.Time
	turnStart time.Time
	// toolClock marks when the current tool began executing, advanced after each
	// tool result so consecutive tools each get their own execution duration.
	toolClock time.Time

	// usage is the latest token accounting reported by the provider; hasUsage
	// gates the footer segment until the first report. contextWindow is the
	// current model's window (0 = unknown), used to show a context percentage.
	// tokensDetailed toggles the compact/verbose token view (Ctrl+P).
	usage          provider.Usage
	hasUsage       bool
	contextWindow  int
	tokensDetailed bool
	// collapseThinking and truncateToolOutput mirror the UI config so the message
	// view's display options can be reapplied after NewConversation rebuilds it.
	collapseThinking   bool
	truncateToolOutput bool
	// quitArmed is set after a Ctrl+C that neither canceled a turn nor cleared
	// the input, so a second Ctrl+C quits; any other key disarms it.
	quitArmed bool
	events    <-chan tea.Msg
	cancel    context.CancelFunc

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
	systemPrompt    SystemPromptFunc
	modelPickerOpen bool
	modelItems      []provider.ModelInfo
	modelIdx        int
	modelLoading    bool
	modelErr        error

	// effort is the current reasoning effort ("" = provider default);
	// effortLevels are the provider's supported values (empty disables the
	// selector); the picker fields hold the selector state while open.
	effort           string
	effortLevels     []string
	effortPickerOpen bool
	effortIdx        int

	// sessions persists and lists conversations (nil disables history);
	// sessionID is the current conversation, minted by newSessionID. The
	// remaining fields hold the session picker's state while it is open.
	// project is the absolute project root the current conversation belongs to;
	// saved sessions are stamped with it and the picker filters to it by
	// default. resumeID and openSessionsOnStart drive the startup behavior of
	// the --resume flag.
	project             string
	resumeID            string
	openSessionsOnStart bool

	sessions          SessionStore
	sessionID         string
	newSessionID      func() string
	sessionPickerOpen bool
	sessionItems      []session.Session
	sessionAll        []session.Session
	sessionShowAll    bool
	sessionIdx        int
	sessionLoading    bool
	sessionErr        error

	// subagentTreeOpen shows the active-subagent tree overlay; subagentIdx is the
	// highlighted row within the flattened tree. subagentFocusID, when non-empty,
	// replaces the conversation with that subagent's live focus view.
	subagentTreeOpen bool
	subagentIdx      int
	subagentFocusID  string

	// agents holds the agent definitions the /agents menu presents and edits (nil
	// disables the menu). The menu is two-level: agentMenuOpen gates it, agentIdx
	// selects an agent in the list, and once agentMenuEditing names an entered
	// agent, agentModelRows/agentModelIdx drive its per-agent model editor.
	agents           *agentdef.Set
	subagents        *task.Catalog
	agentMenuOpen    bool
	agentIdx         int
	agentMenuEditing string
	agentModelRows   []agentModelRow
	agentModelIdx    int

	// files provides the project files for @-references (nil disables them);
	// fileCache is the list loaded once at startup. The picker fields hold the
	// @-picker's state while it is open.
	files          FileSource
	fileCache      []string
	filesLoaded    bool
	filePickerOpen bool
	fileItems      []string
	fileIdx        int

	width, height int
	// contentWidth is the width of the centered content column and leftPad the
	// left offset that centers it; both are derived from width in layout.
	contentWidth, leftPad int
}

var (
	_ tea.Model      = (*Model)(nil)
	_ CommandContext = (*Model)(nil)
)

// SystemPromptFunc rebuilds the system prompt for a model id so the model's
// self-identity stays correct after a live switch. ok is false when a prompt
// could not be built, in which case the current prompt is left unchanged.
type SystemPromptFunc func(model string) (string, bool)

// Deps are the dependencies of the root model. Loop and Model are required;
// the rest are optional: a nil Prompter resolves permissions without a modal,
// a nil Models disables the /model selector, and a nil SystemPrompt leaves the
// system prompt untouched on a model switch.
type Deps struct {
	Loop         LoopRunner
	Model        string
	Prompter     *Prompter
	Models       ModelLister
	SystemPrompt SystemPromptFunc
	EffortLevels []string
	Sessions     SessionStore
	// NewSessionID mints a fresh conversation id; defaults to a counter when
	// nil (tests). Production passes a uuid generator.
	NewSessionID func() string
	Files        FileSource
	// Project is the absolute project root the conversation belongs to; saved
	// sessions are stamped with it and the picker scopes to it by default.
	Project string
	// Agents are the agent definitions the /agents menu presents and edits; nil
	// disables the menu.
	Agents *agentdef.Set
	// Subagents is the live catalog the running loop's task tool reads; when set,
	// an /agents edit updates it so it takes effect this session, not only the
	// next one.
	Subagents *task.Catalog
	// ResumeID, when non-empty, resumes that session on startup. OpenSessions
	// opens the session picker on startup instead. They back the --resume flag.
	ResumeID     string
	OpenSessions bool
	// Now supplies the current time; defaults to time.Now when nil. Tests inject
	// a controlled clock to assert turn timing deterministically.
	Now func() time.Time
	// CollapseThinking folds a finished reasoning block to its header;
	// TruncateToolOutput caps an expanded tool result. Both default to false
	// (shown in full) and come from the user's UI config.
	CollapseThinking   bool
	TruncateToolOutput bool
}

// New constructs the root model from its dependencies.
func New(deps Deps) *Model {
	sp := spinner.New(spinner.WithSpinner(spinner.MiniDot))
	sp.Style = lipgloss.NewStyle().Foreground(CurrentTheme().Accent())

	newID := deps.NewSessionID
	if newID == nil {
		counter := 0
		newID = func() string {
			counter++
			return fmt.Sprintf("session-%d", counter)
		}
	}

	now := deps.Now
	if now == nil {
		now = time.Now
	}

	m := &Model{
		input:               NewInput(),
		status:              NewStatus(deps.Model),
		messages:            NewMessages(),
		spinner:             sp,
		loop:                deps.Loop,
		modelName:           deps.Model,
		prompter:            deps.Prompter,
		commands:            defaultCommands(),
		lister:              deps.Models,
		systemPrompt:        deps.SystemPrompt,
		effortLevels:        deps.EffortLevels,
		sessions:            deps.Sessions,
		newSessionID:        newID,
		sessionID:           newID(),
		files:               deps.Files,
		project:             deps.Project,
		agents:              deps.Agents,
		subagents:           deps.Subagents,
		resumeID:            deps.ResumeID,
		openSessionsOnStart: deps.OpenSessions,
		now:                 now,
		collapseThinking:    deps.CollapseThinking,
		truncateToolOutput:  deps.TruncateToolOutput,
		mouseEnabled:        true,
	}
	m.messages.SetDisplayOptions(m.collapseThinking, m.truncateToolOutput)
	m.messages.SetClock(m.now)
	return m
}

// Init focuses the input and, when an interactive prompter is installed,
// starts listening for permission requests.
func (m *Model) Init() tea.Cmd {
	cmds := []tea.Cmd{m.input.Focus()}
	if m.prompter != nil {
		cmds = append(cmds, waitForPermission(m.prompter.Requests()))
	}
	if m.files != nil {
		cmds = append(cmds, loadFilesCmd(m.files))
	}
	if m.lister != nil {
		// Fetch the catalog in the background so the footer can show the current
		// model's context percentage without waiting for the user to open the
		// /model selector.
		cmds = append(cmds, loadModelsCmd(m.lister))
	}
	if m.sessions != nil {
		switch {
		case m.resumeID != "":
			cmds = append(cmds, resumeSessionCmd(m.sessions, m.resumeID))
		case m.openSessionsOnStart:
			cmds = append(cmds, m.OpenSessionPicker())
		}
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
		_, cmd := m.activeMessages().Update(msg)
		cmds = append(cmds, cmd)

	case tea.KeyMsg:
		// Any key other than Ctrl+C disarms the double-Ctrl+C-to-quit prompt.
		if msg.Type != tea.KeyCtrlC {
			m.quitArmed = false
		}
		if isScrollKey(msg) {
			swallow = true
			_, cmd := m.activeMessages().Update(msg)
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
		if m.effortPickerOpen {
			swallow = true
			m.handleEffortPickerKey(msg)
			break
		}
		if m.sessionPickerOpen {
			swallow = true
			m.handleSessionPickerKey(msg)
			break
		}
		if m.agentMenuOpen {
			swallow = true
			m.handleAgentMenuKey(msg)
			break
		}
		if m.subagentTreeOpen {
			swallow = true
			m.handleSubagentTreeKey(msg)
			break
		}
		if m.subagentFocusID != "" && msg.Type != tea.KeyCtrlC {
			swallow = true
			cmds = append(cmds, m.handleSubagentFocusKey(msg))
			break
		}
		if m.filePickerOpen {
			if m.handleFilePickerKey(msg) {
				swallow = true
				break
			}
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
			if quit := m.handleCtrlC(); quit {
				return m, tea.Quit
			}
		case tea.KeyCtrlUp:
			swallow = true
			cmds = append(cmds, m.OpenSubagentTree())
		case tea.KeyCtrlO:
			swallow = true
			m.activeMessages().ToggleDetails()
		case tea.KeyCtrlP:
			swallow = true
			m.tokensDetailed = !m.tokensDetailed
			m.status.SetTokens(m.tokenSummary())
		case tea.KeyEnter:
			swallow = true
			cmds = append(cmds, m.onEnter())
		}

	case modelsLoadedMsg:
		m.modelLoading = false
		m.modelErr = msg.err
		m.modelItems = msg.models
		m.modelIdx = indexOfModel(msg.models, m.modelName)
		m.contextWindow = contextWindowFor(msg.models, m.modelName)
		m.status.SetTokens(m.tokenSummary())

	case sessionsLoadedMsg:
		m.sessionLoading = false
		m.sessionErr = msg.err
		m.sessionAll = msg.sessions
		m.applySessionFilter()

	case sessionResumeMsg:
		if msg.err != nil {
			m.messages.AddInfo("could not resume session: " + humanizeError(msg.err.Error()))
		} else {
			m.applyResumedSession(msg.sess)
		}

	case filesLoadedMsg:
		m.filesLoaded = true
		m.fileCache = msg.files
		if msg.err != nil {
			m.messages.AddInfo("file references unavailable: " + msg.err.Error())
		}

	case spinner.TickMsg:
		// Ignore ticks that arrive after the turn ended; otherwise a stray
		// tick repaints the spinner next to "ready" with no further ticks to
		// clear it, leaving the loader stuck.
		if m.running {
			var cmd tea.Cmd
			m.spinner, cmd = m.spinner.Update(msg)
			m.status.SetSpinner(m.spinner.View())
			cmds = append(cmds, cmd)
		}

	case StreamMsg:
		cmds = append(cmds, m.handleStream(msg))

	case TurnDoneMsg:
		cmds = append(cmds, m.handleDone(msg))

	case subagentExpiryMsg:
		// Reaching Update re-renders the view, which drops any finished subagent
		// whose linger window has elapsed from the active-subagent tree.
	}

	if !swallow {
		_, cmd := m.input.Update(msg)
		cmds = append(cmds, cmd)

		if _, ok := msg.(tea.KeyMsg); ok {
			m.refreshCompletions()
		}
	}

	return m, tea.Batch(cmds...)
}

// refreshCompletions recomputes the input-driven overlays after a keystroke.
// An in-progress @-reference takes precedence over the slash palette; the two
// never show at once.
func (m *Model) refreshCompletions() {
	if m.refreshFilePicker() {
		m.showPalette = false
		return
	}
	m.filePickerOpen = false
	m.refreshPalette()
}

// refreshFilePicker opens or updates the @-reference picker from the current
// input, returning whether it is active. It is inactive when no file source is
// wired, a turn is running, or the input has no in-progress @-reference.
func (m *Model) refreshFilePicker() bool {
	token, _, ok := atToken(m.input.Value())
	if m.files == nil || m.running || !ok {
		m.filePickerOpen = false
		return false
	}

	m.filePickerOpen = true
	m.fileItems = filterFiles(m.fileCache, token)
	if m.fileIdx >= len(m.fileItems) {
		m.fileIdx = 0
	}
	return true
}

// handleFilePickerKey handles a keypress while the @-picker is open. It reports
// whether it consumed the key: navigation, insertion (Enter), and dismissal
// (Esc) are consumed; anything else falls through so typing keeps filtering.
func (m *Model) handleFilePickerKey(msg tea.KeyMsg) bool {
	n := len(m.fileItems)

	switch msg.Type {
	case tea.KeyUp, tea.KeyShiftTab:
		if n > 0 {
			m.fileIdx = (m.fileIdx - 1 + n) % n
		}
		return true

	case tea.KeyDown, tea.KeyTab:
		if n > 0 {
			m.fileIdx = (m.fileIdx + 1) % n
		}
		return true

	case tea.KeyEsc:
		m.filePickerOpen = false
		return true

	case tea.KeyEnter:
		if n > 0 {
			m.insertFileRef(m.fileItems[m.fileIdx])
		}
		return true

	default:
		return false
	}
}

// insertFileRef replaces the in-progress @-reference in the input with the
// chosen path followed by a space, which ends the reference and closes the
// picker.
func (m *Model) insertFileRef(path string) {
	value := m.input.Value()
	_, start, ok := atToken(value)
	if !ok {
		return
	}

	m.input.SetValue(value[:start] + "@" + path + " ")
	m.filePickerOpen = false
}

// expandFileRefs appends the contents of every @-referenced project file to
// text, so the model sees them. Unknown references are left as-is; each file is
// capped at maxFileRefBytes. failed lists the known files whose read failed, so
// the caller can tell the user their reference was dropped rather than dropping
// it silently.
func (m *Model) expandFileRefs(text string) (expanded string, failed []string) {
	if m.files == nil {
		return text, nil
	}

	known := make(map[string]struct{}, len(m.fileCache))
	for _, f := range m.fileCache {
		known[f] = struct{}{}
	}

	refs := extractFileRefs(text, known)
	if len(refs) == 0 {
		return text, nil
	}

	var b strings.Builder
	b.WriteString(text)
	for _, path := range refs {
		content, err := m.files.Read(path)
		if err != nil {
			failed = append(failed, path)
			continue
		}
		if len(content) > maxFileRefBytes {
			content = content[:maxFileRefBytes] + "\n… (truncated)"
		}
		b.WriteString("\n\n--- " + path + " ---\n")
		b.WriteString(content)
	}
	return b.String(), failed
}

// View composes the fixed frame — conversation, prompt input, footer — and
// then floats any active overlay (permission modal or command palette) just
// above the input. Overlays are composited on top of the conversation rather
// than inserted into the layout, so the chat never resizes or scrolls when one
// appears.
func (m *Model) View() string {
	// Entering a subagent takes over the whole frame: its conversation renders
	// like the main thread (scrollable) under a small header, with no input.
	if s := m.focusedSubagent(); s != nil {
		return frameView(m.subagentFocusView(s), m.leftPad, topPad)
	}

	// The gap row above the input doubles as the inline activity indicator
	// while a turn runs, so "thinking/writing" is visible next to the
	// conversation rather than only in the footer.
	gap := ""
	switch {
	case m.running:
		gap = m.workingLine()
	case m.quitArmed:
		gap = lipgloss.NewStyle().MarginLeft(contentGutter).
			Foreground(CurrentTheme().Muted()).
			Render("press ctrl+c again to quit")
	}

	base := lipgloss.JoinVertical(lipgloss.Left,
		m.messages.View(),
		gap,
		m.input.View(),
		m.status.View(),
	)

	// The input begins after the conversation view and the gap row; overlays
	// end on the row just above it.
	inputRow := m.messages.height + inputGap

	switch {
	case m.pending != nil:
		base = overlayAbove(base, renderPermission(m.pending.Call, m.contentWidth), inputRow)
	case m.modelPickerOpen:
		overlay := renderModelSelector(m.modelItems, m.modelIdx, m.modelLoading, m.modelErr, m.modelName, m.contentWidth)
		base = overlayAbove(base, overlay, inputRow)
	case m.effortPickerOpen:
		base = overlayAbove(base, renderEffortSelector(m.effortLevels, m.effortIdx, m.effort, m.contentWidth), inputRow)
	case m.sessionPickerOpen:
		overlay := renderSessionSelector(m.sessionItems, m.sessionIdx, m.sessionLoading, m.sessionErr, m.sessionShowAll, m.contentWidth)
		base = overlayAbove(base, overlay, inputRow)
	case m.agentMenuOpen:
		base = overlayAbove(base, m.agentMenuView(), inputRow)
	case m.subagentTreeOpen:
		base = overlayAbove(base, renderSubagentTree(m.messages.treeSubagents(), m.subagentIdx, m.contentWidth), inputRow)
	case m.filePickerOpen:
		overlay := renderFileSelector(m.fileItems, m.fileIdx, !m.filesLoaded, m.contentWidth)
		base = overlayAbove(base, overlay, inputRow)
	case m.showPalette:
		base = overlayAbove(base, renderPalette(m.paletteItems, m.paletteIdx, m.contentWidth), inputRow)
	}

	return frameView(base, m.leftPad, topPad)
}

// frameView centers the composed view by prefixing every line with leftPad
// spaces and adds topPad blank rows above it, giving the content top and side
// breathing room without touching the components themselves.
func frameView(s string, leftPad, topPad int) string {
	pad := strings.Repeat(" ", leftPad)

	lines := strings.Split(s, "\n")
	out := make([]string, 0, topPad+len(lines))
	for i := 0; i < topPad; i++ {
		out = append(out, "")
	}
	for _, line := range lines {
		out = append(out, pad+line)
	}

	return strings.Join(out, "\n")
}

// workingLine renders the inline activity indicator: the animated spinner
// followed by what the turn is currently doing and the elapsed time since it
// began, aligned to the shared gutter. The elapsed segment updates on every
// spinner tick.
func (m *Model) workingLine() string {
	muted := lipgloss.NewStyle().Foreground(CurrentTheme().Muted())

	text := m.workLabel
	if !m.turnStart.IsZero() {
		text += " " + formatDuration(m.now().Sub(m.turnStart))
	}
	if n := m.messages.RunningSubagents(); n > 0 {
		text += fmt.Sprintf(" · %d subagent(s) · /subagents", n)
	}
	if n := len(m.queued); n > 0 {
		text += fmt.Sprintf(" · %d en cola", n)
	}

	return lipgloss.NewStyle().MarginLeft(contentGutter).Render(m.spinner.View() + " " + muted.Render(text))
}

// formatDuration renders a turn's elapsed time compactly: one decimal below ten
// seconds, whole seconds below a minute, and m:ss above it.
func formatDuration(d time.Duration) string {
	switch {
	case d < 10*time.Second:
		return fmt.Sprintf("%.1fs", d.Seconds())
	case d < time.Minute:
		return fmt.Sprintf("%ds", int(d.Seconds()))
	default:
		return fmt.Sprintf("%dm%02ds", int(d.Minutes()), int(d.Seconds())%60)
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
	m.contentWidth = m.width - 2*minSidePad
	if m.contentWidth > maxContentWidth {
		m.contentWidth = maxContentWidth
	}
	if m.contentWidth < 1 {
		m.contentWidth = 1
	}
	m.leftPad = (m.width - m.contentWidth) / 2
	if m.leftPad < 0 {
		m.leftPad = 0
	}

	msgHeight := m.height - inputHeight - statusHeight - inputGap - topPad
	if msgHeight < 0 {
		msgHeight = 0
	}

	m.messages.SetSize(m.contentWidth, msgHeight)
	m.status.SetSize(m.contentWidth, statusHeight)
	m.input.SetSize(m.contentWidth, inputHeight)
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
	m.filePickerOpen = false
	return m.submitText(text)
}

// submitText records text as a new user turn, shows it, marks the model busy,
// and starts the turn goroutine. It backs both a live submission (submit) and
// the automatic sending of a queued message when the previous turn finishes.
func (m *Model) submitText(text string) tea.Cmd {
	// The model receives the message with @-referenced files inlined; the
	// conversation shows the original text the user typed.
	expanded, failed := m.expandFileRefs(text)
	m.history = append(m.history, message.NewMessage(message.RoleUser, message.TextPart{Text: expanded}))
	m.messages.AppendUser(text)
	if len(failed) > 0 {
		m.messages.AddInfo("could not read referenced file(s): " + strings.Join(failed, ", "))
	}
	m.status.SetState(stateThinking)
	m.status.SetDuration("")
	m.workLabel = stateThinking
	m.running = true
	m.turnStart = m.now()

	ctx, cancel := context.WithCancel(context.Background())
	m.cancel = cancel
	m.events = runTurn(ctx, m.loop, m.history)

	return tea.Batch(waitFor(m.events), m.spinner.Tick)
}

// enqueueMessage stores the current input as a pending message to be sent when
// the running turn finishes, then clears the input. It backs typing ahead while
// the agent is busy.
func (m *Model) enqueueMessage() {
	text := m.input.Value()
	m.queued = append(m.queued, text)
	m.input.Reset()
	m.filePickerOpen = false
	m.closePalette()
	m.messages.AddInfo(fmt.Sprintf("queued (%d): %s", len(m.queued), truncateTitle(strings.TrimSpace(text))))
	m.layout()
}

// drainQueued sends the next pending message, if any, as a fresh turn. It is
// called after a turn completes successfully so queued follow-ups flow one per
// completed turn.
func (m *Model) drainQueued() tea.Cmd {
	if len(m.queued) == 0 {
		return nil
	}
	next := m.queued[0]
	m.queued = m.queued[1:]
	return m.submitText(next)
}

// onEnter handles the Enter key when the palette has not already consumed it:
// a slash command is run (unknown ones report an error note), an empty or
// in-flight input is ignored, and anything else is submitted as a chat turn.
func (m *Model) onEnter() tea.Cmd {
	value := strings.TrimSpace(m.input.Value())
	if value == "" {
		return nil
	}

	if strings.HasPrefix(value, "/") {
		c, ok := m.commands.Lookup(value)
		if !ok {
			m.input.Reset()
			m.closePalette()
			m.messages.AddInfo("unknown command: " + value)
			m.layout()
			return nil
		}

		// A command that would mutate the in-flight turn or the loop's live
		// settings is refused while a turn runs; safe ones (help, select, the
		// subagent overlays, quit) run immediately.
		if m.running && !c.SafeWhileRunning {
			m.input.Reset()
			m.closePalette()
			m.messages.AddInfo(c.Name + " is not available while a turn is running")
			m.layout()
			return nil
		}

		return m.runCommand(c)
	}

	// A plain message typed while a turn is running is queued and sent when the
	// turn finishes, so the user can line up follow-ups without waiting.
	if m.running {
		m.enqueueMessage()
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

	m.paletteItems = m.commands.MatchRunnable(m.input.Value(), m.running)

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

// NewConversation implements CommandContext: it discards the history, resets
// the conversation view, and starts a fresh session so the next turns are
// saved separately from the previous conversation.
func (m *Model) NewConversation() {
	m.history = nil
	m.messages = NewMessages()
	m.messages.SetDisplayOptions(m.collapseThinking, m.truncateToolOutput)
	m.messages.SetClock(m.now)
	m.sessionID = m.newSessionID()
	m.subagentFocusID = ""
	m.subagentTreeOpen = false
	m.layout()
}

// saveSession persists the current conversation. It is a no-op when there is
// no store or no history, and surfaces a write failure as a note rather than
// interrupting the turn.
func (m *Model) saveSession() {
	if m.sessions == nil || len(m.history) == 0 {
		return
	}

	err := m.sessions.Save(session.Session{
		ID:       m.sessionID,
		Title:    sessionTitle(m.history),
		Project:  m.project,
		Messages: m.history,
	})
	if err != nil {
		m.messages.AddInfo("could not save session: " + err.Error())
	}
}

// OpenSessionPicker implements CommandContext: it opens the session picker and
// starts listing saved conversations, or reports that history is unavailable.
func (m *Model) OpenSessionPicker() tea.Cmd {
	if m.sessions == nil {
		m.messages.AddInfo("session history not available")
		return nil
	}

	m.sessionPickerOpen = true
	m.sessionLoading = true
	m.sessionErr = nil
	m.sessionItems = nil
	m.sessionIdx = 0

	return loadSessionsCmd(m.sessions)
}

// handleSessionPickerKey handles a keypress while the session picker is open:
// Up/Down and Tab/Shift+Tab cycle the selection (wrapping), Enter resumes the
// highlighted conversation, Ctrl+A toggles between this project and all
// projects, and Esc closes. Keys are ignored while loading or when the list is
// empty.
func (m *Model) handleSessionPickerKey(msg tea.KeyMsg) {
	if msg.Type == tea.KeyEsc {
		m.closeSessionPicker()
		return
	}

	// Ctrl+A toggles between this project and every project. It is handled
	// before the empty-list guard so an empty this-project view can still be
	// widened to all projects.
	if msg.Type == tea.KeyCtrlA {
		m.sessionShowAll = !m.sessionShowAll
		m.applySessionFilter()
		return
	}

	n := len(m.sessionItems)
	if n == 0 {
		return
	}

	switch msg.Type {
	case tea.KeyUp, tea.KeyShiftTab:
		m.sessionIdx = (m.sessionIdx - 1 + n) % n

	case tea.KeyDown, tea.KeyTab:
		m.sessionIdx = (m.sessionIdx + 1) % n

	case tea.KeyEnter:
		m.resumeSession(m.sessionItems[m.sessionIdx])
	}
}

// resumeSession loads the chosen conversation from the picker into the current
// view and closes the picker.
func (m *Model) resumeSession(meta session.Session) {
	sess, err := m.sessions.Load(meta.ID)
	if err != nil {
		m.closeSessionPicker()
		m.messages.AddInfo("could not load session: " + humanizeError(err.Error()))
		return
	}

	m.applyResumedSession(sess)
	m.closeSessionPicker()
}

// applyResumedSession adopts a loaded conversation's history and id so
// subsequent turns append to it. It is shared by the picker and the startup
// --resume path.
func (m *Model) applyResumedSession(sess session.Session) {
	m.history = sess.Messages
	m.sessionID = sess.ID
	if sess.Project != "" {
		m.project = sess.Project
	}
	m.messages.SetHistory(sess.Messages)
	m.layout()
}

// applySessionFilter recomputes the visible session list from the full loaded
// set: scoped to the current project, or every session when show-all is active
// or no project is known. The selection resets to the top.
func (m *Model) applySessionFilter() {
	if m.sessionShowAll || m.project == "" {
		m.sessionItems = m.sessionAll
	} else {
		filtered := make([]session.Session, 0, len(m.sessionAll))
		for _, s := range m.sessionAll {
			if s.Project == m.project {
				filtered = append(filtered, s)
			}
		}
		m.sessionItems = filtered
	}
	m.sessionIdx = 0
}

// closeSessionPicker hides the picker and clears its state.
func (m *Model) closeSessionPicker() {
	m.sessionPickerOpen = false
	m.sessionLoading = false
	m.sessionErr = nil
	m.sessionItems = nil
	m.sessionAll = nil
	m.sessionShowAll = false
	m.sessionIdx = 0
}

// Notify implements CommandContext: it appends a system note to the view.
func (m *Model) Notify(text string) { m.messages.AddInfo(text) }

// ToggleMouse implements CommandContext: it flips mouse reporting. Turning it off
// hands click-drag back to the terminal so the user can select and copy text
// (losing wheel scroll until re-enabled); turning it on restores wheel scroll.
func (m *Model) ToggleMouse() tea.Cmd {
	m.mouseEnabled = !m.mouseEnabled
	if m.mouseEnabled {
		m.messages.AddInfo("mouse on: the wheel scrolls the conversation again")
		return tea.EnableMouseCellMotion
	}
	m.messages.AddInfo("mouse off: drag to select and copy text; run /select again to restore scrolling")
	return tea.DisableMouse
}

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

	// Rebuild the system prompt so its model-identity block matches the new
	// model; otherwise the assistant keeps reporting the previous one.
	if m.systemPrompt != nil {
		if prompt, ok := m.systemPrompt(info.ID); ok {
			m.loop.SetSystemPrompt(prompt)
		}
	}

	m.modelName = info.ID
	m.status.SetModel(info.ID)
	m.contextWindow = info.ContextWindow
	m.status.SetTokens(m.tokenSummary())

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

// OpenEffortSelector implements CommandContext: it opens the reasoning-effort
// selector positioned on the current effort, or reports that the provider has
// no reasoning effort to choose.
func (m *Model) OpenEffortSelector() tea.Cmd {
	if len(m.effortLevels) == 0 {
		m.messages.AddInfo("reasoning effort not available for this provider")
		return nil
	}

	m.effortPickerOpen = true
	m.effortIdx = indexOfEffort(m.effortLevels, m.effort)
	return nil
}

// handleEffortPickerKey handles a keypress while the effort selector is open:
// Up/Down and Tab/Shift+Tab cycle (wrapping), Enter applies the effort, and
// Esc closes without changing it.
func (m *Model) handleEffortPickerKey(msg tea.KeyMsg) {
	n := len(m.effortLevels)

	switch msg.Type {
	case tea.KeyUp, tea.KeyShiftTab:
		m.effortIdx = (m.effortIdx - 1 + n) % n

	case tea.KeyDown, tea.KeyTab:
		m.effortIdx = (m.effortIdx + 1) % n

	case tea.KeyEsc:
		m.effortPickerOpen = false

	case tea.KeyEnter:
		m.selectEffort(m.effortLevels[m.effortIdx])
	}
}

// selectEffort applies the chosen effort to the loop and status bar and closes
// the selector.
func (m *Model) selectEffort(effort string) {
	m.effort = effort
	m.loop.SetEffort(effort)
	m.status.SetEffort(effort)

	m.effortPickerOpen = false
	m.messages.AddInfo("reasoning effort set to " + effort)
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

// contextWindowFor returns the context window of the model whose ID equals
// current, or 0 when it is unknown (no match, or the provider does not report
// one), in which case the footer omits the context percentage.
func contextWindowFor(models []provider.ModelInfo, current string) int {
	for _, info := range models {
		if info.ID == current {
			return info.ContextWindow
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

// handleCtrlC resolves a Ctrl+C press and reports whether the program should
// quit. While a turn runs it cancels the turn; with text in the input it
// clears the input; otherwise the first press arms a quit prompt and a second
// consecutive press quits.
func (m *Model) handleCtrlC() (quit bool) {
	switch {
	case m.running:
		m.abort()
		m.quitArmed = false
	case m.input.Value() != "":
		m.input.Reset()
		m.refreshCompletions()
		m.quitArmed = false
	case m.quitArmed:
		return true
	default:
		m.quitArmed = true
	}
	return false
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
	var followup tea.Cmd

	switch msg.Event.Kind {
	case agentloop.LoopIterationStart:
		m.messages.StartAssistant()

	case agentloop.LoopReasoningDelta:
		m.workLabel = stateThinking
		m.messages.AppendReasoningDelta(msg.Event.Text)

	case agentloop.LoopTextDelta:
		m.workLabel = stateWriting
		m.status.SetState(stateWriting)
		m.messages.FinishReasoning()
		m.messages.AppendAssistantDelta(msg.Event.Text)

	case agentloop.LoopToolCallStarted:
		// The tool call's arguments are not known yet (they stream after the
		// start), so the call block itself is added at LoopMessageDone, where
		// the assembled input yields its detail. Here we only reflect activity.
		m.workLabel = stateRunning + msg.Event.ToolCall.Name
		m.messages.FinishReasoning()
		m.messages.FinishAssistant()
		m.status.SetState(stateRunning + msg.Event.ToolCall.Name)

	case agentloop.LoopToolResult:
		result := msg.Event.ToolResult
		dur := m.now().Sub(m.toolClock)
		m.toolClock = m.now()
		m.messages.CompleteToolCall(result.ToolUseID, toolResultText(result), result.IsError, dur)

	case agentloop.LoopToolBatchFinished:
		batch := msg.Event.ToolBatch
		m.messages.CompleteLatestToolBatch(batch.Total, batch.Completed, batch.Failed)

	case agentloop.LoopSubagentStarted:
		s := msg.Event.Subagent
		m.messages.StartSubagent(s.ID, s.ParentID, s.Name, s.Model, s.Prompt)

	case agentloop.LoopSubagentActivity:
		m.handleSubagentActivity(msg.Event.Subagent)

	case agentloop.LoopSubagentFinished:
		s := msg.Event.Subagent
		m.messages.SetSubagentResult(s.ID, s.Result)
		m.messages.CompleteSubagent(s.ID, s.Failed, 0)
		// Re-render once the finished subagent's linger window elapses so it drops
		// off the active-subagent tree.
		followup = subagentExpiryCmd()

	case agentloop.LoopMessageDone:
		m.messages.FinishReasoning()
		m.messages.FinishAssistant()
		m.addToolCalls(msg.Event.Message)
		// Tools execute after the message is finalized; start the per-tool clock
		// here so the first result's duration is measured from this point.
		m.toolClock = m.now()

	case agentloop.LoopUsage:
		if msg.Event.Usage != nil {
			m.usage = *msg.Event.Usage
			m.hasUsage = true
			m.status.SetTokens(m.tokenSummary())
		}
	}

	return tea.Batch(waitFor(m.events), followup)
}

// handleSubagentActivity applies one forwarded subagent event: it feeds the
// subagent's own conversation view (so entering it renders like the main thread)
// and updates the compact inline panel — its tool summary and running token
// total.
func (m *Model) handleSubagentActivity(s agentloop.Subagent) {
	if s.Event == nil {
		return
	}
	ev := *s.Event

	m.messages.ApplySubagentStream(s.ID, ev)

	switch ev.Kind {
	case agentloop.LoopMessageDone:
		if ev.Message == nil {
			return
		}
		for _, part := range ev.Message.Parts {
			if call, ok := part.(message.ToolUsePart); ok {
				m.messages.AddSubagentTool(s.ID, call.ID, call.Name, permissionDetail(call.Input))
			}
		}

	case agentloop.LoopToolResult:
		m.messages.CompleteSubagentTool(s.ID, ev.ToolResult.ToolUseID, toolResultText(ev.ToolResult), ev.ToolResult.IsError)

	case agentloop.LoopUsage:
		if ev.Usage != nil {
			m.messages.UpdateSubagentProgress(s.ID, ev.Usage.InputTokens+ev.Usage.OutputTokens, 0)
		}
	}
}

// subagentExpiryMsg fires after a finished subagent's linger window so the
// active-subagent tree re-renders and drops it.
type subagentExpiryMsg struct{}

// subagentExpiryCmd schedules a subagentExpiryMsg after the linger window.
func subagentExpiryCmd() tea.Cmd {
	return tea.Tick(subagentListLinger, func(time.Time) tea.Msg { return subagentExpiryMsg{} })
}

// tokenSummary renders the footer's token/context segment from the latest usage
// report: a compact total with a context percentage, or an input/output
// breakdown when detailed view is toggled (Ctrl+P). It is empty until the first
// usage report arrives.
func (m *Model) tokenSummary() string {
	if !m.hasUsage {
		return ""
	}

	total := m.usage.InputTokens + m.usage.OutputTokens
	pct := m.contextPct()

	if m.tokensDetailed {
		s := "in " + formatTokens(m.usage.InputTokens) + " · out " + formatTokens(m.usage.OutputTokens)
		if pct != "" {
			s += " · " + pct
		}
		return s
	}

	s := formatTokens(total)
	if pct != "" {
		s += " (" + pct + ")"
	}
	return s
}

// contextPct renders the share of the model's context window the conversation
// occupies, or "" when the window is unknown.
func (m *Model) contextPct() string {
	if m.contextWindow <= 0 {
		return ""
	}
	total := m.usage.InputTokens + m.usage.OutputTokens
	return fmt.Sprintf("%d%%", total*100/m.contextWindow)
}

// formatTokens renders a token count compactly: bare below a thousand, K with
// one decimal below a million, M above.
func formatTokens(n int) string {
	switch {
	case n < 1000:
		return strconv.Itoa(n)
	case n < 1_000_000:
		return fmt.Sprintf("%.1fK", float64(n)/1000)
	default:
		return fmt.Sprintf("%.1fM", float64(n)/1_000_000)
	}
}

// addToolCalls appends finalized tool invocations from the assistant message.
// Multiple calls from the same message render as one batch header followed by
// the same individually expandable child tool rows used for standalone calls.
func (m *Model) addToolCalls(msg *message.Message) {
	if msg == nil {
		return
	}
	// A task delegation is shown live by the subagent panel (driven by the
	// LoopSubagent* events), so it is not also rendered as a tool block.
	calls := filterOutTask(toolUsesInMessage(*msg))
	if len(calls) == 0 {
		return
	}
	if len(calls) > 1 {
		m.messages.AddToolBatch(calls)
		return
	}
	call := calls[0]
	m.messages.AddToolCall(call.ID, call.Name, permissionDetail(call.Input))
}

// filterOutTask drops task tool calls, whose delegation is represented by the
// subagent panel rather than a tool block.
func filterOutTask(calls []message.ToolUsePart) []message.ToolUsePart {
	out := make([]message.ToolUsePart, 0, len(calls))
	for _, c := range calls {
		if c.Name != "task" {
			out = append(out, c)
		}
	}
	return out
}

// handleDone finalizes a completed turn: it clears the running state, adopts
// the grown history, and reflects success or failure in the status bar. A
// canceled turn is treated as a clean stop rather than an error.
// authErrorHint is the actionable note shown below an authentication failure,
// pointing the user at the command that restores their credentials.
const authErrorHint = "Your credentials are missing or expired. Run `agens auth login` to sign in again."

func (m *Model) handleDone(msg TurnDoneMsg) tea.Cmd {
	m.running = false
	if m.cancel != nil {
		m.cancel()
		m.cancel = nil
	}
	m.events = nil
	m.status.SetSpinner("")

	if !m.turnStart.IsZero() {
		m.status.SetDuration(formatDuration(m.now().Sub(m.turnStart)))
	}

	if msg.History != nil {
		m.history = msg.History
	}

	// A canceled turn is a deliberate stop: end cleanly and drop any queued
	// follow-ups the user typed ahead, since they meant to halt.
	if msg.Err != nil && errors.Is(msg.Err, context.Canceled) {
		m.queued = nil
		m.status.SetState(stateReady)
		m.saveSession()
		return nil
	}

	if msg.Err != nil {
		m.messages.SetError(humanizeError(msg.Err.Error()))
		if provider.IsAuthError(msg.Err) {
			m.messages.AddInfo(authErrorHint)
		}
		m.status.SetState(stateError)
		// Queued messages are kept but not auto-sent into a failing turn; the
		// next successful completion drains them.
		if n := len(m.queued); n > 0 {
			m.messages.AddInfo(fmt.Sprintf("%d queued message(s) still pending", n))
		}
		return nil
	}

	m.status.SetState(stateReady)
	m.saveSession()
	return m.drainQueued()
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

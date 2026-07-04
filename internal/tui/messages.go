package tui

import (
	"strings"

	"github.com/charmbracelet/bubbles/viewport"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/glamour"
	"github.com/charmbracelet/lipgloss"

	"github.com/iperez/agens/internal/message"
)

// Visible markers layered on top of the theme colors. The user turn is set
// off by a colored left bar (like opencode) rather than a text label; tool and
// error lines keep a short glyph/word so they read even without color.
const (
	labelToolCall    = "→ "
	labelToolError   = "error: "
	labelErrorBlock  = "error: "
	labelThinking    = "Thinking"
	blockSeparator   = "\n\n"
	toolResultIndent = "  "

	// contentGutter is the left indent applied to every non-user block so the
	// conversation aligns with glamour's built-in document margin (2) instead
	// of hugging the terminal edge.
	contentGutter = 2
)

// Tool results can be arbitrarily large (a whole file, a long command dump).
// They are truncated before rendering so a single result cannot blow up the
// viewport or dominate the scrollback.
const (
	toolResultMaxLines = 15
	toolResultMaxBytes = 2048
	truncationMarker   = "… (truncated)"
)

// blockKind identifies how a finalized conversation block is rendered.
type blockKind int

const (
	blockUser blockKind = iota
	blockAssistant
	blockToolCall
	blockToolResult
	blockToolError
	blockError
	blockSystem
	blockReasoning
)

// block is a finalized conversation entry kept in its raw form so it can be
// re-rendered (and, for assistant markdown, re-wrapped) whenever the width
// changes. detail carries a secondary, muted string (currently the tool
// call's argument, e.g. the shell command) shown after the primary text.
type block struct {
	kind   blockKind
	text   string
	detail string
}

// Messages is the scrollable conversation view. Finalized turns are kept as
// raw blocks and styled on every rebuild; the in-progress assistant response is
// accumulated separately and rendered live (plain, never markdown) until
// FinishAssistant commits it. After every mutation the viewport content is
// rebuilt and scrolled to the bottom.
type Messages struct {
	vp              viewport.Model
	blocks          []block
	streaming       string
	streamingActive bool
	reasoning       string
	reasoningActive bool
	width, height   int

	// renderer is width-bound: a glamour.TermRenderer wraps to a fixed width,
	// so it is rebuilt whenever the width changes rather than per render.
	renderer *glamour.TermRenderer
}

// NewMessages constructs an empty conversation view. Its viewport is sized
// later via SetSize.
func NewMessages() *Messages {
	return &Messages{vp: viewport.New(0, 0)}
}

var _ Component = (*Messages)(nil)

// AppendUser adds a finalized user block.
func (m *Messages) AppendUser(text string) {
	m.blocks = append(m.blocks, block{kind: blockUser, text: text})
	m.rebuild()
}

// StartAssistant begins a new streaming assistant block, discarding any
// uncommitted streaming text from a previous, unfinished response.
func (m *Messages) StartAssistant() {
	m.streaming = ""
	m.streamingActive = true
	m.rebuild()
}

// AppendAssistantDelta appends text to the in-progress assistant response.
func (m *Messages) AppendAssistantDelta(text string) {
	m.streamingActive = true
	m.streaming += text
	m.rebuild()
}

// AppendReasoningDelta appends text to the in-progress reasoning ("thinking")
// stream, shown live above the answer.
func (m *Messages) AppendReasoningDelta(text string) {
	m.reasoningActive = true
	m.reasoning += text
	m.rebuild()
}

// FinishReasoning commits the in-progress reasoning to a finalized block. It is
// a no-op when there is no active, non-empty reasoning, so callers may invoke
// it defensively when the answer or a tool call begins.
func (m *Messages) FinishReasoning() {
	if !m.reasoningActive || m.reasoning == "" {
		m.reasoningActive = false
		m.reasoning = ""
		return
	}

	m.blocks = append(m.blocks, block{kind: blockReasoning, text: m.reasoning})
	m.reasoning = ""
	m.reasoningActive = false
	m.rebuild()
}

// FinishAssistant commits the in-progress assistant response to the block
// list. It is a no-op when there is no active, non-empty streaming text, so
// the root model may call it defensively (both when a tool call interrupts the
// text and when the message completes) without producing empty blocks.
func (m *Messages) FinishAssistant() {
	if !m.streamingActive || m.streaming == "" {
		m.streamingActive = false
		m.streaming = ""
		return
	}

	m.blocks = append(m.blocks, block{kind: blockAssistant, text: m.streaming})
	m.streaming = ""
	m.streamingActive = false
	m.rebuild()
}

// AddToolCall adds a block marking that a tool invocation has started. detail
// is a short description of what the tool acts on (the shell command, a path),
// shown muted after the tool name; it may be empty.
func (m *Messages) AddToolCall(name, detail string) {
	m.blocks = append(m.blocks, block{kind: blockToolCall, text: name, detail: detail})
	m.rebuild()
}

// AddToolResult adds a block carrying a tool's result. isError selects the
// error styling so a failed result is visually distinct.
func (m *Messages) AddToolResult(text string, isError bool) {
	kind := blockToolResult
	if isError {
		kind = blockToolError
	}
	m.blocks = append(m.blocks, block{kind: kind, text: text})
	m.rebuild()
}

// SetError adds an error block describing a turn-level failure.
func (m *Messages) SetError(msg string) {
	m.blocks = append(m.blocks, block{kind: blockError, text: msg})
	m.rebuild()
}

// AddInfo adds a muted, system-level note (e.g. a slash command's output) to
// the conversation.
func (m *Messages) AddInfo(text string) {
	m.blocks = append(m.blocks, block{kind: blockSystem, text: text})
	m.rebuild()
}

// SetHistory replaces the conversation with blocks reconstructed from a saved
// message history, used when resuming a session. The mapping mirrors how the
// live stream builds blocks: user text and tool results under the user role,
// assistant text and tool calls under the assistant role.
func (m *Messages) SetHistory(history []message.Message) {
	m.blocks = nil
	m.streaming = ""
	m.streamingActive = false
	m.reasoning = ""
	m.reasoningActive = false

	for _, msg := range history {
		for _, part := range msg.Parts {
			switch p := part.(type) {
			case message.TextPart:
				kind := blockAssistant
				if msg.Role == message.RoleUser {
					kind = blockUser
				}
				m.blocks = append(m.blocks, block{kind: kind, text: p.Text})

			case message.ToolUsePart:
				m.blocks = append(m.blocks, block{kind: blockToolCall, text: p.Name, detail: permissionDetail(p.Input)})

			case message.ToolResultPart:
				kind := blockToolResult
				if p.IsError {
					kind = blockToolError
				}
				m.blocks = append(m.blocks, block{kind: kind, text: toolResultText(p)})
			}
		}
	}

	m.rebuild()
}

// Init implements tea.Model; the view has no startup command.
func (m *Messages) Init() tea.Cmd { return nil }

// Update forwards scroll messages (keys, mouse wheel) to the viewport.
func (m *Messages) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	m.vp, cmd = m.vp.Update(msg)
	return m, cmd
}

// View renders the viewport.
func (m *Messages) View() string { return m.vp.View() }

// SetSize sizes the viewport, rebuilds the width-bound markdown renderer, and
// re-renders so wrapping tracks the new width.
func (m *Messages) SetSize(width, height int) {
	m.width = width
	m.height = height
	m.vp.Width = width
	m.vp.Height = height
	m.buildRenderer()
	m.rebuild()
}

// buildRenderer (re)creates the glamour renderer bound to the current width.
// It is left nil when the width is unknown or glamour fails to initialize, in
// which case assistant markdown degrades to raw text.
func (m *Messages) buildRenderer() {
	if m.width <= 0 {
		m.renderer = nil
		return
	}

	wrap := m.width - contentGutter
	if wrap < 1 {
		wrap = 1
	}

	// A fixed dark style is used instead of glamour.WithAutoStyle(): auto
	// resolves the style by querying the terminal's background color (OSC 11)
	// at render time, and because Bubble Tea already owns the tty in raw mode,
	// that query's response leaks into the input as stray text. A fixed style
	// never queries the terminal.
	r, err := glamour.NewTermRenderer(
		glamour.WithStandardStyle("dark"),
		glamour.WithWordWrap(wrap),
	)
	if err != nil {
		m.renderer = nil
		return
	}
	m.renderer = r
}

// rebuild renders every finalized block plus the live streaming block (if any)
// and pushes the joined content into the viewport, keeping the newest content
// in view.
func (m *Messages) rebuild() {
	parts := make([]string, 0, len(m.blocks)+2)
	for _, b := range m.blocks {
		parts = append(parts, m.renderBlock(b))
	}
	if m.reasoningActive {
		parts = append(parts, m.renderReasoning(m.reasoning))
	}
	if m.streamingActive {
		parts = append(parts, m.renderStreaming())
	}

	m.vp.SetContent(strings.Join(parts, blockSeparator))
	m.vp.GotoBottom()
}

// renderReasoning renders the model's thinking as a dim, italic block under a
// "Thinking" label, aligned to the shared gutter. It is used both for the live
// stream and for a finalized reasoning block.
func (m *Messages) renderReasoning(text string) string {
	theme := CurrentTheme()

	width := m.width - contentGutter
	if width < 1 {
		width = 1
	}

	dim := lipgloss.NewStyle().Foreground(theme.Muted()).Italic(true)
	label := dim.Bold(true).Render(labelThinking)
	body := dim.Width(width).Render(text)

	return lipgloss.NewStyle().MarginLeft(contentGutter).Render(label + "\n" + body)
}

// renderStreaming renders the in-progress assistant text as plain, gutter-
// aligned text. Markdown is intentionally NOT applied here: token deltas arrive
// rapidly and running glamour on every delta would be wasteful, so markdown is
// deferred to FinishAssistant.
func (m *Messages) renderStreaming() string {
	return m.gutteredBlock(CurrentTheme().Assistant(), m.streaming, false)
}

// renderBlock styles one finalized block according to its kind and the active
// theme. The user turn gets a colored left bar; assistant blocks are rendered
// as markdown; tool and error lines are gutter-aligned colored text.
func (m *Messages) renderBlock(b block) string {
	theme := CurrentTheme()

	switch b.kind {
	case blockUser:
		return m.renderUser(b.text)

	case blockAssistant:
		return m.renderMarkdown(b.text)

	case blockToolCall:
		head := lipgloss.NewStyle().Foreground(theme.Tool()).Bold(true).Render(labelToolCall + b.text)
		if b.detail != "" {
			head += "  " + lipgloss.NewStyle().Foreground(theme.Muted()).Render(b.detail)
		}
		width := m.width - contentGutter
		if width < 1 {
			width = 1
		}
		return lipgloss.NewStyle().MarginLeft(contentGutter).Width(width).Render(head)

	case blockToolResult:
		return m.gutteredBlock(theme.Muted(), indentLines(truncateToolResult(b.text)), false)

	case blockToolError:
		return m.gutteredBlock(theme.Error(), indentLines(labelToolError+truncateToolResult(b.text)), false)

	case blockError:
		return m.gutteredBlock(theme.Error(), labelErrorBlock+b.text, true)

	case blockSystem:
		return m.gutteredBlock(theme.Muted(), b.text, false)

	case blockReasoning:
		return m.renderReasoning(b.text)

	default:
		return b.text
	}
}

// renderUser renders a user turn as a block set off by a colored left bar,
// matching opencode's treatment. The bar sits at the far left and the text is
// inset so it lines up with the gutter used by every other block.
func (m *Messages) renderUser(text string) string {
	theme := CurrentTheme()

	width := m.width - 2 // left bar (1) + a column of right slack
	if width < 1 {
		width = 1
	}

	return lipgloss.NewStyle().
		BorderStyle(lipgloss.ThickBorder()).
		BorderLeft(true).
		BorderForeground(theme.User()).
		Foreground(theme.Assistant()).
		PaddingLeft(1).
		MarginTop(1).
		Width(width).
		Render(text)
}

// gutteredBlock renders text in color, wrapped to the content width and inset
// by the shared left gutter so non-user blocks align with the assistant's
// markdown margin. bold emphasizes headers such as the tool-call and
// turn-error lines.
func (m *Messages) gutteredBlock(color lipgloss.Color, text string, bold bool) string {
	width := m.width - contentGutter
	if width < 1 {
		width = 1
	}

	return lipgloss.NewStyle().
		Foreground(color).
		Bold(bold).
		Width(width).
		MarginLeft(contentGutter).
		Render(text)
}

// renderMarkdown renders assistant text as terminal markdown, falling back to
// the raw text if the renderer is unavailable or errors, so a malformed
// response can never crash the UI.
func (m *Messages) renderMarkdown(text string) string {
	if m.renderer == nil {
		return text
	}

	out, err := m.renderer.Render(text)
	if err != nil {
		return text
	}
	// glamour pads the block with a leading and trailing blank line; trim both
	// so assistant turns sit tight against their neighbors.
	return strings.Trim(out, "\n")
}

// truncateToolResult caps a tool result at a line and byte budget, appending a
// truncation marker when anything was dropped. The byte slice is made valid
// UTF-8 so a cut mid-rune never produces a replacement glyph.
func truncateToolResult(text string) string {
	truncated := false

	if len(text) > toolResultMaxBytes {
		text = strings.ToValidUTF8(text[:toolResultMaxBytes], "")
		truncated = true
	}

	lines := strings.Split(text, "\n")
	if len(lines) > toolResultMaxLines {
		lines = lines[:toolResultMaxLines]
		truncated = true
	}
	text = strings.Join(lines, "\n")

	if truncated {
		text += "\n" + truncationMarker
	}
	return text
}

// indentLines prefixes every line with a fixed indent so tool results read as a
// nested, secondary block under their tool call.
func indentLines(text string) string {
	lines := strings.Split(text, "\n")
	for i, line := range lines {
		lines[i] = toolResultIndent + line
	}
	return strings.Join(lines, "\n")
}

package tui

import (
	"strings"

	"github.com/charmbracelet/bubbles/viewport"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/glamour"
	"github.com/charmbracelet/lipgloss"
)

// Visible markers kept as text (not just color) so the conversation stays
// legible on terminals without color and so a screen reader still conveys the
// speaker/role. Color is layered on top via the active theme at render time.
const (
	labelUser        = "You:"
	labelToolCall    = "→ "
	labelToolError   = "error: "
	labelErrorBlock  = "error: "
	prefixAssistant  = "agens: "
	blockSeparator   = "\n\n"
	toolResultIndent = "  "
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
)

// block is a finalized conversation entry kept in its raw form so it can be
// re-rendered (and, for assistant markdown, re-wrapped) whenever the width
// changes.
type block struct {
	kind blockKind
	text string
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

// AddToolCall adds a block marking that a tool invocation has started.
func (m *Messages) AddToolCall(name string) {
	m.blocks = append(m.blocks, block{kind: blockToolCall, text: name})
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

	r, err := glamour.NewTermRenderer(
		glamour.WithAutoStyle(),
		glamour.WithWordWrap(m.width),
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
	parts := make([]string, 0, len(m.blocks)+1)
	for _, b := range m.blocks {
		parts = append(parts, m.renderBlock(b))
	}
	if m.streamingActive {
		parts = append(parts, m.renderStreaming())
	}

	m.vp.SetContent(strings.Join(parts, blockSeparator))
	m.vp.GotoBottom()
}

// renderStreaming renders the in-progress assistant text as plain, styled text.
// Markdown is intentionally NOT applied here: token deltas arrive rapidly and
// running glamour on every delta would be wasteful, so markdown is deferred to
// FinishAssistant.
func (m *Messages) renderStreaming() string {
	body := m.styled(CurrentTheme().Assistant(), m.streaming)
	return prefixAssistant + body
}

// renderBlock styles one finalized block according to its kind and the active
// theme. Assistant blocks are rendered as markdown; the rest are plain text
// with a role color.
func (m *Messages) renderBlock(b block) string {
	theme := CurrentTheme()

	switch b.kind {
	case blockUser:
		label := lipgloss.NewStyle().Foreground(theme.User()).Bold(true).Render(labelUser)
		return label + " " + m.styled(theme.User(), b.text)

	case blockAssistant:
		return m.renderMarkdown(b.text)

	case blockToolCall:
		return lipgloss.NewStyle().Foreground(theme.Tool()).Bold(true).Render(labelToolCall + b.text)

	case blockToolResult:
		return m.styled(theme.Muted(), indentLines(truncateToolResult(b.text)))

	case blockToolError:
		return m.styled(theme.Error(), indentLines(labelToolError+truncateToolResult(b.text)))

	case blockError:
		return lipgloss.NewStyle().Foreground(theme.Error()).Bold(true).Render(labelErrorBlock + b.text)

	default:
		return b.text
	}
}

// styled applies a foreground color and, when a width is known, wraps the text
// to that width so long non-markdown blocks do not overflow the viewport.
func (m *Messages) styled(color lipgloss.Color, text string) string {
	style := lipgloss.NewStyle().Foreground(color)
	if m.width > 0 {
		style = style.Width(m.width)
	}
	return style.Render(text)
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
	return strings.TrimRight(out, "\n")
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

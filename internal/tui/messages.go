package tui

import (
	_ "embed"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/charmbracelet/bubbles/viewport"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/glamour"
	"github.com/charmbracelet/lipgloss"

	"github.com/iperez/agens/internal/message"
)

// ayuStyleJSON is the glamour style config that renders assistant markdown —
// including code-block syntax highlighting — with the ayu dark palette, so code
// fences match the rest of the TUI's ayu theme instead of glamour's default
// dark chroma colors.
//
//go:embed ayu.json
var ayuStyleJSON []byte

// Visible markers layered on top of the theme colors. The user turn is set
// off by a colored left bar (like opencode) rather than a text label; tool and
// error lines keep a short glyph/word so they read even without color.
const (
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
	blockTool
	blockToolBatch
	blockSubagent
	blockError
	blockSystem
	blockReasoning
)

// block is a finalized conversation entry kept in its raw form so it can be
// re-rendered (and, for assistant markdown, re-wrapped) whenever the width
// changes. detail carries a secondary, muted string (the tool call's argument,
// e.g. the shell command) shown after the primary text.
//
// A blockTool pairs a tool call with its result: text is the tool name, detail
// the argument, result the (raw) output. toolID links the call to the result
// that completes it; done marks that the result has arrived; isError flags a
// failed result; dur is the execution time (zero when unknown, e.g. resumed
// from history).
type block struct {
	kind   blockKind
	text   string
	detail string

	toolID         string
	batchID        string
	batchTotal     int
	batchCompleted int
	batchFailed    int
	result         string
	isError        bool
	done           bool
	dur            time.Duration

	// sub holds the live state of a delegated subagent when kind is
	// blockSubagent; it is nil for every other block kind.
	sub *subagentState
}

// Messages is the scrollable conversation view. Finalized turns are kept as
// raw blocks and styled on every rebuild; the in-progress assistant response is
// accumulated separately and rendered live as markdown until FinishAssistant
// commits it. After every mutation the viewport content is rebuilt and, when
// already at the bottom, scrolled to follow the newest content.
type Messages struct {
	vp              viewport.Model
	blocks          []block
	streaming       string
	streamingActive bool
	reasoning       string
	reasoningActive bool
	// detailsExpanded controls whether collapsible blocks (tool output and
	// finished "Thinking") show their body; folded by default, toggled for all
	// of them at once via ToggleDetails (Ctrl+O).
	detailsExpanded bool
	// collapseThinking folds a finished reasoning block to its header; when false
	// (default) finished thinking is shown in full. truncateToolOutput caps an
	// expanded tool result to a line/byte budget; when false (default) the full
	// result is shown. Both come from the user's UI config.
	collapseThinking   bool
	truncateToolOutput bool
	width, height      int

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

// AddToolCall adds a collapsible tool block for a started invocation. id links
// it to the result that later completes it; detail is a short description of
// what the tool acts on (the shell command, a path), shown muted after the tool
// name and may be empty. The block starts pending (no result, no duration).
func (m *Messages) AddToolCall(id, name, detail string) {
	m.blocks = append(m.blocks, block{kind: blockTool, toolID: id, text: name, detail: detail})
	m.rebuild()
}

// AddToolBatch adds one aggregate batch header followed by individually
// collapsible child tool rows. Each child keeps the same result pairing and
// expansion behavior as a standalone tool call.
func (m *Messages) AddToolBatch(calls []message.ToolUsePart) {
	if len(calls) == 0 {
		return
	}
	if len(calls) == 1 {
		call := calls[0]
		m.AddToolCall(call.ID, call.Name, permissionDetail(call.Input))
		return
	}

	batchID := toolBatchID(len(m.blocks), calls)
	m.blocks = append(m.blocks, block{kind: blockToolBatch, batchID: batchID, batchTotal: len(calls)})
	for _, call := range calls {
		m.blocks = append(m.blocks, block{
			kind:       blockTool,
			toolID:     call.ID,
			batchID:    batchID,
			batchTotal: len(calls),
			text:       call.Name,
			detail:     permissionDetail(call.Input),
		})
	}
	m.rebuild()
}

// CompleteToolCall fills the pending tool block matching id with its result,
// error status, and execution duration. It is a no-op when no matching pending
// block exists (e.g. a duplicate or unknown result).
func (m *Messages) CompleteToolCall(id, result string, isError bool, dur time.Duration) {
	for i := range m.blocks {
		b := &m.blocks[i]
		if b.kind == blockTool && b.toolID == id && !b.done {
			b.result = result
			b.isError = isError
			b.dur = dur
			b.done = true
			m.rebuild()
			return
		}
	}
}

// CompleteLatestToolBatch marks the newest unfinished batch header terminal.
// The backend batch IDs are execution-oriented while the TUI batch IDs are
// derived from finalized tool calls, so completion is matched by recency.
func (m *Messages) CompleteLatestToolBatch(total, completed, failed int) {
	for i := len(m.blocks) - 1; i >= 0; i-- {
		b := &m.blocks[i]
		if b.kind != blockToolBatch || b.done {
			continue
		}
		if total > 0 {
			b.batchTotal = total
		}
		b.batchCompleted = completed
		b.batchFailed = failed
		b.isError = failed > 0
		b.done = true
		m.rebuild()
		return
	}
}

func toolUsesInMessage(msg message.Message) []message.ToolUsePart {
	calls := make([]message.ToolUsePart, 0)
	if msg.Role != message.RoleAssistant {
		return calls
	}
	for _, part := range msg.Parts {
		if call, ok := part.(message.ToolUsePart); ok {
			calls = append(calls, call)
		}
	}
	return calls
}

func toolBatchID(blockIndex int, calls []message.ToolUsePart) string {
	if len(calls) == 0 {
		return ""
	}
	return fmt.Sprintf("batch:%d:%s", blockIndex, calls[0].ID)
}

func (m *Messages) addToolUseBlocks(calls []message.ToolUsePart, toolIndex map[string]int) {
	if len(calls) == 0 {
		return
	}
	if len(calls) > 1 {
		batchID := toolBatchID(len(m.blocks), calls)
		m.blocks = append(m.blocks, block{kind: blockToolBatch, batchID: batchID, batchTotal: len(calls)})
		for _, call := range calls {
			toolIndex[call.ID] = len(m.blocks)
			m.blocks = append(m.blocks, block{
				kind:       blockTool,
				toolID:     call.ID,
				batchID:    batchID,
				batchTotal: len(calls),
				text:       call.Name,
				detail:     permissionDetail(call.Input),
			})
		}
		return
	}

	call := calls[0]
	toolIndex[call.ID] = len(m.blocks)
	m.blocks = append(m.blocks, block{kind: blockTool, toolID: call.ID, text: call.Name, detail: permissionDetail(call.Input)})
}

// ToggleDetails flips whether collapsible blocks (tool output and finished
// "Thinking") show their body, for all of them at once. It backs the Ctrl+O
// collapse/expand shortcut.
func (m *Messages) ToggleDetails() {
	m.detailsExpanded = !m.detailsExpanded
	m.rebuild()
}

// SetDisplayOptions configures how much of a finished reasoning block and an
// expanded tool result are shown, from the user's UI config, and re-renders.
func (m *Messages) SetDisplayOptions(collapseThinking, truncateToolOutput bool) {
	m.collapseThinking = collapseThinking
	m.truncateToolOutput = truncateToolOutput
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
// message history, used when resuming a session. User and assistant text map to
// their turn blocks; a tool call and its later result are paired by tool-use id
// into one collapsible tool block. Durations are unknown when resuming, so they
// are left zero (hidden).
func (m *Messages) SetHistory(history []message.Message) {
	m.blocks = nil
	m.streaming = ""
	m.streamingActive = false
	m.reasoning = ""
	m.reasoningActive = false

	toolIndex := map[string]int{}

	for _, msg := range history {
		toolUses := toolUsesInMessage(msg)
		batchInserted := false
		for _, part := range msg.Parts {
			switch p := part.(type) {
			case message.TextPart:
				kind := blockAssistant
				if msg.Role == message.RoleUser {
					kind = blockUser
				}
				m.blocks = append(m.blocks, block{kind: kind, text: p.Text})

			case message.ToolUsePart:
				if len(toolUses) > 1 {
					if !batchInserted {
						m.addToolUseBlocks(toolUses, toolIndex)
						batchInserted = true
					}
					continue
				}
				m.addToolUseBlocks([]message.ToolUsePart{p}, toolIndex)

			case message.ToolResultPart:
				if i, ok := toolIndex[p.ToolUseID]; ok {
					m.blocks[i].result = toolResultText(p)
					m.blocks[i].isError = p.IsError
					m.blocks[i].done = true
				}
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

	// A fixed embedded ayu style is used instead of glamour.WithAutoStyle():
	// auto resolves the style by querying the terminal's background color
	// (OSC 11) at render time, and because Bubble Tea already owns the tty in
	// raw mode, that query's response leaks into the input as stray text. A
	// fixed style never queries the terminal.
	r, err := glamour.NewTermRenderer(
		glamour.WithStylesFromJSONBytes(ayuStyleJSON),
		glamour.WithChromaFormatter(codeChromaFormatter()),
		glamour.WithWordWrap(wrap),
	)
	if err != nil {
		m.renderer = nil
		return
	}
	m.renderer = r
}

// codeChromaFormatter picks the chroma formatter for code-block highlighting.
// glamour's default ("terminal256") quantizes the ayu palette to the 256-color
// cube; when the terminal advertises truecolor via COLORTERM it uses
// "terminal16m" so the exact ayu hex colors render. COLORTERM is read from the
// environment only — never by querying the terminal — so it cannot reintroduce
// the OSC escape leak that WithAutoStyle caused.
func codeChromaFormatter() string {
	switch strings.ToLower(os.Getenv("COLORTERM")) {
	case "truecolor", "24bit":
		return "terminal16m"
	default:
		return "terminal256"
	}
}

// rebuild renders every finalized block plus the live streaming block (if any)
// and pushes the joined content into the viewport, keeping the newest content
// in view.
func (m *Messages) rebuild() {
	// Auto-follow only when already at the bottom, so the user can scroll up
	// to read while the agent keeps streaming without being snapped back.
	follow := m.vp.AtBottom()

	parts := make([]string, 0, len(m.blocks)+2)
	for _, b := range m.blocks {
		parts = append(parts, m.renderBlock(b))
	}
	if m.reasoningActive {
		parts = append(parts, m.renderReasoning(m.reasoning, true))
	}
	if m.streamingActive {
		parts = append(parts, m.renderStreaming())
	}

	m.vp.SetContent(strings.Join(parts, blockSeparator))
	if follow {
		m.vp.GotoBottom()
	}
}

// renderReasoning renders the model's thinking as a dim, italic block under a
// "Thinking" label, aligned to the shared gutter. The live stream (live=true)
// always shows the full text so the user can watch it think; a finished block
// is collapsible, showing only the labeled header until details are expanded.
func (m *Messages) renderReasoning(text string, live bool) string {
	theme := CurrentTheme()

	width := m.width - contentGutter
	if width < 1 {
		width = 1
	}

	dim := lipgloss.NewStyle().Foreground(theme.Muted()).Italic(true)

	// A finished block collapses to a caret + header (expandable with Ctrl+O)
	// only when the user opted into collapsing; otherwise, like the live stream,
	// it is always shown in full.
	collapsible := !live && m.collapseThinking

	caret := ""
	if collapsible {
		caret = "▸ "
		if m.detailsExpanded {
			caret = "▾ "
		}
	}
	label := dim.Bold(true).Render(caret + labelThinking)

	if collapsible && !m.detailsExpanded {
		return lipgloss.NewStyle().MarginLeft(contentGutter).Render(label)
	}

	body := dim.Width(width).Render(text)
	return lipgloss.NewStyle().MarginLeft(contentGutter).Render(label + "\n" + body)
}

// renderStreaming renders the in-progress assistant text as live markdown,
// using the same renderer as a finalized block so the layout does not shift
// when the response commits. It falls back to the raw text if the renderer is
// unavailable, matching renderMarkdown.
func (m *Messages) renderStreaming() string {
	return m.renderMarkdown(m.streaming)
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

	case blockTool:
		return m.renderTool(b)

	case blockToolBatch:
		return m.renderToolBatch(b)

	case blockSubagent:
		return m.renderSubagent(b.sub)

	case blockError:
		return m.gutteredBlock(theme.Error(), labelErrorBlock+b.text, true)

	case blockSystem:
		return m.gutteredBlock(theme.Muted(), b.text, false)

	case blockReasoning:
		return m.renderReasoning(b.text, false)

	default:
		return b.text
	}
}

// renderToolBatch renders the aggregate header for a same-turn group of tool
// calls. The header summarizes child completion state only; the child rows
// below remain the interactive units for inspecting individual tool details.
func (m *Messages) renderToolBatch(b block) string {
	theme := CurrentTheme()

	completed := 0
	failed := 0
	for _, child := range m.blocks {
		if child.kind != blockTool || child.batchID != b.batchID {
			continue
		}
		if child.done {
			completed++
		}
		if child.isError {
			failed++
		}
	}
	if b.batchCompleted > completed {
		completed = b.batchCompleted
	}
	if b.batchFailed > failed {
		failed = b.batchFailed
	}

	total := b.batchTotal
	if total == 0 {
		total = completed
	}

	state := fmt.Sprintf("Batch %d/%d running", completed, total)
	if b.done || (total > 0 && completed >= total) {
		if failed == 0 && completed >= total {
			state = fmt.Sprintf("Batch %d/%d succeeded", completed, total)
		} else {
			if failed == 0 {
				failed = 1
			}
			state = fmt.Sprintf("Batch %d/%d completed · %d failed", completed, total, failed)
		}
	}

	color := theme.Tool()
	if failed > 0 {
		color = theme.Error()
	}
	return m.gutteredBlock(color, "▦ "+state, true)
}

// renderTool renders a collapsible tool block: a header line with a disclosure
// caret, the tool name and argument, its duration and (on failure) a status
// marker; the result body follows only when tools are expanded and the result
// has arrived.
func (m *Messages) renderTool(b block) string {
	theme := CurrentTheme()

	caret := "▸ "
	if m.detailsExpanded && b.done {
		caret = "▾ "
	}

	head := lipgloss.NewStyle().Foreground(theme.Tool()).Bold(true).Render(caret + b.text)
	muted := lipgloss.NewStyle().Foreground(theme.Muted())
	if b.detail != "" {
		head += "  " + muted.Render(b.detail)
	}
	if b.done && b.dur > 0 {
		head += "  " + muted.Render("· "+formatDuration(b.dur))
	}
	if b.isError {
		head += "  " + lipgloss.NewStyle().Foreground(theme.Error()).Render("· failed")
	}

	width := m.width - contentGutter
	if width < 1 {
		width = 1
	}
	header := lipgloss.NewStyle().MarginLeft(contentGutter).Width(width).Render(head)

	if !m.detailsExpanded || !b.done || b.result == "" {
		return header
	}

	if !b.isError && isDiffResult(b.result) {
		return header + "\n" + m.renderDiffBody(b.result)
	}

	color := theme.Muted()
	if b.isError {
		color = theme.Error()
	}
	result := b.result
	if m.truncateToolOutput {
		result = truncateToolResult(result)
	}
	body := m.gutteredBlock(color, indentLines(result), false)
	return header + "\n" + body
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

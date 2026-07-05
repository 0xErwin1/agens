package tui

import (
	"strings"
	"time"

	"github.com/charmbracelet/lipgloss"

	"github.com/iperez/agens/internal/agentloop"
)

// subagentGlyph marks a delegated subagent panel, distinguishing it from a plain
// tool call (no glyph) and a tool batch (▦).
const subagentGlyph = "◆"

// subagentActivityPrefix marks a subagent's tool lines, echoing the child-line
// connector opencode uses in its task blocks.
const subagentActivityPrefix = "↳ "

// subagentToolResultMaxLines caps the reduced result shown under a subagent's
// tool call so a long output cannot dominate the panel; the full output still
// lives in the model's context.
const subagentToolResultMaxLines = 6

// subagentPromptMaxRunes caps the task prompt shown on a collapsed panel to a
// single readable line; the full prompt shows when the panel is expanded.
const subagentPromptMaxRunes = 100

// subagentListLinger is how long a finished subagent stays in the active-subagent
// tree before it drops off; its inline conversation panel remains as the record.
const subagentListLinger = 30 * time.Second

// subagentStatus tracks a delegated subagent's lifecycle for its inline panel.
type subagentStatus int

const (
	subagentRunning subagentStatus = iota
	subagentDone
	subagentFailed
)

// subagentTool is one tool call a subagent made, shown in its panel like the
// main conversation's tool blocks: the tool name, its argument, and — when the
// panel is expanded — a reduced view of its result.
type subagentTool struct {
	id      string
	name    string
	detail  string
	result  string
	isError bool
	done    bool
}

// subagentState is the live state of one delegated subagent, shown inline in the
// main conversation as a collapsible panel and in the active-subagent tree. A
// subagent nested under another carries its parent's id and a depth so its panel
// indents beneath the parent. prompt is the task it was given; tools are the
// calls it made; result is its final report.
type subagentState struct {
	id       string
	parentID string
	depth    int

	name   string
	model  string
	prompt string

	status     subagentStatus
	tokens     int
	dur        time.Duration
	tools      []subagentTool
	result     string
	finishedAt time.Time

	// convo is the subagent's own conversation, rendered exactly like the main
	// thread when the subagent is entered (focused). It is fed the subagent's
	// full stream via ApplyStream.
	convo *Messages
}

// RunningSubagents reports how many delegated subagents are still executing, so a
// surface can persistently show that work is happening off the main thread.
func (m *Messages) RunningSubagents() int {
	n := 0
	for i := range m.blocks {
		if b := &m.blocks[i]; b.kind == blockSubagent && b.sub != nil && b.sub.status == subagentRunning {
			n++
		}
	}
	return n
}

// findSubagent returns the live state of the subagent with the given id, or nil
// when no such panel exists.
func (m *Messages) findSubagent(id string) *subagentState {
	for i := range m.blocks {
		if b := &m.blocks[i]; b.kind == blockSubagent && b.sub != nil && b.sub.id == id {
			return b.sub
		}
	}
	return nil
}

// StartSubagent adds a collapsible panel for a delegated subagent. parentID links
// it to the subagent that spawned it (empty for a top-level delegation) so the
// panel indents beneath its parent. prompt is the task the subagent was given.
func (m *Messages) StartSubagent(id, parentID, name, model, prompt string) {
	depth := 0
	if parentID != "" {
		if parent := m.findSubagent(parentID); parent != nil {
			depth = parent.depth + 1
		}
	}

	// The subagent gets its own conversation view, sharing the parent's display
	// options and clock, so entering it renders identically to the main thread.
	convo := NewMessages()
	convo.SetDisplayOptions(m.collapseThinking, m.truncateToolOutput)
	convo.SetClock(m.now)

	m.blocks = append(m.blocks, block{kind: blockSubagent, sub: &subagentState{
		id:       id,
		parentID: parentID,
		depth:    depth,
		name:     name,
		model:    model,
		prompt:   prompt,
		status:   subagentRunning,
		convo:    convo,
	}})
	m.rebuild()
}

// ApplySubagentStream feeds one of a subagent's own stream events into its
// conversation view, so a focused subagent renders like the main thread. It is a
// no-op for an unknown id.
func (m *Messages) ApplySubagentStream(id string, ev agentloop.LoopEvent) {
	sub := m.findSubagent(id)
	if sub == nil || sub.convo == nil {
		return
	}
	sub.convo.ApplyStream(ev)
}

// AddSubagentTool records a tool call a subagent started, shown with its name and
// argument. It is a no-op for an unknown subagent id.
func (m *Messages) AddSubagentTool(subID, toolID, name, detail string) {
	sub := m.findSubagent(subID)
	if sub == nil {
		return
	}

	sub.tools = append(sub.tools, subagentTool{id: toolID, name: name, detail: detail})
	m.rebuild()
}

// CompleteSubagentTool fills the subagent tool call matching toolUseID with its
// result text and error status. It is a no-op when no matching pending call
// exists.
func (m *Messages) CompleteSubagentTool(subID, toolUseID, result string, isError bool) {
	sub := m.findSubagent(subID)
	if sub == nil {
		return
	}

	for i := range sub.tools {
		if sub.tools[i].id == toolUseID && !sub.tools[i].done {
			sub.tools[i].result = result
			sub.tools[i].isError = isError
			sub.tools[i].done = true
			m.rebuild()
			return
		}
	}
}

// UpdateSubagentProgress refreshes a running subagent's token count and elapsed
// time, driving the live figures in its panel header. A zero value leaves the
// corresponding figure unchanged. It is a no-op for an unknown id.
func (m *Messages) UpdateSubagentProgress(id string, tokens int, dur time.Duration) {
	sub := m.findSubagent(id)
	if sub == nil {
		return
	}

	if tokens > 0 {
		sub.tokens = tokens
	}
	if dur > 0 {
		sub.dur = dur
	}
	m.rebuild()
}

// SetSubagentResult records a subagent's final report so its panel can show it
// once finished. It is a no-op for an unknown id.
func (m *Messages) SetSubagentResult(id, result string) {
	sub := m.findSubagent(id)
	if sub == nil {
		return
	}
	sub.result = result
	m.rebuild()
}

// CompleteSubagent marks a subagent finished, recording its final status and
// elapsed time. A nonzero dur overrides the last live figure. It is a no-op for
// an unknown id.
func (m *Messages) CompleteSubagent(id string, isError bool, dur time.Duration) {
	sub := m.findSubagent(id)
	if sub == nil {
		return
	}

	sub.status = subagentDone
	if isError {
		sub.status = subagentFailed
	}
	if dur > 0 {
		sub.dur = dur
	}
	sub.finishedAt = m.clock()
	m.rebuild()
}

// clock returns the current time from the injected clock, defaulting to time.Now
// when none was set (tests may inject a controlled clock via SetClock).
func (m *Messages) clock() time.Time {
	if m.now == nil {
		return time.Now()
	}
	return m.now()
}

// subagentExpired reports whether a finished subagent has lingered past
// subagentListLinger and should drop off the active-subagent tree. A running
// subagent never expires.
func (m *Messages) subagentExpired(s *subagentState) bool {
	if s.status == subagentRunning || s.finishedAt.IsZero() {
		return false
	}
	return m.clock().Sub(s.finishedAt) >= subagentListLinger
}

// treeSubagents is orderedSubagents narrowed to what the active-subagent tree
// shows: every running subagent plus finished ones still within their linger
// window. Expired finished subagents drop off the list (their inline panels
// stay).
func (m *Messages) treeSubagents() []subagentRow {
	rows := m.orderedSubagents()
	out := make([]subagentRow, 0, len(rows))
	for _, r := range rows {
		if !m.subagentExpired(r.state) {
			out = append(out, r)
		}
	}
	return out
}

// metaLine renders the muted header detail: model, token count, and elapsed
// time, in that order, omitting any figure not yet known.
func (s *subagentState) metaLine() string {
	parts := make([]string, 0, 3)
	if s.model != "" {
		parts = append(parts, s.model)
	}
	if s.tokens > 0 {
		parts = append(parts, formatTokens(s.tokens)+" tok")
	}
	if s.dur > 0 {
		parts = append(parts, formatDuration(s.dur))
	}
	return strings.Join(parts, " · ")
}

// visibleTools returns the tool calls to show below the header. Expanded shows
// them all; a collapsed running panel shows only the latest so its current step
// stays visible while staying compact; a collapsed finished panel shows none.
func (s *subagentState) visibleTools(expanded bool) []subagentTool {
	if len(s.tools) == 0 {
		return nil
	}
	if expanded {
		return s.tools
	}
	if s.status == subagentRunning {
		return s.tools[len(s.tools)-1:]
	}
	return nil
}

// subagentToolLine formats a subagent tool call as "↳ name detail", with a
// failed marker when the call errored.
func subagentToolLine(t subagentTool) string {
	line := subagentActivityPrefix + t.name
	if t.detail != "" {
		line += " " + t.detail
	}
	if t.isError {
		line += " · failed"
	}
	return line
}

// panelBody builds the muted lines shown under the header: the task prompt, the
// subagent's tool calls (each with a reduced result when expanded), and its
// final report when finished and expanded.
func (s *subagentState) panelBody(expanded bool) []string {
	var body []string

	if s.prompt != "" {
		prompt := s.prompt
		if !expanded {
			prompt = truncateRunes(firstLine(prompt), subagentPromptMaxRunes)
		}
		body = append(body, "task: "+prompt)
	}

	for _, t := range s.visibleTools(expanded) {
		body = append(body, subagentToolLine(t))
		if expanded && t.done && t.result != "" {
			for _, ln := range reduceLines(t.result, subagentToolResultMaxLines) {
				body = append(body, "  "+ln)
			}
		}
	}

	if s.status != subagentRunning && s.result != "" && expanded {
		body = append(body, "")
		body = append(body, strings.Split(s.result, "\n")...)
	}

	return body
}

// renderSubagent renders a delegated subagent as a collapsible panel: a header
// with a disclosure caret, the subagent glyph and name in the accent color, and
// a muted metadata line; below it the task prompt, the tool calls it made, and
// its final report. Nested subagents indent beneath their parent.
func (m *Messages) renderSubagent(s *subagentState) string {
	theme := CurrentTheme()

	indent := contentGutter + s.depth*2
	width := m.width - indent
	if width < 1 {
		width = 1
	}

	caret := "▸ "
	if m.detailsExpanded {
		caret = "▾ "
	}

	muted := lipgloss.NewStyle().Foreground(theme.Muted())
	head := lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render(caret + subagentGlyph + " " + s.name)
	if meta := s.metaLine(); meta != "" {
		head += "  " + muted.Render(meta)
	}
	switch s.status {
	case subagentFailed:
		head += "  " + lipgloss.NewStyle().Foreground(theme.Error()).Render("· failed")
	case subagentDone:
		head += "  " + muted.Render("· done")
	}

	header := lipgloss.NewStyle().MarginLeft(indent).Width(width).Render(head)

	body := s.panelBody(m.detailsExpanded)
	if len(body) == 0 {
		return header
	}

	rendered := muted.MarginLeft(indent).Width(width).Render(strings.Join(body, "\n"))
	return header + "\n" + rendered
}

// firstLine returns s up to its first newline.
func firstLine(s string) string {
	if i := strings.IndexByte(s, '\n'); i >= 0 {
		return s[:i]
	}
	return s
}

// truncateRunes shortens s to at most max runes, appending an ellipsis when it
// was cut.
func truncateRunes(s string, max int) string {
	r := []rune(s)
	if len(r) <= max {
		return s
	}
	return string(r[:max]) + "…"
}

// reduceLines returns at most max lines of s, appending an ellipsis line when it
// was truncated, so a long tool result reads as a compact preview.
func reduceLines(s string, max int) []string {
	lines := strings.Split(strings.TrimRight(s, "\n"), "\n")
	if len(lines) > max {
		return append(lines[:max:max], "…")
	}
	return lines
}

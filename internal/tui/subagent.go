package tui

import (
	"strings"
	"time"

	"github.com/charmbracelet/lipgloss"
)

// subagentGlyph marks a delegated subagent panel, distinguishing it from a plain
// tool call (no glyph) and a tool batch (▦).
const subagentGlyph = "◆"

// subagentActivityMax caps how many recent activity lines a subagent panel keeps
// so a long-running subagent's step history cannot grow the panel without bound.
const subagentActivityMax = 6

// subagentStatus tracks a delegated subagent's lifecycle for its inline panel.
type subagentStatus int

const (
	subagentRunning subagentStatus = iota
	subagentDone
	subagentFailed
)

// subagentState is the live state of one delegated subagent, shown inline in the
// main conversation as a collapsible panel and (later) in the active-subagent
// tree. A subagent nested under another carries its parent's id and a depth so
// its panel indents beneath the parent.
type subagentState struct {
	id       string
	parentID string
	depth    int

	name   string
	model  string
	effort string

	status   subagentStatus
	tokens   int
	dur      time.Duration
	activity []string
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
// panel indents beneath its parent. The panel starts running with no activity.
func (m *Messages) StartSubagent(id, parentID, name, model, effort string) {
	depth := 0
	if parentID != "" {
		if parent := m.findSubagent(parentID); parent != nil {
			depth = parent.depth + 1
		}
	}

	m.blocks = append(m.blocks, block{kind: blockSubagent, sub: &subagentState{
		id:       id,
		parentID: parentID,
		depth:    depth,
		name:     name,
		model:    model,
		effort:   effort,
		status:   subagentRunning,
	}})
	m.rebuild()
}

// AddSubagentActivity records what a subagent is currently doing (a tool call, a
// step) as the newest line of its panel, keeping only the most recent lines. It
// is a no-op for an unknown id.
func (m *Messages) AddSubagentActivity(id, line string) {
	sub := m.findSubagent(id)
	if sub == nil {
		return
	}

	sub.activity = append(sub.activity, line)
	if len(sub.activity) > subagentActivityMax {
		sub.activity = sub.activity[len(sub.activity)-subagentActivityMax:]
	}
	m.rebuild()
}

// UpdateSubagentProgress refreshes a running subagent's token count and elapsed
// time, driving the live figures in its panel header. A zero value leaves the
// corresponding figure unchanged so callers may update either independently. It
// is a no-op for an unknown id.
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
	m.rebuild()
}

// metaLine renders the muted header detail: model, effort, token count, and
// elapsed time, in that order, omitting any figure not yet known.
func (s *subagentState) metaLine() string {
	parts := make([]string, 0, 4)
	if s.model != "" {
		parts = append(parts, s.model)
	}
	if s.effort != "" {
		parts = append(parts, s.effort)
	}
	if s.tokens > 0 {
		parts = append(parts, formatTokens(s.tokens)+" tok")
	}
	if s.dur > 0 {
		parts = append(parts, formatDuration(s.dur))
	}
	return strings.Join(parts, " · ")
}

// visibleActivity returns the activity lines to show below the header. When
// expanded, the full recent history is shown. When collapsed, a running panel
// shows only its latest step so its current activity stays visible from the main
// thread while staying compact; a finished panel collapses to just its header.
func (s *subagentState) visibleActivity(expanded bool) []string {
	if len(s.activity) == 0 {
		return nil
	}
	if expanded {
		return s.activity
	}
	if s.status == subagentRunning {
		return s.activity[len(s.activity)-1:]
	}
	return nil
}

// renderSubagent renders a delegated subagent as a collapsible panel: a header
// with a disclosure caret, the subagent glyph and name in the accent color, and
// a muted metadata line; below it the activity lines chosen by visibleActivity.
// Nested subagents indent beneath their parent.
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

	lines := s.visibleActivity(m.detailsExpanded)
	if len(lines) == 0 {
		return header
	}

	bodyText := toolResultIndent + strings.Join(lines, "\n"+toolResultIndent)
	body := lipgloss.NewStyle().Foreground(theme.Muted()).MarginLeft(indent).Width(width).Render(bodyText)
	return header + "\n" + body
}

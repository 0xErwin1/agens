package tui

import (
	"fmt"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// OpenSubagentTree implements CommandContext: it opens the active-subagent tree
// overlay positioned at the top of the list. The overlay reads live subagent
// state on every render, so it tracks running delegations as they progress.
func (m *Model) OpenSubagentTree() tea.Cmd {
	m.subagentTreeOpen = true
	m.subagentIdx = 0
	return nil
}

// handleSubagentTreeKey handles a keypress while the tree overlay is open:
// Up/Down and Tab/Shift+Tab cycle the selection (wrapping) and Esc closes. The
// selection count is recomputed each press so it stays valid as running
// subagents appear.
func (m *Model) handleSubagentTreeKey(msg tea.KeyMsg) {
	if msg.Type == tea.KeyEsc {
		m.subagentTreeOpen = false
		m.subagentIdx = 0
		return
	}

	n := len(m.messages.orderedSubagents())
	if n == 0 {
		return
	}
	if m.subagentIdx >= n {
		m.subagentIdx = n - 1
	}

	switch msg.Type {
	case tea.KeyUp, tea.KeyShiftTab:
		m.subagentIdx = (m.subagentIdx - 1 + n) % n

	case tea.KeyDown, tea.KeyTab:
		m.subagentIdx = (m.subagentIdx + 1) % n
	}
}

// maxSubagentRows caps how many subagent rows the tree overlay shows at once; a
// longer list scrolls a window that follows the selection.
const maxSubagentRows = 10

// subagentRow is one entry of the flattened subagent tree: the subagent state
// paired with its depth in the traversal (used for indentation, independent of
// the panel's own recorded depth so re-rooted orphans still align).
type subagentRow struct {
	state *subagentState
	depth int
}

// orderedSubagents flattens the subagent panels into pre-order tree traversal:
// each top-level subagent (no parent, or a parent no longer present) followed by
// its descendants, siblings kept in creation order. It backs the tree overlay.
func (m *Messages) orderedSubagents() []subagentRow {
	var all []*subagentState
	byID := map[string]*subagentState{}
	for i := range m.blocks {
		if b := &m.blocks[i]; b.kind == blockSubagent && b.sub != nil {
			all = append(all, b.sub)
			byID[b.sub.id] = b.sub
		}
	}

	children := map[string][]*subagentState{}
	var roots []*subagentState
	for _, s := range all {
		if _, ok := byID[s.parentID]; s.parentID == "" || !ok {
			roots = append(roots, s)
			continue
		}
		children[s.parentID] = append(children[s.parentID], s)
	}

	var out []subagentRow
	var walk func(s *subagentState, depth int)
	walk = func(s *subagentState, depth int) {
		out = append(out, subagentRow{state: s, depth: depth})
		for _, c := range children[s.id] {
			walk(c, depth+1)
		}
	}
	for _, r := range roots {
		walk(r, 0)
	}
	return out
}

// countRunningSubagents reports how many of the flattened rows are still running,
// for the overlay's "(n active)" header.
func countRunningSubagents(rows []subagentRow) int {
	n := 0
	for _, r := range rows {
		if r.state.status == subagentRunning {
			n++
		}
	}
	return n
}

// subagentStatusMark returns the glyph and color role for a subagent's status:
// a filled dot while running, a check when done, a cross when failed.
func subagentStatusMark(theme Theme, status subagentStatus) (string, lipgloss.Color) {
	switch status {
	case subagentDone:
		return "✓", theme.User()
	case subagentFailed:
		return "✗", theme.Error()
	default:
		return "•", theme.Accent()
	}
}

// renderSubagentTree draws the active-subagent overlay: a title with the running
// count, one row per subagent indented by its depth in the tree, and a hint. The
// selection is highlighted and the visible window follows it.
func renderSubagentTree(rows []subagentRow, selected, width int) string {
	theme := CurrentTheme()

	inner := width - 4
	if inner < 8 {
		inner = 8
	}

	oneLine := func(s string) string {
		return lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(s)
	}

	title := oneLine(lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render("Subagents") +
		lipgloss.NewStyle().Foreground(theme.Muted()).Render(fmt.Sprintf("  (%d active)", countRunningSubagents(rows))))
	hint := oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("↑/↓ navigate · esc close"))

	var body []string
	if len(rows) == 0 {
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("no subagents yet — try /subagents-demo"))}
	} else {
		body = subagentTreeRows(rows, selected, theme, inner)
	}

	content := append([]string{title}, body...)
	content = append(content, "", hint)

	box := lipgloss.NewStyle().
		Border(lipgloss.RoundedBorder()).
		BorderForeground(theme.Accent()).
		Padding(0, 1)
	if width > 4 {
		box = box.Width(width - 2)
	}

	return box.Render(strings.Join(content, "\n"))
}

// subagentTreeRows renders the visible window of subagent rows. Each row is
// indented by its tree depth and shows a status glyph, the name, and a muted
// metadata line (model, tokens, elapsed).
func subagentTreeRows(rows []subagentRow, selected int, theme Theme, inner int) []string {
	start := windowStart(selected, len(rows), maxSubagentRows)
	end := start + maxSubagentRows
	if end > len(rows) {
		end = len(rows)
	}

	out := make([]string, 0, end-start)
	for i := start; i < end; i++ {
		row := rows[i]
		s := row.state

		marker := "  "
		nameColor := theme.Assistant()
		if i == selected {
			marker = lipgloss.NewStyle().Foreground(theme.User()).Render("› ")
			nameColor = theme.User()
		}

		indent := strings.Repeat("  ", row.depth)
		glyph, glyphColor := subagentStatusMark(theme, s.status)
		mark := lipgloss.NewStyle().Foreground(glyphColor).Render(glyph)
		name := lipgloss.NewStyle().Foreground(nameColor).Render(s.name)

		line := marker + indent + mark + " " + name
		if meta := s.metaLine(); meta != "" {
			line += "  " + lipgloss.NewStyle().Foreground(theme.Muted()).Render(meta)
		}

		out = append(out, lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(line))
	}
	return out
}

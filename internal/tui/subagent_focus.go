package tui

import (
	"fmt"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// subagentSiblings returns the subagents sharing the focused one's parent, in
// tree order, together with the focused subagent's index among them. It backs
// the focus view's prev/next navigation and its "(i/N)" position. The result is
// empty for an unknown id.
func (m *Messages) subagentSiblings(id string) (siblings []*subagentState, index int) {
	target := m.findSubagent(id)
	if target == nil {
		return nil, 0
	}

	for _, r := range m.orderedSubagents() {
		if r.state.parentID != target.parentID {
			continue
		}
		if r.state.id == id {
			index = len(siblings)
		}
		siblings = append(siblings, r.state)
	}
	return siblings, index
}

// focusLine insets a single rendered line by the shared gutter and clamps it to
// width, matching the alignment of the conversation.
func focusLine(s string, width int) string {
	return lipgloss.NewStyle().Inline(true).MaxWidth(width).Render(strings.Repeat(" ", contentGutter) + s)
}

// focusedSubagent returns the subagent currently entered (focused), or nil when
// none is or it is no longer present.
func (m *Model) focusedSubagent() *subagentState {
	if m.subagentFocusID == "" {
		return nil
	}
	return m.messages.findSubagent(m.subagentFocusID)
}

// activeMessages is the conversation the scroll and expand keys act on: the
// entered subagent's own conversation when one is focused, otherwise the main
// thread.
func (m *Model) activeMessages() *Messages {
	if s := m.focusedSubagent(); s != nil && s.convo != nil {
		return s.convo
	}
	return m.messages
}

// subagentFocusView renders entering a subagent: a compact header with its glyph,
// name, metadata and task, then the subagent's own conversation — rendered
// exactly like the main thread and scrollable — and a breadcrumb footer. It fills
// the screen (the input is hidden while focused). Sizing the subagent's viewport
// here keeps it bound to the current terminal size.
func (m *Model) subagentFocusView(s *subagentState) string {
	theme := CurrentTheme()

	width := m.contentWidth
	if width < 1 {
		width = 1
	}

	muted := lipgloss.NewStyle().Foreground(theme.Muted())

	glyph, glyphColor := subagentStatusMark(theme, s.status)
	title := lipgloss.NewStyle().Foreground(glyphColor).Render(glyph) + " " +
		lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render(subagentGlyph+" "+s.name)
	if meta := s.metaLine(); meta != "" {
		title += "  " + muted.Render(meta)
	}
	switch s.status {
	case subagentFailed:
		title += "  " + lipgloss.NewStyle().Foreground(theme.Error()).Render("· failed")
	case subagentDone:
		title += "  " + muted.Render("· done")
	}

	header := []string{focusLine(title, width)}
	if s.prompt != "" {
		header = append(header, focusLine(muted.Render("task: "+truncateRunes(firstLine(s.prompt), width)), width))
	}
	header = append(header, "")

	siblings, index := m.messages.subagentSiblings(s.id)
	footer := focusLine(muted.Render(subagentFocusCrumbs(s, siblings, index)), width)

	convoHeight := m.height - topPad - len(header) - 1
	if convoHeight < 1 {
		convoHeight = 1
	}
	if s.convo != nil {
		s.convo.SetSize(width, convoHeight)
	}

	lines := make([]string, 0, len(header)+convoHeight+1)
	lines = append(lines, header...)
	if s.convo != nil {
		lines = append(lines, s.convo.View())
	}
	lines = append(lines, footer)
	return strings.Join(lines, "\n")
}

// subagentFocusCrumbs builds the breadcrumb hint: how to scroll and leave the
// view, the position among siblings and how to move between them.
func subagentFocusCrumbs(s *subagentState, siblings []*subagentState, index int) string {
	parts := []string{"↑/↓ scroll", "esc back"}
	if len(siblings) > 1 {
		parts = append(parts, fmt.Sprintf("(%d/%d)", index+1, len(siblings)), "←/→ prev/next")
	}
	return strings.Join(parts, " · ")
}

// handleSubagentFocusKey handles a keypress while a subagent is entered. The
// subagent's conversation behaves like the main thread — Ctrl+O expands its tool
// output and thinking, Up/Down (and PgUp/PgDn, the wheel) scroll it — with the
// focus-only additions Esc (step back to the list) and Left/Right (move between
// siblings).
func (m *Model) handleSubagentFocusKey(msg tea.KeyMsg) tea.Cmd {
	switch msg.Type {
	case tea.KeyEsc:
		// Step back out to the subagent list rather than all the way to the chat,
		// preserving the tree selection so a second Esc leaves the list.
		m.subagentFocusID = ""
		m.subagentTreeOpen = true

	case tea.KeyLeft, tea.KeyShiftTab:
		m.focusSibling(-1)

	case tea.KeyRight, tea.KeyTab:
		m.focusSibling(1)

	case tea.KeyCtrlO:
		m.activeMessages().ToggleDetails()

	case tea.KeyUp, tea.KeyDown:
		if s := m.focusedSubagent(); s != nil && s.convo != nil {
			_, cmd := s.convo.Update(msg)
			return cmd
		}
	}
	return nil
}

// focusSibling moves focus to the previous or next sibling of the focused
// subagent, wrapping around the ends. It is a no-op when there are no siblings.
func (m *Model) focusSibling(delta int) {
	siblings, idx := m.messages.subagentSiblings(m.subagentFocusID)
	if len(siblings) == 0 {
		return
	}
	next := (idx + delta + len(siblings)) % len(siblings)
	m.subagentFocusID = siblings[next].id
}

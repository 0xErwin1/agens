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
// width, matching the alignment of the conversation it replaces.
func focusLine(s string, width int) string {
	return lipgloss.NewStyle().Inline(true).MaxWidth(width).Render(strings.Repeat(" ", contentGutter) + s)
}

// renderSubagentFocus renders the full-height focus view for one subagent: a
// header (status glyph, name, metadata), its activity log as a live stream, and
// a breadcrumb footer pinned to the bottom row for navigating back and between
// siblings. It always returns exactly height lines so the surrounding input and
// footer stay put when focus replaces the conversation.
func renderSubagentFocus(s *subagentState, siblings []*subagentState, index, width, height int) string {
	theme := CurrentTheme()
	if width < 1 {
		width = 1
	}
	if height < 1 {
		height = 1
	}

	muted := lipgloss.NewStyle().Foreground(theme.Muted())

	glyph, glyphColor := subagentStatusMark(theme, s.status)
	title := lipgloss.NewStyle().Foreground(glyphColor).Render(glyph) + " " +
		lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render(subagentGlyph+" "+s.name)

	top := []string{focusLine(title, width)}
	if meta := s.metaLine(); meta != "" {
		top = append(top, focusLine(muted.Render(meta), width))
	}

	if s.prompt != "" {
		top = append(top, "", focusLine(muted.Bold(true).Render("Task"), width))
		for _, ln := range strings.Split(s.prompt, "\n") {
			top = append(top, focusLine(muted.Render(ln), width))
		}
	}

	top = append(top, "", focusLine(muted.Bold(true).Render("Activity"), width))
	if len(s.tools) == 0 {
		top = append(top, focusLine(muted.Italic(true).Render("(waiting…)"), width))
	} else {
		for _, t := range s.tools {
			top = append(top, focusLine(muted.Render(subagentToolLine(t)), width))
			if t.done && t.result != "" {
				for _, ln := range reduceLines(t.result, subagentToolResultMaxLines) {
					top = append(top, focusLine(muted.Render("  "+ln), width))
				}
			}
		}
	}

	if s.status != subagentRunning && s.result != "" {
		top = append(top, "", focusLine(muted.Bold(true).Render("Result"), width))
		for _, ln := range strings.Split(s.result, "\n") {
			top = append(top, focusLine(muted.Render(ln), width))
		}
	}

	footer := focusLine(muted.Render(subagentFocusCrumbs(s, siblings, index)), width)

	return assembleFocus(top, footer, height)
}

// subagentFocusCrumbs builds the breadcrumb hint: how to leave the view, the
// position among siblings and how to move between them, and how to jump to the
// parent when nested.
func subagentFocusCrumbs(s *subagentState, siblings []*subagentState, index int) string {
	parts := []string{"esc back"}
	if len(siblings) > 1 {
		parts = append(parts, fmt.Sprintf("(%d/%d)", index+1, len(siblings)), "←/→ prev/next")
	}
	if s.parentID != "" {
		parts = append(parts, "↑ parent")
	}
	return strings.Join(parts, " · ")
}

// assembleFocus stacks the top lines and the footer into exactly height rows,
// padding the gap between them and, if the top overflows, truncating it (real
// transcript scrolling arrives with real subagents).
func assembleFocus(top []string, footer string, height int) string {
	lines := make([]string, 0, height)
	lines = append(lines, top...)

	if len(lines) > height-1 {
		lines = lines[:height-1]
	}
	for len(lines) < height-1 {
		lines = append(lines, "")
	}
	lines = append(lines, footer)

	return strings.Join(lines, "\n")
}

// conversationView returns what fills the conversation area: the focused
// subagent's live view when one is focused (and still present), otherwise the
// normal message list. Reading the subagent's state here keeps the focus view
// updating in real time as the delegation progresses.
func (m *Model) conversationView() string {
	if m.subagentFocusID == "" {
		return m.messages.View()
	}

	s := m.messages.findSubagent(m.subagentFocusID)
	if s == nil {
		return m.messages.View()
	}

	siblings, index := m.messages.subagentSiblings(m.subagentFocusID)
	return renderSubagentFocus(s, siblings, index, m.messages.width, m.messages.height)
}

// handleSubagentFocusKey handles a keypress while a subagent is focused: Esc
// leaves the view, Left/Right (or Tab/Shift+Tab) move between siblings, and Up
// jumps to the parent when nested.
func (m *Model) handleSubagentFocusKey(msg tea.KeyMsg) {
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

	case tea.KeyUp:
		if s := m.messages.findSubagent(m.subagentFocusID); s != nil && s.parentID != "" {
			m.subagentFocusID = s.parentID
		}
	}
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

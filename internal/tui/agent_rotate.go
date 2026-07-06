package tui

import (
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// defaultAgentName is the reserved name of the rotation entry that runs the
// model's base persona — no agent definition override. It is always the first
// entry, so a fresh session starts on it and the user can always return to it.
const defaultAgentName = "default"

// agentEntry is one option in the primary-agent rotation: its name, the persona
// (system-prompt body) it applies — empty for the default — and a short
// description shown in the picker.
type agentEntry struct {
	name    string
	persona string
	desc    string
}

// agentEntries is the primary-agent rotation: the default entry followed by the
// primary-capable definitions, in definition order.
func (m *Model) agentEntries() []agentEntry {
	entries := []agentEntry{{name: defaultAgentName, desc: "the model's base persona"}}
	if m.agents != nil {
		for _, d := range m.agents.Primary() {
			entries = append(entries, agentEntry{name: d.Name, persona: d.Prompt, desc: d.Description})
		}
	}
	return entries
}

// activeAgentName returns the active agent's name, normalizing the empty initial
// value to the default entry.
func (m *Model) activeAgentName() string {
	if m.activeAgent == "" {
		return defaultAgentName
	}
	return m.activeAgent
}

// hasAgent reports whether name is one of the rotation entries (the default or a
// primary-capable definition).
func (m *Model) hasAgent(name string) bool {
	for _, e := range m.agentEntries() {
		if e.name == name {
			return true
		}
	}
	return false
}

// currentPersona returns the persona (system-prompt body) of the active agent,
// or "" for the default.
func (m *Model) currentPersona() string {
	for _, e := range m.agentEntries() {
		if e.name == m.activeAgentName() {
			return e.persona
		}
	}
	return ""
}

// OpenAgentPicker implements CommandContext: it opens the primary-agent picker
// positioned on the active agent, or notes that there is nothing to switch to
// when only the default entry exists.
func (m *Model) OpenAgentPicker() tea.Cmd {
	entries := m.agentEntries()
	if len(entries) <= 1 {
		m.messages.AddInfo("no agents to switch to")
		return nil
	}

	m.agentPickerOpen = true
	m.agentPickerIdx = indexOfAgent(entries, m.activeAgentName())
	return nil
}

// handleAgentPickerKey handles a keypress while the agent picker is open:
// Up/Down (and Tab/Shift+Tab) cycle the selection, Enter switches to the
// highlighted agent, and Esc closes without changing anything.
func (m *Model) handleAgentPickerKey(msg tea.KeyMsg) {
	entries := m.agentEntries()
	n := len(entries)

	switch msg.Type {
	case tea.KeyEsc:
		m.closeAgentPicker()

	case tea.KeyUp, tea.KeyShiftTab:
		m.agentPickerIdx = (m.agentPickerIdx - 1 + n) % n

	case tea.KeyDown, tea.KeyTab:
		m.agentPickerIdx = (m.agentPickerIdx + 1) % n

	case tea.KeyEnter:
		m.selectAgent(entries[m.agentPickerIdx])
	}
}

// cycleAgent rotates the active agent by delta (Tab/Shift+Tab from the main
// view, without opening the picker). It is a no-op when only the default entry
// exists.
func (m *Model) cycleAgent(delta int) {
	entries := m.agentEntries()
	if len(entries) <= 1 {
		return
	}

	idx := indexOfAgent(entries, m.activeAgentName())
	next := (idx + delta + len(entries)) % len(entries)
	m.selectAgent(entries[next])
}

// selectAgent switches the primary agent: it rebuilds the loop's system prompt
// with the agent's persona for the current model, updates the status segment,
// and queues a synthetic switch note prepended to the next message so the model
// notices the change mid-conversation. Selecting the already-active agent only
// closes the picker.
func (m *Model) selectAgent(entry agentEntry) {
	if entry.name == m.activeAgentName() {
		m.closeAgentPicker()
		return
	}

	m.activeAgent = entry.name
	m.rebuildSystemPrompt(m.modelName)
	m.status.SetAgent(statusAgentLabel(entry.name))
	m.pendingAgentNote = agentSwitchNote(entry)

	m.closeAgentPicker()
	m.messages.AddInfo("agent switched to " + entry.name)
}

// rebuildSystemPrompt rebuilds the loop's system prompt for model using the
// active agent's persona, so both an agent switch and a model switch keep the
// prompt (and its model-identity block) consistent. It prefers the agent-aware
// builder and falls back to the persona-less one.
func (m *Model) rebuildSystemPrompt(model string) {
	persona := m.currentPersona()

	if m.agentPrompt != nil {
		if sp, ok := m.agentPrompt(persona, model); ok {
			m.loop.SetSystemPrompt(sp)
		}
		return
	}
	if m.systemPrompt != nil {
		if sp, ok := m.systemPrompt(model); ok {
			m.loop.SetSystemPrompt(sp)
		}
	}
}

// closeAgentPicker hides the picker and resets its selection.
func (m *Model) closeAgentPicker() {
	m.agentPickerOpen = false
	m.agentPickerIdx = 0
}

// agentSwitchNote builds the synthetic system-reminder prepended to the next
// message so the model is told, in-band, that the active agent changed and to
// follow the new agent's instructions from here on. It is the agens equivalent
// of opencode's BUILD_SWITCH message.
func agentSwitchNote(entry agentEntry) string {
	var note string
	if entry.name == defaultAgentName {
		note = "The active agent is now the default assistant. Follow your base instructions from here on."
	} else {
		note = `The active agent is now "` + entry.name + `"`
		if entry.desc != "" {
			note += " (" + entry.desc + ")"
		}
		note += ". Follow this agent's instructions from here on, even where they differ from earlier in the conversation."
	}
	return "<system-reminder>" + note + "</system-reminder>\n\n"
}

// statusAgentLabel returns the status-bar label for an agent name: empty for the
// default (the baseline persona is not worth a segment), the name otherwise.
func statusAgentLabel(name string) string {
	if name == defaultAgentName || name == "" {
		return ""
	}
	return name
}

// indexOfAgent returns the index of the entry named current, or 0 (the default)
// when there is no match.
func indexOfAgent(entries []agentEntry, current string) int {
	for i, e := range entries {
		if e.name == current {
			return i
		}
	}
	return 0
}

// renderAgentPicker draws the primary-agent picker overlay: one row per rotation
// entry with its description, the active one marked and the selection
// highlighted.
func renderAgentPicker(entries []agentEntry, selected int, active string, width int) string {
	theme := CurrentTheme()

	inner := width - 4
	if inner < 8 {
		inner = 8
	}

	oneLine := func(s string) string {
		return lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(s)
	}

	title := oneLine(lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render("Active agent"))
	hint := oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("↑/↓ · tab · enter switch · esc cancel"))

	rows := make([]string, 0, len(entries))
	for i, e := range entries {
		marker := "  "
		nameColor := theme.Assistant()
		if i == selected {
			marker = lipgloss.NewStyle().Foreground(theme.User()).Render("› ")
			nameColor = theme.User()
		}

		label := lipgloss.NewStyle().Foreground(nameColor).Bold(true).Render(e.name)
		if e.desc != "" {
			label += lipgloss.NewStyle().Foreground(theme.Muted()).Render("  " + e.desc)
		}
		if e.name == active {
			label += lipgloss.NewStyle().Foreground(theme.Muted()).Render("  (active)")
		}
		rows = append(rows, oneLine(marker+label))
	}

	content := append([]string{title}, rows...)
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

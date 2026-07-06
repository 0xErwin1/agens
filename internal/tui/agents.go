package tui

import (
	"fmt"
	"path/filepath"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/iperez/agens/internal/agentdef"
	"github.com/iperez/agens/internal/provider"
)

// maxAgentRows and maxAgentModelRows cap how many rows each level of the agents
// menu shows at once; longer lists scroll a window that follows the selection.
const (
	maxAgentRows      = 8
	maxAgentModelRows = 8
)

// agentModelRow is one toggle in the per-agent model editor: a model id, whether
// it is currently allowed for the agent, and whether the provider still serves
// it (a checked-but-unserved model was configured for a model not in the current
// catalog).
type agentModelRow struct {
	id      string
	checked bool
	served  bool
}

// OpenAgentMenu implements CommandContext: it opens the two-level agents menu on
// its agent list. When the model catalog has not been fetched yet, it returns
// the command that loads it so the per-agent model editor has models to toggle.
func (m *Model) OpenAgentMenu() tea.Cmd {
	if m.agents == nil || len(m.agents.Subagents()) == 0 {
		m.messages.AddInfo("no agents available")
		return nil
	}

	m.agentMenuOpen = true
	m.agentMenuEditing = ""
	m.agentIdx = 0

	if len(m.modelItems) == 0 && m.lister != nil {
		return loadModelsCmd(m.lister)
	}
	return nil
}

// handleAgentMenuKey routes a keypress to the level currently shown: the agent
// list, or the per-agent model editor once an agent has been entered.
func (m *Model) handleAgentMenuKey(msg tea.KeyMsg) {
	if m.agentMenuEditing == "" {
		m.handleAgentListKey(msg)
		return
	}
	m.handleAgentModelsKey(msg)
}

// handleAgentListKey handles the agent-list level: Up/Down (and Tab/Shift+Tab)
// cycle the selection, Enter enters the selected agent's model editor, and Esc
// closes the menu.
func (m *Model) handleAgentListKey(msg tea.KeyMsg) {
	subs := m.agents.Subagents()
	n := len(subs)

	switch msg.Type {
	case tea.KeyEsc:
		m.closeAgentMenu()

	case tea.KeyUp, tea.KeyShiftTab:
		if n > 0 {
			m.agentIdx = (m.agentIdx - 1 + n) % n
		}

	case tea.KeyDown, tea.KeyTab:
		if n > 0 {
			m.agentIdx = (m.agentIdx + 1) % n
		}

	case tea.KeyEnter:
		if n > 0 {
			m.enterAgentModels(subs[m.agentIdx])
		}
	}
}

// enterAgentModels switches to the model editor for def, building its toggle
// rows from the served catalog and the agent's currently allowed models.
func (m *Model) enterAgentModels(def agentdef.Definition) {
	m.agentMenuEditing = def.Name
	m.agentModelRows = buildAgentModelRows(m.modelItems, def.Models)
	m.agentModelIdx = 0
}

// refreshAgentModelEditor rebuilds the open model editor's rows from the current
// catalog. It is called when the model catalog finishes loading, so an editor
// entered before the fetch returned (empty rows) fills in rather than staying
// stuck on "no models available".
func (m *Model) refreshAgentModelEditor() {
	if !m.agentMenuOpen || m.agentMenuEditing == "" || m.agents == nil {
		return
	}

	def, ok := m.agents.ByName(m.agentMenuEditing)
	if !ok {
		return
	}

	m.agentModelRows = buildAgentModelRows(m.modelItems, def.Models)
	if m.agentModelIdx >= len(m.agentModelRows) {
		m.agentModelIdx = 0
	}
}

// handleAgentModelsKey handles the per-agent model editor: Up/Down cycle, Space
// toggles the highlighted model, Enter saves the selection to the agent's
// definition file, and Esc returns to the agent list without saving.
func (m *Model) handleAgentModelsKey(msg tea.KeyMsg) {
	n := len(m.agentModelRows)

	switch {
	case msg.Type == tea.KeyEsc:
		m.agentMenuEditing = ""
		m.agentModelRows = nil
		m.agentModelIdx = 0

	case msg.Type == tea.KeyUp || msg.Type == tea.KeyShiftTab:
		if n > 0 {
			m.agentModelIdx = (m.agentModelIdx - 1 + n) % n
		}

	case msg.Type == tea.KeyDown || msg.Type == tea.KeyTab:
		if n > 0 {
			m.agentModelIdx = (m.agentModelIdx + 1) % n
		}

	case isSpaceKey(msg):
		if n > 0 {
			m.agentModelRows[m.agentModelIdx].checked = !m.agentModelRows[m.agentModelIdx].checked
		}

	case msg.Type == tea.KeyEnter:
		m.saveAgentModels()
	}
}

// saveAgentModels persists the toggled model selection to the edited agent's
// definition file (creating a project-level file for a built-in), reflects the
// change in the in-memory set, and closes the menu. The edit applies to new
// sessions, so the note points the user at /new. A write failure is surfaced as
// a note rather than losing the menu silently.
func (m *Model) saveAgentModels() {
	def, ok := m.agents.ByName(m.agentMenuEditing)
	if !ok {
		m.closeAgentMenu()
		return
	}

	models := selectedModels(m.agentModelRows)
	projectDir := filepath.Join(m.project, ".agens", "agents")

	path, err := agentdef.SaveModels(projectDir, def, models)
	if err != nil {
		m.messages.AddInfo("could not save agent models: " + humanizeError(err.Error()))
		m.closeAgentMenu()
		return
	}

	def.Models = models
	def.Source = path
	m.agents.Upsert(def)

	// Update the live catalog so the running loop's task tool honors the new set
	// on the next turn, not only in a fresh session.
	applied := m.subagents != nil && m.subagents.SetModels(def.Name, models)

	m.closeAgentMenu()
	if applied {
		m.messages.AddInfo(fmt.Sprintf("saved %s models — in effect now", def.Name))
	} else {
		m.messages.AddInfo(fmt.Sprintf("saved %s models — applies to new sessions (/new)", def.Name))
	}
}

// closeAgentMenu hides the menu and clears both levels' state.
func (m *Model) closeAgentMenu() {
	m.agentMenuOpen = false
	m.agentMenuEditing = ""
	m.agentModelRows = nil
	m.agentIdx = 0
	m.agentModelIdx = 0
}

// buildAgentModelRows builds the toggle rows for the model editor: one per served
// model (checked when it is in the agent's allowed set), followed by any allowed
// model the provider no longer serves so a stale-but-configured model is still
// visible and editable.
func buildAgentModelRows(served []provider.ModelInfo, allowed []string) []agentModelRow {
	allowedSet := make(map[string]bool, len(allowed))
	for _, a := range allowed {
		allowedSet[a] = true
	}

	seen := make(map[string]bool, len(served))
	rows := make([]agentModelRow, 0, len(served)+len(allowed))
	for _, info := range served {
		rows = append(rows, agentModelRow{id: info.ID, checked: allowedSet[info.ID], served: true})
		seen[info.ID] = true
	}
	for _, a := range allowed {
		if !seen[a] {
			rows = append(rows, agentModelRow{id: a, checked: true, served: false})
			seen[a] = true
		}
	}
	return rows
}

// selectedModels returns the ids of the checked rows, in row order.
func selectedModels(rows []agentModelRow) []string {
	out := make([]string, 0, len(rows))
	for _, r := range rows {
		if r.checked {
			out = append(out, r.id)
		}
	}
	return out
}

// isSpaceKey reports whether msg is the space bar, which some terminals deliver
// as KeySpace and others as a single space rune.
func isSpaceKey(msg tea.KeyMsg) bool {
	if msg.Type == tea.KeySpace {
		return true
	}
	return msg.Type == tea.KeyRunes && len(msg.Runes) == 1 && msg.Runes[0] == ' '
}

// agentMenuView renders whichever level of the menu is active inside a bordered
// box sized to width.
func (m *Model) agentMenuView() string {
	if m.agentMenuEditing == "" {
		return m.agentListView()
	}
	return m.agentModelsView()
}

// agentListView renders the agent-list level: one row per subagent-capable
// definition with its description and a summary of its allowed models.
func (m *Model) agentListView() string {
	theme := CurrentTheme()
	inner := agentMenuInner(m.contentWidth)

	oneLine := func(s string) string {
		return lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(s)
	}

	title := oneLine(lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render("Agents"))
	hint := oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("↑/↓ · enter edit models · esc close"))

	subs := m.agents.Subagents()
	start := windowStart(m.agentIdx, len(subs), maxAgentRows)
	end := start + maxAgentRows
	if end > len(subs) {
		end = len(subs)
	}

	rows := make([]string, 0, end-start)
	for i := start; i < end; i++ {
		def := subs[i]

		marker := "  "
		nameColor := theme.Assistant()
		if i == m.agentIdx {
			marker = lipgloss.NewStyle().Foreground(theme.User()).Render("› ")
			nameColor = theme.User()
		}

		name := lipgloss.NewStyle().Foreground(nameColor).Bold(true).Render(def.Name)
		meta := lipgloss.NewStyle().Foreground(theme.Muted()).Render("  " + agentModelSummary(def))
		rows = append(rows, oneLine(marker+name+meta))
	}

	content := append([]string{title}, rows...)
	content = append(content, "", hint)
	return agentMenuBox(theme, m.contentWidth).Render(strings.Join(content, "\n"))
}

// agentModelsView renders the per-agent model editor: a checkbox per model with
// the highlighted row marked, and a footer explaining how to toggle and save.
func (m *Model) agentModelsView() string {
	theme := CurrentTheme()
	inner := agentMenuInner(m.contentWidth)

	oneLine := func(s string) string {
		return lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(s)
	}

	title := oneLine(lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render(m.agentMenuEditing+" · available models") +
		lipgloss.NewStyle().Foreground(theme.Muted()).Render("  (none checked = any model)"))
	hint := oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("↑/↓ · space toggle · enter save · esc back"))

	var body []string
	switch {
	case len(m.agentModelRows) == 0 && m.modelLoading:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("loading models…"))}
	case len(m.agentModelRows) == 0:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("no models available"))}
	default:
		body = m.agentModelRowsView(theme, inner)
	}

	content := append([]string{title}, body...)
	content = append(content, "", hint)
	return agentMenuBox(theme, m.contentWidth).Render(strings.Join(content, "\n"))
}

// agentModelRowsView renders the visible window of checkbox rows, marking the
// highlighted row and tagging any model the provider no longer serves.
func (m *Model) agentModelRowsView(theme Theme, inner int) []string {
	start := windowStart(m.agentModelIdx, len(m.agentModelRows), maxAgentModelRows)
	end := start + maxAgentModelRows
	if end > len(m.agentModelRows) {
		end = len(m.agentModelRows)
	}

	rows := make([]string, 0, end-start)
	for i := start; i < end; i++ {
		row := m.agentModelRows[i]

		marker := "  "
		idColor := theme.Assistant()
		if i == m.agentModelIdx {
			marker = lipgloss.NewStyle().Foreground(theme.User()).Render("› ")
			idColor = theme.User()
		}

		box := "[ ] "
		if row.checked {
			box = "[x] "
		}

		label := lipgloss.NewStyle().Foreground(idColor).Render(row.id)
		if !row.served {
			label += lipgloss.NewStyle().Foreground(theme.Muted()).Render("  (not served)")
		}

		rows = append(rows, lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(marker+box+label))
	}
	return rows
}

// agentModelSummary describes an agent's allowed-models set for the list row:
// "any model" when unrestricted, otherwise the comma-joined ids.
func agentModelSummary(def agentdef.Definition) string {
	if len(def.Models) == 0 {
		return "any model"
	}
	return strings.Join(def.Models, ", ")
}

// agentMenuInner is the usable inner width of the menu box (border plus
// horizontal padding removed), floored so it never collapses.
func agentMenuInner(width int) int {
	inner := width - 4
	if inner < 8 {
		inner = 8
	}
	return inner
}

// agentMenuBox is the shared bordered container for both menu levels.
func agentMenuBox(theme Theme, width int) lipgloss.Style {
	box := lipgloss.NewStyle().
		Border(lipgloss.RoundedBorder()).
		BorderForeground(theme.Accent()).
		Padding(0, 1)
	if width > 4 {
		box = box.Width(width - 2)
	}
	return box
}

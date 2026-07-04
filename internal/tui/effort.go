package tui

import (
	"strings"

	"github.com/charmbracelet/lipgloss"
)

// effortOptions is the reasoning-effort choices the selector offers, in
// ascending order. The empty default (model's own effort) is not listed;
// choosing one always sets an explicit value.
var effortOptions = []string{"low", "medium", "high"}

// indexOfEffort returns the index of current in effortOptions, or 1 (medium)
// when it is unset or unknown, so the selector opens on a sensible default.
func indexOfEffort(current string) int {
	for i, e := range effortOptions {
		if e == current {
			return i
		}
	}
	return 1
}

// renderEffortSelector draws the effort selector overlay: one row per option
// with the current one marked and the selection highlighted.
func renderEffortSelector(selected int, current string, width int) string {
	theme := CurrentTheme()

	inner := width - 4
	if inner < 8 {
		inner = 8
	}

	oneLine := func(s string) string {
		return lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(s)
	}

	title := oneLine(lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render("Reasoning effort"))

	rows := make([]string, 0, len(effortOptions))
	for i, opt := range effortOptions {
		marker := "  "
		color := theme.Assistant()
		if i == selected {
			marker = lipgloss.NewStyle().Foreground(theme.User()).Render("› ")
			color = theme.User()
		}

		label := lipgloss.NewStyle().Foreground(color).Render(opt)
		if opt == current {
			label += lipgloss.NewStyle().Foreground(theme.Muted()).Render("  (current)")
		}
		rows = append(rows, oneLine(marker+label))
	}

	hint := oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("↑/↓ · tab · enter choose · esc cancel"))

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

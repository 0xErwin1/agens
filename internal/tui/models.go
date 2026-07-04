package tui

import (
	"context"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/iperez/agens/internal/provider"
)

// maxModelRows caps how many model rows the selector shows at once; a longer
// list scrolls a window that follows the selection.
const maxModelRows = 8

// modelsTimeout bounds the backend /models fetch so a hung request cannot
// leave the selector spinning forever.
const modelsTimeout = 15 * time.Second

// ModelLister lists the models the active provider can serve. provider.Provider
// satisfies it directly, so the CLI passes the built provider as the lister.
type ModelLister interface {
	Models(ctx context.Context) ([]provider.ModelInfo, error)
}

// modelsLoadedMsg delivers the result of an asynchronous model fetch into the
// Update loop.
type modelsLoadedMsg struct {
	models []provider.ModelInfo
	err    error
}

// loadModelsCmd fetches the model catalog off the UI goroutine (Bubble Tea runs
// the command concurrently) and reports it as a modelsLoadedMsg.
func loadModelsCmd(lister ModelLister) tea.Cmd {
	return func() tea.Msg {
		ctx, cancel := context.WithTimeout(context.Background(), modelsTimeout)
		defer cancel()

		models, err := lister.Models(ctx)
		return modelsLoadedMsg{models: models, err: err}
	}
}

// renderModelSelector draws the model selector overlay: a loading line while
// the fetch is in flight, the error otherwise, or the model list with the
// current model marked and the selection highlighted. The visible rows follow
// the selection so a long list stays navigable.
func renderModelSelector(items []provider.ModelInfo, selected int, loading bool, loadErr error, current string, width int) string {
	theme := CurrentTheme()

	inner := width - 4
	if inner < 8 {
		inner = 8
	}

	oneLine := func(s string) string {
		return lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(s)
	}

	title := oneLine(lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render("Choose a model"))
	hint := oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("↑/↓ · tab · enter choose · esc cancel"))

	var body []string
	switch {
	case loading:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("loading models…"))}
	case loadErr != nil:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Error()).Render("error: " + loadErr.Error()))}
	case len(items) == 0:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("no models available"))}
	default:
		body = modelRows(items, selected, current, theme, inner)
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

// modelRows renders the visible window of model rows, marking the selected and
// current entries.
func modelRows(items []provider.ModelInfo, selected int, current string, theme Theme, inner int) []string {
	start := windowStart(selected, len(items), maxModelRows)
	end := start + maxModelRows
	if end > len(items) {
		end = len(items)
	}

	rows := make([]string, 0, end-start)
	for i := start; i < end; i++ {
		info := items[i]

		marker := "  "
		nameColor := theme.Assistant()
		if i == selected {
			marker = lipgloss.NewStyle().Foreground(theme.User()).Render("› ")
			nameColor = theme.User()
		}

		label := lipgloss.NewStyle().Foreground(nameColor).Render(info.ID)
		if info.ID == current {
			label += lipgloss.NewStyle().Foreground(theme.Muted()).Render("  (current)")
		}

		rows = append(rows, lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(marker+label))
	}
	return rows
}

// windowStart returns the first index of a size-n window over a list of length
// total that keeps selected visible.
func windowStart(selected, total, size int) int {
	if total <= size || selected < size {
		return 0
	}
	start := selected - size + 1
	if start+size > total {
		start = total - size
	}
	return start
}

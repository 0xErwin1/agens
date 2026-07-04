package tui

import (
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/session"
)

// maxSessionRows caps how many session rows the picker shows at once; a longer
// list scrolls a window that follows the selection.
const maxSessionRows = 8

// sessionTimeFormat is how a session's last-updated time is shown in the picker.
const sessionTimeFormat = "Jan 2 15:04"

// SessionStore lists, loads, and saves conversations. *session.Store satisfies
// it, so the CLI passes the file-backed store; nil disables session history.
type SessionStore interface {
	Save(s session.Session) error
	List() ([]session.Session, error)
	Load(id string) (session.Session, error)
}

// sessionsLoadedMsg delivers the result of an asynchronous session list into
// the Update loop.
type sessionsLoadedMsg struct {
	sessions []session.Session
	err      error
}

// loadSessionsCmd lists the saved sessions off the UI goroutine.
func loadSessionsCmd(store SessionStore) tea.Cmd {
	return func() tea.Msg {
		sessions, err := store.List()
		return sessionsLoadedMsg{sessions: sessions, err: err}
	}
}

// sessionTitle derives a short title for a conversation from its first user
// message, falling back to a placeholder for an empty history.
func sessionTitle(history []message.Message) string {
	for _, msg := range history {
		if msg.Role != message.RoleUser {
			continue
		}
		for _, part := range msg.Parts {
			if text, ok := part.(message.TextPart); ok {
				return truncateTitle(strings.TrimSpace(text.Text))
			}
		}
	}
	return "untitled"
}

// truncateTitle collapses a title to a single line capped at 48 columns.
func truncateTitle(s string) string {
	if i := strings.IndexByte(s, '\n'); i >= 0 {
		s = s[:i]
	}
	if s == "" {
		return "untitled"
	}
	if len(s) > 48 {
		return s[:47] + "…"
	}
	return s
}

// renderSessionSelector draws the session picker overlay: a loading or error
// line, an empty note, or the sessions with the selection highlighted and the
// visible window following it.
func renderSessionSelector(sessions []session.Session, selected int, loading bool, loadErr error, width int) string {
	theme := CurrentTheme()

	inner := width - 4
	if inner < 8 {
		inner = 8
	}

	oneLine := func(s string) string {
		return lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(s)
	}

	title := oneLine(lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render("Resume a conversation"))
	hint := oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("↑/↓ · tab · enter resume · esc cancel"))

	var body []string
	switch {
	case loading:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("loading sessions…"))}
	case loadErr != nil:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Error()).Render("error: " + humanizeError(loadErr.Error())))}
	case len(sessions) == 0:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("no saved conversations"))}
	default:
		body = sessionRows(sessions, selected, theme, inner)
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

// sessionRows renders the visible window of session rows.
func sessionRows(sessions []session.Session, selected int, theme Theme, inner int) []string {
	start := windowStart(selected, len(sessions), maxSessionRows)
	end := start + maxSessionRows
	if end > len(sessions) {
		end = len(sessions)
	}

	rows := make([]string, 0, end-start)
	for i := start; i < end; i++ {
		sess := sessions[i]

		marker := "  "
		titleColor := theme.Assistant()
		if i == selected {
			marker = lipgloss.NewStyle().Foreground(theme.User()).Render("› ")
			titleColor = theme.User()
		}

		title := lipgloss.NewStyle().Foreground(titleColor).Render(sess.Title)
		when := lipgloss.NewStyle().Foreground(theme.Muted()).Render("  " + sess.Updated.Format(sessionTimeFormat))

		rows = append(rows, lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(marker+title+when))
	}
	return rows
}

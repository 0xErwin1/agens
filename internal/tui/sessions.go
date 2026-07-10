package tui

import (
	"path/filepath"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/session"
)

// maxSessionRows caps how many session rows the picker shows at once; a longer
// list scrolls a window that follows the selection.
const maxSessionRows = 8

// sessionTimeFormat is how a session's last-updated time is shown in the picker.
const sessionTimeFormat = "Jan 2 15:04"

// SessionStore lists, loads, and saves conversations. *sessiondb.Store
// satisfies it; nil disables session history.
type SessionStore interface {
	Save(s session.Session) error
	ListMeta() ([]session.Session, error)
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
		sessions, err := store.ListMeta()
		return sessionsLoadedMsg{sessions: sessions, err: err}
	}
}

// sessionResumeMsg delivers the result of loading a single session by id (the
// startup --resume path) into the Update loop.
type sessionResumeMsg struct {
	sess session.Session
	err  error
}

// resumeSessionCmd loads one session by id off the UI goroutine.
func resumeSessionCmd(store SessionStore, id string) tea.Cmd {
	return func() tea.Msg {
		sess, err := store.Load(id)
		return sessionResumeMsg{sess: sess, err: err}
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
func renderSessionSelector(sessions []session.Session, selected int, loading bool, loadErr error, showAll bool, width int) string {
	theme := CurrentTheme()

	inner := width - 4
	if inner < 8 {
		inner = 8
	}

	oneLine := func(s string) string {
		return lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(s)
	}

	scope := "this project"
	if showAll {
		scope = "all projects"
	}
	title := oneLine(lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render("Resume a conversation") +
		lipgloss.NewStyle().Foreground(theme.Muted()).Render("  ("+scope+")"))
	hint := oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("↑/↓ · enter resume · ctrl+a all/this · esc cancel"))

	var body []string
	switch {
	case loading:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("loading sessions…"))}
	case loadErr != nil:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Error()).Render("error: " + humanizeError(loadErr.Error())))}
	case len(sessions) == 0:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("no saved conversations"))}
	default:
		body = sessionRows(sessions, selected, showAll, theme, inner)
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

// sessionRows renders the visible window of session rows. When showAll is set
// each row is tagged with its project's base name so cross-project results are
// distinguishable.
func sessionRows(sessions []session.Session, selected int, showAll bool, theme Theme, inner int) []string {
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

		meta := "  " + sess.Updated.Format(sessionTimeFormat)
		if showAll {
			meta = "  " + sessionProjectLabel(sess.Project) + meta
		}
		when := lipgloss.NewStyle().Foreground(theme.Muted()).Render(meta)

		rows = append(rows, lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(marker+title+when))
	}
	return rows
}

// sessionProjectLabel is the short project tag shown in the all-projects view:
// the base name of the project root, or a placeholder for sessions saved
// without a project.
func sessionProjectLabel(project string) string {
	if project == "" {
		return "[no project]"
	}
	return "[" + filepath.Base(project) + "]"
}

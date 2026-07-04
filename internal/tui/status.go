package tui

import (
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// statusReady is the initial state shown before any turn runs.
const statusReady = "ready"

// statusSeparator joins the status segments (app name, model, state).
const statusSeparator = " · "

// statusHints is the right-aligned keybind legend, mirroring opencode's footer.
const statusHints = "enter send · ctrl+c quit"

// Status is the single-line status bar rendering the app name, active model,
// and current state, e.g. "agens · gpt-5.5 · thinking…". Each segment is
// colored through the active theme: the app name in the accent, the model
// muted, and the state colored by whether it reports an error.
type Status struct {
	model    string
	state    string
	effort   string
	spinner  string
	duration string
	tokens   string
	width    int
}

// NewStatus constructs a Status for the given model, starting in the ready
// state.
func NewStatus(model string) *Status {
	return &Status{model: model, state: statusReady}
}

var _ Component = (*Status)(nil)

// SetState replaces the state segment (e.g. "thinking…", "running read",
// "error: <msg>").
func (s *Status) SetState(state string) { s.state = state }

// SetModel replaces the model segment, used when the model is switched live.
func (s *Status) SetModel(model string) { s.model = model }

// SetEffort sets the reasoning-effort segment; empty hides it.
func (s *Status) SetEffort(effort string) { s.effort = effort }

// SetSpinner sets the animated spinner frame shown before the state segment
// while a turn is in flight. An empty frame hides it.
func (s *Status) SetSpinner(frame string) { s.spinner = frame }

// SetDuration sets the last turn's elapsed-time segment shown after the state;
// empty hides it. It is cleared when a new turn starts and set when one ends.
func (s *Status) SetDuration(d string) { s.duration = d }

// SetTokens sets the token/context-usage segment shown before the right-aligned
// hints; empty hides it.
func (s *Status) SetTokens(t string) { s.tokens = t }

// Init implements tea.Model; the status bar has no startup command.
func (s *Status) Init() tea.Cmd { return nil }

// Update implements tea.Model. The status bar is driven entirely through
// SetState, so it ignores incoming messages.
func (s *Status) Update(tea.Msg) (tea.Model, tea.Cmd) { return s, nil }

// View renders the themed footer: the app name, model, and state on the left,
// and the keybind hints on the right, justified across the stored width and
// inset by a one-column gutter to match the conversation.
func (s *Status) View() string {
	theme := CurrentTheme()

	name := lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render("agens")
	model := lipgloss.NewStyle().Foreground(theme.Muted()).Render(s.model)
	state := lipgloss.NewStyle().Foreground(s.stateColor(theme)).Render(s.state)
	if s.spinner != "" {
		state = lipgloss.NewStyle().Foreground(theme.Accent()).Render(s.spinner) + " " + state
	}

	left := " " + name + statusSeparator + model
	if s.effort != "" {
		left += statusSeparator + lipgloss.NewStyle().Foreground(theme.Muted()).Render(s.effort)
	}
	left += statusSeparator + state
	if s.duration != "" {
		left += statusSeparator + lipgloss.NewStyle().Foreground(theme.Muted()).Render(s.duration)
	}

	right := ""
	if s.tokens != "" {
		right += lipgloss.NewStyle().Foreground(theme.Muted()).Render(s.tokens) + statusSeparator
	}
	right += lipgloss.NewStyle().Foreground(theme.Muted()).Render(statusHints) + " "

	line := s.justify(left, right)

	style := lipgloss.NewStyle().Inline(true)
	if s.width > 0 {
		style = style.MaxWidth(s.width)
	}

	return style.Render(line)
}

// justify places left and right on one row separated by padding that fills the
// stored width. When there is not enough room for both, the right hints are
// dropped so the model/state on the left is never lost.
func (s *Status) justify(left, right string) string {
	if s.width <= 0 {
		return left
	}

	gap := s.width - lipgloss.Width(left) - lipgloss.Width(right)
	if gap < 1 {
		return left
	}

	return left + strings.Repeat(" ", gap) + right
}

// stateColor selects the error color when the state reports a failure and the
// accent color otherwise.
func (s *Status) stateColor(theme Theme) lipgloss.Color {
	if strings.HasPrefix(s.state, "error") {
		return theme.Error()
	}
	return theme.Accent()
}

// SetSize stores the available width; the height is fixed at one line.
func (s *Status) SetSize(width, _ int) { s.width = width }

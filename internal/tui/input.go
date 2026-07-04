package tui

import (
	"github.com/charmbracelet/bubbles/textarea"
	tea "github.com/charmbracelet/bubbletea"
)

// inputPlaceholder is shown in the prompt area before the user types.
const inputPlaceholder = "Ask agens…"

// Input is the prompt component: a thin wrapper over a bubbles textarea that
// exposes only the operations the root model needs. The root intercepts Enter
// for submit, so the textarea itself never has to treat Enter as a newline.
type Input struct {
	ta textarea.Model
}

// NewInput constructs a focused, single-purpose prompt input with the agens
// placeholder. It is focused on construction so the caller can immediately
// return the blink command from the model's Init.
func NewInput() *Input {
	ta := textarea.New()
	ta.Placeholder = inputPlaceholder
	ta.ShowLineNumbers = false
	ta.Focus()

	return &Input{ta: ta}
}

var _ Component = (*Input)(nil)

// Value returns the current text the user has entered.
func (i *Input) Value() string { return i.ta.Value() }

// Reset clears the entered text, returning the input to its empty state.
func (i *Input) Reset() { i.ta.Reset() }

// Focus focuses the input and returns the textarea's blink command.
func (i *Input) Focus() tea.Cmd { return i.ta.Focus() }

// Blur removes focus from the input.
func (i *Input) Blur() { i.ta.Blur() }

// Focused reports whether the input currently has focus.
func (i *Input) Focused() bool { return i.ta.Focused() }

// Init implements tea.Model. The input has no startup command of its own; the
// root model owns focus and returns the blink command from its own Init.
func (i *Input) Init() tea.Cmd { return nil }

// Update forwards msg to the underlying textarea.
func (i *Input) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	var cmd tea.Cmd
	i.ta, cmd = i.ta.Update(msg)
	return i, cmd
}

// View renders the prompt input.
func (i *Input) View() string { return i.ta.View() }

// SetSize sizes the textarea to the given width and a short height.
func (i *Input) SetSize(width, height int) {
	i.ta.SetWidth(width)
	i.ta.SetHeight(height)
}

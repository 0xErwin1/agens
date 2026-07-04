package tui

import (
	"github.com/charmbracelet/bubbles/key"
	"github.com/charmbracelet/bubbles/textarea"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// inputPlaceholder is shown in the prompt area before the user types.
const inputPlaceholder = "Ask agens…"

// inputBorderRows and inputBorderCols are the rows/columns the surrounding
// rounded border and its horizontal padding consume, subtracted when sizing
// the inner textarea so the framed input fits its allotted space.
const (
	inputBorderRows = 2 // top and bottom border
	inputBorderCols = 4 // left/right border (2) + horizontal padding (2)
)

// Input is the prompt component: a bubbles textarea wrapped in a rounded
// border so the prompt reads as a clearly delimited box. The root intercepts
// Enter for submit, so the textarea itself never has to treat Enter as a
// newline.
type Input struct {
	ta    textarea.Model
	width int
}

// NewInput constructs a focused, single-purpose prompt input with the agens
// placeholder. It is focused on construction so the caller can immediately
// return the blink command from the model's Init. The textarea's own prompt
// bar is disabled; the surrounding border provides the framing instead.
func NewInput() *Input {
	ta := textarea.New()
	ta.Placeholder = inputPlaceholder
	ta.ShowLineNumbers = false
	ta.Prompt = ""

	// Fill the textarea's own cells with the surface color so the box interior
	// is never transparent (the terminal background must not show through).
	surface := CurrentTheme().Surface()
	for _, style := range []*textarea.Style{&ta.FocusedStyle, &ta.BlurredStyle} {
		style.Base = style.Base.Background(surface)
		style.Text = style.Text.Background(surface)
		style.Placeholder = style.Placeholder.Background(surface)
		style.CursorLine = style.CursorLine.Background(surface)
		style.EndOfBuffer = style.EndOfBuffer.Background(surface)
	}

	// Word-wise navigation on Ctrl+arrows, in addition to the textarea's
	// default Alt+arrows, matching common terminal editors.
	ta.KeyMap.WordForward = key.NewBinding(key.WithKeys("ctrl+right", "alt+right", "alt+f"))
	ta.KeyMap.WordBackward = key.NewBinding(key.WithKeys("ctrl+left", "alt+left", "alt+b"))

	ta.Focus()

	return &Input{ta: ta}
}

var _ Component = (*Input)(nil)

// Value returns the current text the user has entered.
func (i *Input) Value() string { return i.ta.Value() }

// Reset clears the entered text, returning the input to its empty state.
func (i *Input) Reset() { i.ta.Reset() }

// SetValue replaces the entered text and moves the cursor to the end, used by
// the command palette to complete a highlighted command into the input.
func (i *Input) SetValue(s string) {
	i.ta.SetValue(s)
	i.ta.CursorEnd()
}

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

// View renders the prompt input inside a rounded, accent-colored border that
// spans the full width, so the prompt is clearly delimited.
func (i *Input) View() string {
	theme := CurrentTheme()
	box := lipgloss.NewStyle().
		Border(lipgloss.RoundedBorder()).
		BorderForeground(theme.Accent()).
		BorderBackground(theme.Surface()).
		Background(theme.Surface()).
		Padding(0, 1)
	if i.width > inputBorderRows {
		box = box.Width(i.width - inputBorderRows) // border adds the remaining columns
	}

	return box.Render(i.ta.View())
}

// SetSize sizes the inner textarea to fit within the border and padding of the
// framed input.
func (i *Input) SetSize(width, height int) {
	i.width = width

	innerWidth := width - inputBorderCols
	if innerWidth < 1 {
		innerWidth = 1
	}
	innerHeight := height - inputBorderRows
	if innerHeight < 1 {
		innerHeight = 1
	}

	i.ta.SetWidth(innerWidth)
	i.ta.SetHeight(innerHeight)
}

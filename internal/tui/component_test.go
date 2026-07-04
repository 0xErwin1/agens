package tui

import (
	"testing"

	tea "github.com/charmbracelet/bubbletea"
)

// fakeComponent is a minimal tea.Model that also implements SetSize, used
// only to assert at compile time that the Component interface shape can be
// satisfied by an ordinary Bubble Tea model plus a resize hook.
type fakeComponent struct {
	width  int
	height int
}

func (fakeComponent) Init() tea.Cmd { return nil }

func (f fakeComponent) Update(tea.Msg) (tea.Model, tea.Cmd) { return f, nil }

func (fakeComponent) View() string { return "" }

func (f *fakeComponent) SetSize(width, height int) {
	f.width = width
	f.height = height
}

var _ Component = (*fakeComponent)(nil)

// TestComponent_SetSize verifies that a Component, when resized through the
// interface, actually applies the new dimensions rather than ignoring them.
func TestComponent_SetSize(t *testing.T) {
	var c Component = &fakeComponent{}

	c.SetSize(80, 24)

	got := c.(*fakeComponent)
	if got.width != 80 || got.height != 24 {
		t.Fatalf("SetSize(80, 24) = width %d, height %d; want 80, 24", got.width, got.height)
	}
}

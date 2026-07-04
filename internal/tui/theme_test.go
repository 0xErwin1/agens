package tui

import (
	"testing"

	"github.com/charmbracelet/lipgloss"
)

func TestDefaultTheme_EveryRoleReturnsANonEmptyColor(t *testing.T) {
	theme := DefaultTheme{}

	roles := map[string]lipgloss.Color{
		"Accent":    theme.Accent(),
		"User":      theme.User(),
		"Assistant": theme.Assistant(),
		"Tool":      theme.Tool(),
		"Muted":     theme.Muted(),
		"Error":     theme.Error(),
		"Surface":   theme.Surface(),
	}

	for role, color := range roles {
		if string(color) == "" {
			t.Fatalf("DefaultTheme.%s() = empty, want a non-empty color", role)
		}
	}
}

func TestAyuTheme_EveryRoleReturnsANonEmptyColor(t *testing.T) {
	theme := AyuTheme{}

	roles := map[string]lipgloss.Color{
		"Accent":    theme.Accent(),
		"User":      theme.User(),
		"Assistant": theme.Assistant(),
		"Tool":      theme.Tool(),
		"Muted":     theme.Muted(),
		"Error":     theme.Error(),
		"Surface":   theme.Surface(),
	}

	for role, color := range roles {
		if string(color) == "" {
			t.Fatalf("AyuTheme.%s() = empty, want a non-empty color", role)
		}
	}
}

func TestDefaultActiveThemeIsAyu(t *testing.T) {
	if _, ok := CurrentTheme().(AyuTheme); !ok {
		t.Fatalf("CurrentTheme() = %T, want AyuTheme as the default active theme", CurrentTheme())
	}
}

func TestSetThemeCurrentThemeRoundTrip(t *testing.T) {
	original := CurrentTheme()
	t.Cleanup(func() { SetTheme(original) })

	custom := stubTheme{}
	SetTheme(custom)

	if got := CurrentTheme(); got != custom {
		t.Fatalf("CurrentTheme() = %#v, want the theme set via SetTheme (%#v)", got, custom)
	}
}

// stubTheme is a trivial Theme implementation used to prove SetTheme/CurrentTheme
// swap the active theme without touching any rendering component.
type stubTheme struct{}

func (stubTheme) Accent() lipgloss.Color    { return "1" }
func (stubTheme) User() lipgloss.Color      { return "2" }
func (stubTheme) Assistant() lipgloss.Color { return "3" }
func (stubTheme) Tool() lipgloss.Color      { return "4" }
func (stubTheme) Muted() lipgloss.Color     { return "5" }
func (stubTheme) Error() lipgloss.Color     { return "6" }
func (stubTheme) Surface() lipgloss.Color   { return "7" }

package tui

import "github.com/charmbracelet/lipgloss"

// Theme provides the semantic colors the TUI renders with. Implementations
// return lipgloss colors; a component asks the theme for a role, never a
// hardcoded hex, so introducing a new theme is a single struct rather than an
// edit spread across every component.
type Theme interface {
	Accent() lipgloss.Color    // agens/brand accent, status line
	User() lipgloss.Color      // user message
	Assistant() lipgloss.Color // assistant text
	Tool() lipgloss.Color      // tool call header
	Muted() lipgloss.Color     // dim/secondary (tool results, hints)
	Error() lipgloss.Color     // errors
}

// DefaultTheme is a dark palette used when no other theme has been installed.
type DefaultTheme struct{}

var _ Theme = DefaultTheme{}

func (DefaultTheme) Accent() lipgloss.Color    { return lipgloss.Color("#7D56F4") }
func (DefaultTheme) User() lipgloss.Color      { return lipgloss.Color("#3DDC97") }
func (DefaultTheme) Assistant() lipgloss.Color { return lipgloss.Color("#E4E4E4") }
func (DefaultTheme) Tool() lipgloss.Color      { return lipgloss.Color("#48CAE4") }
func (DefaultTheme) Muted() lipgloss.Color     { return lipgloss.Color("#7A7A7A") }
func (DefaultTheme) Error() lipgloss.Color     { return lipgloss.Color("#FF6B6B") }

// currentTheme is the active theme every component renders against. The TUI
// runs on a single goroutine (the Bubble Tea update loop), so no mutex guards
// this — SetTheme must not be called concurrently with rendering.
var currentTheme Theme = DefaultTheme{}

// CurrentTheme returns the active theme; components call this rather than
// referencing colors directly.
func CurrentTheme() Theme { return currentTheme }

// SetTheme installs a new active theme for subsequent renders.
func SetTheme(t Theme) { currentTheme = t }

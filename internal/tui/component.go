// Package tui implements the Bubble Tea terminal UI for the interactive
// agent: a root model composes Components and consumes loop events
// delivered by the bridge.
package tui

import (
	tea "github.com/charmbracelet/bubbletea"
)

// Component is the contract every TUI child satisfies: a Bubble Tea model
// that can be resized. SetSize is called by the root model on a
// tea.WindowSizeMsg and whenever layout changes, so components never read a
// global for their size.
type Component interface {
	tea.Model
	SetSize(width, height int)
}

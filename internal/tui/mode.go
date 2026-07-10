package tui

import (
	"strings"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/0xErwin1/agens/internal/permission"
)

// modeLabel returns the display name for m: "chat" or "edit".
func modeLabel(m permission.Mode) string {
	if m == permission.ModeChat {
		return "chat"
	}
	return "edit"
}

// modeStatusLabel returns the status-bar label for m: empty for edit (the
// unrestricted default, not worth a segment of its own), "chat" otherwise, so
// the bar only draws attention to the mode when writes are actually blocked.
func modeStatusLabel(m permission.Mode) string {
	if m == permission.ModeChat {
		return "chat"
	}
	return ""
}

// parseModeArg resolves the /mode command's argument against current: a blank
// argument flips between the two modes, "chat" and "edit" (case-insensitively,
// trimmed) set that mode explicitly, and anything else is rejected.
func parseModeArg(arg string, current permission.Mode) (permission.Mode, bool) {
	switch strings.ToLower(strings.TrimSpace(arg)) {
	case "":
		if current == permission.ModeChat {
			return permission.ModeEdit, true
		}
		return permission.ModeChat, true
	case "chat":
		return permission.ModeChat, true
	case "edit":
		return permission.ModeEdit, true
	default:
		return 0, false
	}
}

// ToggleMode implements CommandContext: it sets or flips the shared
// ModeState live — the running Engine reads the new mode on its very next
// Evaluate call, with no loop rebuild, mirroring how the live task.Catalog
// takes effect. It is a no-op (with a note) when no ModeState is wired (for
// example a --dangerously-allow-all session never installs one) or arg names
// neither mode.
func (m *Model) ToggleMode(arg string) tea.Cmd {
	if m.modeState == nil {
		m.messages.AddInfo("mode switching not available")
		return nil
	}

	next, ok := parseModeArg(arg, m.modeState.Get())
	if !ok {
		m.messages.AddInfo(`usage: /mode [chat|edit]`)
		return nil
	}

	m.modeState.Set(next)
	m.status.SetMode(modeStatusLabel(next))
	m.messages.AddInfo("mode switched to " + modeLabel(next))
	return nil
}

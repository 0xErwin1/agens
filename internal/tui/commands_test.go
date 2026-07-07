package tui

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/0xErwin1/agens/internal/message"
)

func typeString(m *Model, s string) {
	for _, r := range s {
		sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune{r}})
	}
}

func TestRegistryMatch(t *testing.T) {
	r := defaultCommands()

	if got := r.Match("/"); len(got) != len(r.All()) {
		t.Fatalf("Match(%q) matched %d, want all %d", "/", len(got), len(r.All()))
	}
	if got := r.Match("/m"); len(got) != 1 || got[0].Name != "/model" {
		t.Fatalf("Match(%q) = %v, want just /model", "/m", got)
	}
	if got := r.Match("hola"); got != nil {
		t.Fatalf("Match(%q) = %v, want nil for non-command input", "hola", got)
	}
	if got := r.Match("/model gpt-5.5"); len(got) != 1 || got[0].Name != "/model" {
		t.Fatalf("Match with an argument = %v, want the command matched on its token", got)
	}
}

func TestRegistryLookup(t *testing.T) {
	r := defaultCommands()

	if c, ok := r.Lookup("/help"); !ok || c.Name != "/help" {
		t.Fatalf("Lookup(/help) = (%v, %v), want /help", c, ok)
	}
	if _, ok := r.Lookup("/nope"); ok {
		t.Fatal("Lookup(/nope) matched, want no match")
	}
}

func TestRegistryRegisterIsExtensible(t *testing.T) {
	r := NewCommandRegistry()
	ran := false
	r.Register(Command{Name: "/ping", Desc: "test", Run: func(CommandContext) tea.Cmd {
		ran = true
		return nil
	}})

	c, ok := r.Lookup("/ping")
	if !ok {
		t.Fatal("registered command not found by Lookup")
	}
	c.Run(nil)
	if !ran {
		t.Fatal("registered command's handler did not run")
	}
}

func TestModel_SlashShowsPalette(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	typeString(m, "/")

	if !m.showPalette {
		t.Fatal("typing '/' did not open the command palette")
	}
	if len(m.paletteItems) != len(m.commands.All()) {
		t.Fatalf("palette shows %d items, want all %d", len(m.paletteItems), len(m.commands.All()))
	}
	if !strings.Contains(m.View(), "/model") {
		t.Fatalf("View() = %q, want the palette to list commands", m.View())
	}
}

func TestModel_TabAndShiftTabCyclePalette(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")
	typeString(m, "/")

	if m.paletteIdx != 0 {
		t.Fatalf("initial selection = %d, want 0", m.paletteIdx)
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyTab})
	if m.paletteIdx != 1 {
		t.Fatalf("selection after Tab = %d, want 1", m.paletteIdx)
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyShiftTab})
	if m.paletteIdx != 0 {
		t.Fatalf("selection after Shift+Tab = %d, want 0", m.paletteIdx)
	}

	// Shift+Tab from the top wraps to the last item.
	sendKey(m, tea.KeyMsg{Type: tea.KeyShiftTab})
	if want := len(m.paletteItems) - 1; m.paletteIdx != want {
		t.Fatalf("selection after wrap = %d, want %d", m.paletteIdx, want)
	}

	// Tab from the bottom wraps back to the top.
	sendKey(m, tea.KeyMsg{Type: tea.KeyTab})
	if m.paletteIdx != 0 {
		t.Fatalf("selection after wrap forward = %d, want 0", m.paletteIdx)
	}
}

func TestModel_ClearCommandResetsConversation(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")
	m.history = []message.Message{message.NewMessage(message.RoleUser, message.TextPart{Text: "x"})}
	m.messages.AppendUser("x")

	typeString(m, "/clear")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if len(m.history) != 0 {
		t.Fatalf("history not cleared by /clear: %d messages remain", len(m.history))
	}
	if m.showPalette {
		t.Fatal("palette still open after running a command")
	}
}

func TestModel_ModelCommandWithoutListerReportsUnavailable(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5") // sized() wires no lister

	typeString(m, "/model")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if !strings.Contains(stripANSI(m.View()), "unavailable") {
		t.Fatalf("View() = %q, want /model to report the selector is unavailable without a lister", stripANSI(m.View()))
	}
}

func TestModel_UnknownCommandReportsError(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	typeString(m, "/nope")
	if m.showPalette {
		t.Fatal("an unknown command should not keep the palette open")
	}
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if !strings.Contains(stripANSI(m.View()), "unknown command") {
		t.Fatalf("View() = %q, want an unknown-command note", stripANSI(m.View()))
	}
}

func TestModel_QuitCommandReturnsQuit(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	typeString(m, "/quit")
	cmd := sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if cmd == nil {
		t.Fatal("/quit returned no command, want tea.Quit")
	}
	if _, ok := cmd().(tea.QuitMsg); !ok {
		t.Fatalf("/quit command produced %T, want tea.QuitMsg", cmd())
	}
}

func TestModel_SlashDoesNotSubmitAsChat(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	typeString(m, "/help")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if m.running {
		t.Fatal("a slash command started a chat turn, want it handled as a command")
	}
}

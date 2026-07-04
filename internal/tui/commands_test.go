package tui

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/iperez/agens/internal/message"
)

func typeString(m *Model, s string) {
	for _, r := range s {
		sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune{r}})
	}
}

func TestMatchCommands(t *testing.T) {
	if got := matchCommands("/"); len(got) != len(commands) {
		t.Fatalf("matchCommands(%q) matched %d, want all %d", "/", len(got), len(commands))
	}
	if got := matchCommands("/m"); len(got) != 1 || got[0].name != "/model" {
		t.Fatalf("matchCommands(%q) = %v, want just /model", "/m", got)
	}
	if got := matchCommands("hola"); got != nil {
		t.Fatalf("matchCommands(%q) = %v, want nil for non-command input", "hola", got)
	}
	if got := matchCommands("/model gpt-5.5"); len(got) != 1 || got[0].name != "/model" {
		t.Fatalf("matchCommands with an argument = %v, want the command matched on its token", got)
	}
}

func TestLookupCommand(t *testing.T) {
	if c, ok := lookupCommand("/help"); !ok || c.name != "/help" {
		t.Fatalf("lookupCommand(/help) = (%v, %v), want /help", c, ok)
	}
	if _, ok := lookupCommand("/nope"); ok {
		t.Fatal("lookupCommand(/nope) matched, want no match")
	}
}

func TestModel_SlashShowsPalette(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	typeString(m, "/")

	if !m.showPalette {
		t.Fatal("typing '/' did not open the command palette")
	}
	if len(m.paletteItems) != len(commands) {
		t.Fatalf("palette shows %d items, want all %d", len(m.paletteItems), len(commands))
	}
	if !strings.Contains(m.View(), "/model") {
		t.Fatalf("View() = %q, want the palette to list commands", m.View())
	}
}

func TestModel_TabCompletesCommand(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	typeString(m, "/h")
	sendKey(m, tea.KeyMsg{Type: tea.KeyTab})

	if got := m.input.Value(); got != "/help " {
		t.Fatalf("input after Tab = %q, want %q", got, "/help ")
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

func TestModel_ModelCommandReportsCurrentModel(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	typeString(m, "/model")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if !strings.Contains(stripANSI(m.View()), "modelo actual: gpt-5.5") {
		t.Fatalf("View() = %q, want /model to report the current model", stripANSI(m.View()))
	}
}

func TestModel_UnknownCommandReportsError(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")

	typeString(m, "/nope")
	if m.showPalette {
		t.Fatal("an unknown command should not keep the palette open")
	}
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if !strings.Contains(stripANSI(m.View()), "comando desconocido") {
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

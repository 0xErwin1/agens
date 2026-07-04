package tui

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
)

func typeRunes(in *Input, s string) {
	in.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(s)})
}

func hasKey(keys []string, want string) bool {
	for _, k := range keys {
		if k == want {
			return true
		}
	}
	return false
}

func TestInput_CtrlArrowsNavigateWords(t *testing.T) {
	i := NewInput()

	if !hasKey(i.ta.KeyMap.WordForward.Keys(), "ctrl+right") {
		t.Fatalf("WordForward keys = %v, want ctrl+right bound", i.ta.KeyMap.WordForward.Keys())
	}
	if !hasKey(i.ta.KeyMap.WordBackward.Keys(), "ctrl+left") {
		t.Fatalf("WordBackward keys = %v, want ctrl+left bound", i.ta.KeyMap.WordBackward.Keys())
	}
	// The textarea's default alt+arrows must still work.
	if !hasKey(i.ta.KeyMap.WordForward.Keys(), "alt+right") {
		t.Fatalf("WordForward keys = %v, want alt+right kept", i.ta.KeyMap.WordForward.Keys())
	}
}

func TestInput_ValueReflectsTypedText(t *testing.T) {
	in := NewInput()
	in.SetSize(40, 3)

	typeRunes(in, "hello")

	if got := in.Value(); got != "hello" {
		t.Fatalf("Value() = %q, want %q", got, "hello")
	}
}

func TestInput_ResetClearsValue(t *testing.T) {
	in := NewInput()
	in.SetSize(40, 3)

	typeRunes(in, "draft prompt")
	if in.Value() == "" {
		t.Fatal("precondition failed: Value() is empty after typing")
	}

	in.Reset()

	if got := in.Value(); got != "" {
		t.Fatalf("Value() after Reset() = %q, want empty", got)
	}
}

func TestInput_PlaceholderShownWhenEmpty(t *testing.T) {
	in := NewInput()
	in.SetSize(40, 3)

	if got := in.View(); !strings.Contains(got, "Ask agens") {
		t.Fatalf("View() = %q, want it to contain the placeholder %q", got, "Ask agens")
	}
}

func TestInput_FocusReturnsCommand(t *testing.T) {
	in := NewInput()

	if cmd := in.Focus(); cmd == nil {
		t.Fatal("Focus() = nil, want a non-nil blink command")
	}
}

func TestInput_BlurThenFocusTogglesFocused(t *testing.T) {
	in := NewInput()
	in.SetSize(40, 3)

	in.Blur()
	if in.Focused() {
		t.Fatal("Focused() = true after Blur(), want false")
	}

	in.Focus()
	if !in.Focused() {
		t.Fatal("Focused() = false after Focus(), want true")
	}
}

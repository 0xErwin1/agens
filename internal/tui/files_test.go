package tui

import (
	"errors"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/iperez/agens/internal/message"
)

// fakeFileSource is a FileSource double over an in-memory file map.
type fakeFileSource struct {
	files   []string
	content map[string]string
}

func (f fakeFileSource) List() ([]string, error) { return f.files, nil }

func (f fakeFileSource) Read(path string) (string, error) {
	if c, ok := f.content[path]; ok {
		return c, nil
	}
	return "", errors.New("not found")
}

func TestAtToken(t *testing.T) {
	cases := []struct {
		value string
		token string
		start int
		ok    bool
	}{
		{"@", "", 0, true},
		{"@src/ma", "src/ma", 0, true},
		{"explain @cmd/main", "cmd/main", 8, true},
		{"@file done", "", 0, false}, // space after ends the reference
		{"no ref here", "", 0, false},
		{"email@example", "", 0, false}, // @ not at a word boundary
	}
	for _, c := range cases {
		token, start, ok := atToken(c.value)
		if ok != c.ok || (ok && (token != c.token || start != c.start)) {
			t.Fatalf("atToken(%q) = (%q,%d,%v), want (%q,%d,%v)", c.value, token, start, ok, c.token, c.start, c.ok)
		}
	}
}

func TestFilterFiles(t *testing.T) {
	files := []string{"cmd/main.go", "internal/tui/tui.go", "README.md", "internal/main_test.go"}

	got := filterFiles(files, "main")
	if len(got) != 2 || got[0] != "cmd/main.go" {
		t.Fatalf("filterFiles(main) = %v, want prefix match cmd/main.go first", got)
	}
	if len(filterFiles(files, "")) != len(files) {
		t.Fatal("empty query should return all files")
	}
}

func TestExtractFileRefs(t *testing.T) {
	known := map[string]struct{}{"cmd/main.go": {}, "README.md": {}}

	refs := extractFileRefs("look at @cmd/main.go and @nope and @README.md", known)
	if len(refs) != 2 || refs[0] != "cmd/main.go" || refs[1] != "README.md" {
		t.Fatalf("extractFileRefs = %v, want the two known files only", refs)
	}
}

func sizedWithFiles(src FileSource) *Model {
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5", Files: src})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})
	// Deliver the startup file index.
	m.Update(filesLoadedMsg{files: src.(fakeFileSource).files})
	return m
}

func TestModel_AtOpensFilePickerAndInserts(t *testing.T) {
	src := fakeFileSource{
		files:   []string{"cmd/main.go", "internal/tui/tui.go"},
		content: map[string]string{"cmd/main.go": "package main"},
	}
	m := sizedWithFiles(src)

	typeString(m, "@main")
	if !m.filePickerOpen {
		t.Fatal("typing @ did not open the file picker")
	}
	if len(m.fileItems) == 0 || m.fileItems[0] != "cmd/main.go" {
		t.Fatalf("file items = %v, want cmd/main.go matched", m.fileItems)
	}
	if !strings.Contains(stripANSI(m.View()), "cmd/main.go") {
		t.Fatalf("View() = %q, want the file listed", stripANSI(m.View()))
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter}) // insert selected

	if m.filePickerOpen {
		t.Fatal("picker still open after inserting")
	}
	if got := m.input.Value(); got != "@cmd/main.go " {
		t.Fatalf("input after insert = %q, want %q", got, "@cmd/main.go ")
	}
}

func TestModel_AtReferenceExpandsFileIntoHistory(t *testing.T) {
	src := fakeFileSource{
		files:   []string{"cmd/main.go"},
		content: map[string]string{"cmd/main.go": "package main // hello"},
	}
	m := sizedWithFiles(src)

	typeString(m, "explain @cmd/main.go ")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter}) // submit

	if len(m.history) != 1 {
		t.Fatalf("history has %d messages, want 1 user message", len(m.history))
	}
	sent := userText(t, m.history[0])
	if !strings.Contains(sent, "package main // hello") {
		t.Fatalf("sent message = %q, want the referenced file inlined", sent)
	}
	// The conversation shows the original text, not the file contents.
	view := stripANSI(m.messages.View())
	if strings.Contains(view, "package main // hello") {
		t.Fatalf("conversation view should show the typed text, not the file body: %q", view)
	}
	if !strings.Contains(view, "@cmd/main.go") {
		t.Fatalf("conversation view = %q, want the original @reference shown", view)
	}
}

func userText(t *testing.T, msg message.Message) string {
	t.Helper()
	for _, p := range msg.Parts {
		if tp, ok := p.(message.TextPart); ok {
			return tp.Text
		}
	}
	return ""
}

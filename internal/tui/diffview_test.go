package tui

import (
	"strings"
	"testing"
)

func TestIsDiffResult(t *testing.T) {
	diff := "--- a/main.go\n+++ b/main.go\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n"
	if !isDiffResult(diff) {
		t.Fatal("isDiffResult() = false for a unified diff, want true")
	}
	if isDiffResult("wrote 12 bytes to main.go") {
		t.Fatal("isDiffResult() = true for a plain message, want false")
	}
}

func TestParseHunkNewStart(t *testing.T) {
	cases := map[string]int{
		"@@ -1,2 +1,2 @@":   1,
		"@@ -10,3 +14,4 @@": 14,
		"@@ -1 +1 @@":       1,
		"@@ -0,0 +1,5 @@":   1,
		"not a hunk":        0,
	}
	for header, want := range cases {
		if got := parseHunkNewStart(header); got != want {
			t.Fatalf("parseHunkNewStart(%q) = %d, want %d", header, got, want)
		}
	}
}

func TestMessages_DiffToolResultRendersFriendlyColoredDiff(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	diff := "--- a/main.go\n+++ b/main.go\n@@ -1,3 +1,3 @@\n first\n-second old\n+second new\n third\n"
	m.AddToolCall("t1", "edit", "main.go")
	m.CompleteToolCall("t1", diff, false, 0)
	m.ToggleDetails()

	view := stripANSI(m.View())

	// The git headers are dropped in favor of a friendly view.
	if strings.Contains(view, "--- a/main.go") || strings.Contains(view, "@@ ") {
		t.Fatalf("View() = %q, want the git diff headers dropped", view)
	}
	// The changed lines are shown with their markers and content.
	if !strings.Contains(view, "- second old") || !strings.Contains(view, "+ second new") {
		t.Fatalf("View() = %q, want the removed and added lines shown", view)
	}
	// Context and additions are gutter-numbered by their line in the new file.
	if !strings.Contains(view, "1 ") || !strings.Contains(view, "3 ") {
		t.Fatalf("View() = %q, want line numbers for context/added lines", view)
	}
}

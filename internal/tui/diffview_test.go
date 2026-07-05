package tui

import (
	"fmt"
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

func TestMessages_ExpandedDiffIsShownInFull(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	var b strings.Builder
	b.WriteString("--- a/big.txt\n+++ b/big.txt\n@@ -0,0 +1,40 @@\n")
	for i := 1; i <= 40; i++ {
		fmt.Fprintf(&b, "+line %d\n", i)
	}

	body := stripANSI(m.renderDiffBody(b.String()))

	if strings.Contains(body, truncationMarker) {
		t.Fatalf("diff body = %q, want no truncation marker (the full diff shows on expand)", body)
	}
	for _, n := range []int{1, 20, 40} {
		if !strings.Contains(body, fmt.Sprintf("line %d", n)) {
			t.Fatalf("diff body missing 'line %d', want every changed line rendered", n)
		}
	}
	if got := strings.Count(body, "line "); got != 40 {
		t.Fatalf("diff body rendered %d changed lines, want all 40", got)
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

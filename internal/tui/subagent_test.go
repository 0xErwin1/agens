package tui

import (
	"strings"
	"testing"
	"time"
)

func TestMessages_SubagentPanelShowsMetaPromptAndTool(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartSubagent("a1", "", "explore", "gpt-5.5", "investigate the parser")
	m.AddSubagentTool("a1", "t1", "read", "internal/parser.go")
	m.UpdateSubagentProgress("a1", 1500, 900*time.Millisecond)

	view := stripANSI(m.View())
	for _, want := range []string{subagentGlyph + " explore", "gpt-5.5", "1.5K tok", "0.9s", "investigate the parser", "read internal/parser.go"} {
		if !strings.Contains(view, want) {
			t.Fatalf("View() = %q, want it to contain %q", view, want)
		}
	}
}

func TestMessages_SubagentCollapsedHidesToolHistoryExpandedShowsItWithResults(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartSubagent("a1", "", "explore", "gpt-5.5", "look around")
	m.AddSubagentTool("a1", "t1", "glob", "**/*.go")
	m.CompleteSubagentTool("a1", "t1", "found 12 files", false)
	m.AddSubagentTool("a1", "t2", "read", "main.go")

	// Collapsed while running shows only the latest tool, not the older one or
	// any result.
	collapsed := stripANSI(m.View())
	if strings.Contains(collapsed, "glob") {
		t.Fatalf("collapsed View() = %q, want the older tool hidden", collapsed)
	}
	if !strings.Contains(collapsed, "read main.go") {
		t.Fatalf("collapsed View() = %q, want the latest tool shown while running", collapsed)
	}

	m.ToggleDetails()

	// Expanded shows every tool and each finished tool's reduced result.
	expanded := stripANSI(m.View())
	if !strings.Contains(expanded, "glob **/*.go") || !strings.Contains(expanded, "read main.go") {
		t.Fatalf("expanded View() = %q, want the full tool history", expanded)
	}
	if !strings.Contains(expanded, "found 12 files") {
		t.Fatalf("expanded View() = %q, want the finished tool's result shown", expanded)
	}
}

func TestMessages_SubagentToolResultIsReducedNotDumped(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 40)

	var b strings.Builder
	for i := 1; i <= 30; i++ {
		b.WriteString("line\n")
	}
	b.WriteString("TAIL_MARKER")

	m.StartSubagent("a1", "", "worker", "gpt-5.5", "grep the tree")
	m.AddSubagentTool("a1", "t1", "grep", "needle")
	m.CompleteSubagentTool("a1", "t1", b.String(), false)
	m.ToggleDetails()

	view := stripANSI(m.View())
	if strings.Contains(view, "TAIL_MARKER") {
		t.Fatalf("View() = %q, want a long subagent tool result reduced, not dumped in full", view)
	}
	if !strings.Contains(view, "…") {
		t.Fatalf("View() = %q, want a truncation marker on the reduced result", view)
	}
}

func TestMessages_SubagentCompletionShowsFailedStatus(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartSubagent("a1", "", "worker", "gpt-5.5", "do work")
	m.AddSubagentTool("a1", "t1", "bash", "go test")
	m.CompleteSubagent("a1", true, 3*time.Second)

	view := stripANSI(m.View())
	if !strings.Contains(view, "failed") {
		t.Fatalf("View() = %q, want a failed marker", view)
	}
}

func TestMessages_NestedSubagentIndentsBeneathParent(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartSubagent("parent", "", "explore", "gpt-5.5", "top")
	m.StartSubagent("child", "parent", "grep", "gpt-5.5", "nested")

	if got := m.findSubagent("parent").depth; got != 0 {
		t.Fatalf("parent depth = %d, want 0 (top-level)", got)
	}
	if got := m.findSubagent("child").depth; got != 1 {
		t.Fatalf("child depth = %d, want 1 (nested one level under its parent)", got)
	}
}

func TestMessages_SubagentLifecycleIgnoresUnknownID(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.AddSubagentTool("ghost", "t1", "read", "x")
	m.CompleteSubagentTool("ghost", "t1", "y", false)
	m.UpdateSubagentProgress("ghost", 100, time.Second)
	m.CompleteSubagent("ghost", false, time.Second)

	if m.findSubagent("ghost") != nil {
		t.Fatal("findSubagent(ghost) != nil, want no panel for an unknown id")
	}
}

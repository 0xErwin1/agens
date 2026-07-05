package tui

import (
	"strings"
	"testing"
	"time"
)

func TestMessages_SubagentPanelShowsMetaAndActivity(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartSubagent("a1", "", "explore", "gpt-5.5", "high")
	m.AddSubagentActivity("a1", "reading files")
	m.UpdateSubagentProgress("a1", 1500, 900*time.Millisecond)

	view := stripANSI(m.View())
	for _, want := range []string{subagentGlyph + " explore", "gpt-5.5", "high", "1.5K tok", "0.9s", "reading files"} {
		if !strings.Contains(view, want) {
			t.Fatalf("View() = %q, want it to contain %q", view, want)
		}
	}
}

func TestMessages_SubagentCollapsedHidesHistoryExpandedShowsIt(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartSubagent("a1", "", "explore", "gpt-5.5", "")
	m.AddSubagentActivity("a1", "step one")
	m.AddSubagentActivity("a1", "step two")

	collapsed := stripANSI(m.View())
	if strings.Contains(collapsed, "step one") {
		t.Fatalf("collapsed View() = %q, want the older activity hidden", collapsed)
	}
	if !strings.Contains(collapsed, "step two") {
		t.Fatalf("collapsed View() = %q, want the latest activity shown while running", collapsed)
	}

	m.ToggleDetails()

	expanded := stripANSI(m.View())
	if !strings.Contains(expanded, "step one") || !strings.Contains(expanded, "step two") {
		t.Fatalf("expanded View() = %q, want the full activity history", expanded)
	}
}

func TestMessages_SubagentCompletionShowsFailedStatus(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartSubagent("a1", "", "worker", "gpt-5.5", "")
	m.AddSubagentActivity("a1", "did some work")
	m.CompleteSubagent("a1", true, 3*time.Second)

	view := stripANSI(m.View())
	if !strings.Contains(view, "failed") {
		t.Fatalf("View() = %q, want a failed marker", view)
	}
	// A finished panel collapses to just its header, hiding its activity.
	if strings.Contains(view, "did some work") {
		t.Fatalf("View() = %q, want a finished panel to hide its activity when collapsed", view)
	}
}

func TestMessages_NestedSubagentIndentsBeneathParent(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartSubagent("parent", "", "explore", "gpt-5.5", "")
	m.StartSubagent("child", "parent", "grep", "gpt-5.5", "")

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

	// None of these panic or create panels for an id that was never started.
	m.AddSubagentActivity("ghost", "x")
	m.UpdateSubagentProgress("ghost", 100, time.Second)
	m.CompleteSubagent("ghost", false, time.Second)

	if m.findSubagent("ghost") != nil {
		t.Fatal("findSubagent(ghost) != nil, want no panel for an unknown id")
	}
}

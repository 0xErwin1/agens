package tui

import (
	"strings"
	"testing"
	"time"

	"github.com/charmbracelet/x/ansi"
)

// stripANSI removes terminal escape sequences so assertions can match the plain
// text content that glamour and lipgloss wrap in styling.
func stripANSI(s string) string { return ansi.Strip(s) }

func TestMessages_AppendUserRendersPrefixedBlock(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.AppendUser("what is 2+2?")

	view := m.View()
	if !strings.Contains(view, "┃") {
		t.Fatalf("View() = %q, want the user turn's left bar %q", view, "┃")
	}
	if !strings.Contains(view, "what is 2+2?") {
		t.Fatalf("View() = %q, want the user text", view)
	}
}

func TestMessages_StreamingAssistantVisibleBeforeFinish(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartAssistant()
	m.AppendAssistantDelta("hel")
	m.AppendAssistantDelta("lo")

	view := m.View()
	if !strings.Contains(view, "hello") {
		t.Fatalf("View() = %q, want the streamed assistant text %q", view, "hello")
	}
}

func TestMessages_FinishAssistantKeepsTextAcrossFollowingBlocks(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartAssistant()
	m.AppendAssistantDelta("answer text")
	m.FinishAssistant()
	m.AppendUser("next question")

	// Assistant text is markdown, which colors each word as its own span, so
	// strip ANSI before matching the contiguous phrase.
	view := stripANSI(m.View())
	if !strings.Contains(view, "answer text") {
		t.Fatalf("View() = %q, want the finalized assistant text to persist", view)
	}
	if !strings.Contains(view, "next question") {
		t.Fatalf("View() = %q, want the later user block too", view)
	}
}

func TestMessages_ToolCallHeaderShowsCaretNameAndDetail(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.AddToolCall("t1", "bash", "ls -la")

	view := stripANSI(m.View())
	if !strings.Contains(view, "▸ bash") {
		t.Fatalf("View() = %q, want the collapsed tool caret and name %q", view, "▸ bash")
	}
	if !strings.Contains(view, "ls -la") {
		t.Fatalf("View() = %q, want the executed command detail %q", view, "ls -la")
	}
}

func TestMessages_ToolResultFoldedByDefaultAndExpandsOnToggle(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.AddToolCall("t1", "bash", "ls")
	m.CompleteToolCall("t1", "file contents here", false, 1200*time.Millisecond)

	// Folded by default: the header shows the duration but not the result body.
	view := stripANSI(m.View())
	if !strings.Contains(view, "1.2s") {
		t.Fatalf("View() = %q, want the tool duration in the header", view)
	}
	if strings.Contains(view, "file contents here") {
		t.Fatalf("View() = %q, want the result folded away by default", view)
	}

	m.ToggleDetails()

	view = stripANSI(m.View())
	if !strings.Contains(view, "▾ bash") {
		t.Fatalf("View() = %q, want the expanded caret after toggling", view)
	}
	if !strings.Contains(view, "file contents here") {
		t.Fatalf("View() = %q, want the result body shown when expanded", view)
	}
}

func TestMessages_ToolErrorMarksHeaderAndShowsBodyWhenExpanded(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.AddToolCall("t1", "bash", "rm x")
	m.CompleteToolCall("t1", "permission denied", true, 500*time.Millisecond)

	// The failure marker shows on the header even while folded.
	view := stripANSI(m.View())
	if !strings.Contains(view, "failed") {
		t.Fatalf("View() = %q, want a failure marker on a failed tool header", view)
	}
	if strings.Contains(view, "permission denied") {
		t.Fatalf("View() = %q, want the error body folded by default", view)
	}

	m.ToggleDetails()
	if !strings.Contains(stripANSI(m.View()), "permission denied") {
		t.Fatalf("View() = %q, want the error body when expanded", stripANSI(m.View()))
	}
}

func TestMessages_SetErrorRendersMessage(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.SetError("stream closed unexpectedly")

	view := m.View()
	if !strings.Contains(view, "stream closed unexpectedly") {
		t.Fatalf("View() = %q, want the error message", view)
	}
}

func TestMessages_FinalizedAssistantRendersMarkdownContent(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartAssistant()
	m.AppendAssistantDelta("# Hi\n\ncode: `x`")
	m.FinishAssistant()

	view := stripANSI(m.View())
	if !strings.Contains(view, "Hi") {
		t.Fatalf("stripped View() = %q, want the rendered heading text %q", view, "Hi")
	}
	if !strings.Contains(view, "x") {
		t.Fatalf("stripped View() = %q, want the rendered inline-code text %q", view, "x")
	}
}

func TestMessages_StreamingAssistantRendersMarkdownLive(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartAssistant()
	m.AppendAssistantDelta("some **bold** text")

	view := stripANSI(m.View())
	if !strings.Contains(view, "bold") {
		t.Fatalf("stripped View() = %q, want the emphasized word rendered live", view)
	}
	// Live markdown consumes the emphasis markers; raw text would keep them.
	if strings.Contains(view, "**bold**") {
		t.Fatalf("stripped View() = %q, want markdown applied to the streaming text, not raw markers", view)
	}
}

func TestMessages_ToolResultTruncatesLongOutputWhenExpanded(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	var b strings.Builder
	for i := 0; i < toolResultMaxLines+50; i++ {
		b.WriteString("line\n")
	}
	b.WriteString("UNIQUE_TAIL_MARKER")

	m.AddToolCall("t1", "bash", "cat big")
	m.CompleteToolCall("t1", b.String(), false, time.Second)
	m.ToggleDetails()

	view := stripANSI(m.View())
	if !strings.Contains(view, truncationMarker) {
		t.Fatalf("stripped View() = %q, want the truncation marker %q for a long result", view, truncationMarker)
	}
	if strings.Contains(view, "UNIQUE_TAIL_MARKER") {
		t.Fatalf("stripped View() = %q, must not contain the tail past the truncation limit", view)
	}
}

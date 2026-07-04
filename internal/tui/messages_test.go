package tui

import (
	"strings"
	"testing"

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
	if !strings.Contains(view, "You:") {
		t.Fatalf("View() = %q, want the user prefix %q", view, "You:")
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
	if !strings.Contains(view, "agens:") {
		t.Fatalf("View() = %q, want the assistant prefix %q while streaming", view, "agens:")
	}
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

	view := m.View()
	if !strings.Contains(view, "answer text") {
		t.Fatalf("View() = %q, want the finalized assistant text to persist", view)
	}
	if !strings.Contains(view, "next question") {
		t.Fatalf("View() = %q, want the later user block too", view)
	}
}

func TestMessages_AddToolCallRendersArrowAndName(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.AddToolCall("read")

	view := m.View()
	if !strings.Contains(view, "→ read") {
		t.Fatalf("View() = %q, want the tool-call marker %q", view, "→ read")
	}
}

func TestMessages_AddToolResultRendersContent(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.AddToolResult("file contents here", false)

	view := m.View()
	if !strings.Contains(view, "file contents here") {
		t.Fatalf("View() = %q, want the tool-result content", view)
	}
}

func TestMessages_AddToolResultMarksErrors(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.AddToolResult("permission denied", true)

	view := m.View()
	if !strings.Contains(view, "permission denied") {
		t.Fatalf("View() = %q, want the tool-result content", view)
	}
	if !strings.Contains(strings.ToLower(view), "error") {
		t.Fatalf("View() = %q, want an error marker for a failed tool result", view)
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

func TestMessages_StreamingAssistantShowsRawDeltaNotMarkdown(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.StartAssistant()
	m.AppendAssistantDelta("# raw heading")

	view := stripANSI(m.View())
	if !strings.Contains(view, "# raw heading") {
		t.Fatalf("stripped View() = %q, want the raw, unprocessed delta %q while streaming", view, "# raw heading")
	}
}

func TestMessages_AddToolResultTruncatesLongOutput(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	var b strings.Builder
	for i := 0; i < toolResultMaxLines+50; i++ {
		b.WriteString("line\n")
	}
	b.WriteString("UNIQUE_TAIL_MARKER")

	m.AddToolResult(b.String(), false)

	view := stripANSI(m.View())
	if !strings.Contains(view, truncationMarker) {
		t.Fatalf("stripped View() = %q, want the truncation marker %q for a long result", view, truncationMarker)
	}
	if strings.Contains(view, "UNIQUE_TAIL_MARKER") {
		t.Fatalf("stripped View() = %q, must not contain the tail past the truncation limit", view)
	}
}

func TestMessages_AddToolResultErrorContentPresent(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)

	m.AddToolResult("disk is full", true)

	view := stripANSI(m.View())
	if !strings.Contains(view, "disk is full") {
		t.Fatalf("stripped View() = %q, want the error result content", view)
	}
	if !strings.Contains(strings.ToLower(view), "error") {
		t.Fatalf("stripped View() = %q, want an error marker for a failed tool result", view)
	}
}

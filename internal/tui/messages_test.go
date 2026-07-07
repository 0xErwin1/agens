package tui

import (
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/charmbracelet/x/ansi"

	"github.com/0xErwin1/agens/internal/message"
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

func TestMessages_ToolResultTruncatesLongOutputWhenConfigured(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 20)
	m.SetDisplayOptions(false, true) // truncate_tool_output = true

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
		t.Fatalf("stripped View() = %q, want the truncation marker %q when truncation is configured", view, truncationMarker)
	}
	if strings.Contains(view, "UNIQUE_TAIL_MARKER") {
		t.Fatalf("stripped View() = %q, must not contain the tail past the truncation limit", view)
	}
}

func TestMessages_ToolResultShownInFullWhenExpandedByDefault(t *testing.T) {
	m := NewMessages()
	m.SetSize(80, 200) // tall enough that the full result fits the viewport

	var b strings.Builder
	for i := 0; i < toolResultMaxLines+50; i++ {
		b.WriteString("line\n")
	}
	b.WriteString("UNIQUE_TAIL_MARKER")

	m.AddToolCall("t1", "bash", "cat big")
	m.CompleteToolCall("t1", b.String(), false, time.Second)
	m.ToggleDetails()

	view := stripANSI(m.View())
	if strings.Contains(view, truncationMarker) {
		t.Fatalf("stripped View() = %q, want no truncation marker by default (expanded output shown in full)", view)
	}
	if !strings.Contains(view, "UNIQUE_TAIL_MARKER") {
		t.Fatalf("stripped View() = %q, want the full result including its tail when expanded by default", view)
	}
}

func TestMessages_ToolBatchShowsAggregateHeaderAndChildRows(t *testing.T) {
	m := NewMessages()
	m.SetSize(90, 20)

	m.AddToolBatch([]message.ToolUsePart{
		{ID: "t1", Name: "read", Input: json.RawMessage(`{"path":"a.go"}`)},
		{ID: "t2", Name: "bash", Input: json.RawMessage(`{"command":"go test ./..."}`)},
		{ID: "t3", Name: "edit", Input: json.RawMessage(`{"path":"b.go"}`)},
		{ID: "t4", Name: "grep", Input: json.RawMessage(`{"path":"internal"}`)},
	})

	view := stripANSI(m.View())
	if !strings.Contains(view, "Batch 0/4 running") {
		t.Fatalf("View() = %q, want the running batch header", view)
	}
	for _, want := range []string{"▸ read", "a.go", "▸ bash", "go test ./...", "▸ edit", "b.go", "▸ grep", "internal"} {
		if !strings.Contains(view, want) {
			t.Fatalf("View() = %q, want visible child row content %q", view, want)
		}
	}

	m.CompleteToolCall("t1", "read ok", false, 100*time.Millisecond)
	m.CompleteToolCall("t2", "tests ok", false, 200*time.Millisecond)
	m.CompleteToolCall("t3", "edit ok", false, 300*time.Millisecond)
	m.CompleteToolCall("t4", "grep ok", false, 400*time.Millisecond)

	view = stripANSI(m.View())
	if !strings.Contains(view, "Batch 4/4 succeeded") {
		t.Fatalf("View() = %q, want the successful aggregate batch header", view)
	}
	if strings.Contains(view, "tests ok") {
		t.Fatalf("View() = %q, want child result bodies folded by default", view)
	}

	m.ToggleDetails()
	view = stripANSI(m.View())
	if !strings.Contains(view, "tests ok") {
		t.Fatalf("View() = %q, want child result bodies inspectable after expanding", view)
	}
}

func TestMessages_ToolBatchFailureSummaryKeepsFailedChildVisible(t *testing.T) {
	m := NewMessages()
	m.SetSize(90, 20)

	m.AddToolBatch([]message.ToolUsePart{
		{ID: "t1", Name: "bash", Input: json.RawMessage(`{"command":"rm secret"}`)},
		{ID: "t2", Name: "read", Input: json.RawMessage(`{"path":"safe.go"}`)},
	})
	m.CompleteToolCall("t1", "permission denied", true, time.Second)
	m.CompleteToolCall("t2", "safe contents", false, time.Second)

	view := stripANSI(m.View())
	if !strings.Contains(view, "Batch 2/2 completed · 1 failed") {
		t.Fatalf("View() = %q, want the failed aggregate batch header", view)
	}
	if !strings.Contains(view, "▸ bash") || !strings.Contains(view, "failed") {
		t.Fatalf("View() = %q, want the failed child row to remain visible and marked", view)
	}

	m.ToggleDetails()
	if view = stripANSI(m.View()); !strings.Contains(view, "permission denied") {
		t.Fatalf("View() = %q, want the failed child result body inspectable", view)
	}
}

func TestMessages_ToolBatchIDsDoNotBleedAcrossRepeatedToolIDs(t *testing.T) {
	m := NewMessages()
	m.SetSize(90, 20)

	calls := []message.ToolUsePart{
		{ID: "reused", Name: "read", Input: json.RawMessage(`{"path":"a.go"}`)},
		{ID: "first-bash", Name: "bash", Input: json.RawMessage(`{"command":"go test ./a"}`)},
	}
	m.AddToolBatch(calls)
	m.CompleteToolCall("reused", "a", false, time.Second)
	m.CompleteToolCall("first-bash", "ok", false, time.Second)

	m.AddToolBatch([]message.ToolUsePart{
		{ID: "reused", Name: "read", Input: json.RawMessage(`{"path":"b.go"}`)},
		{ID: "second-bash", Name: "bash", Input: json.RawMessage(`{"command":"go test ./b"}`)},
	})

	view := stripANSI(m.View())
	if strings.Count(view, "Batch 2/2 succeeded") != 1 {
		t.Fatalf("View() = %q, want exactly one completed batch", view)
	}
	if strings.Count(view, "Batch 0/2 running") != 1 {
		t.Fatalf("View() = %q, want second batch to remain independently running", view)
	}
}

func TestMessages_SetHistoryReconstructsAssistantToolBatch(t *testing.T) {
	m := NewMessages()
	m.SetSize(90, 20)

	history := []message.Message{
		message.NewMessage(message.RoleAssistant,
			message.TextPart{Text: "I will inspect both files."},
			message.ToolUsePart{ID: "t1", Name: "read", Input: json.RawMessage(`{"path":"a.go"}`)},
			message.ToolUsePart{ID: "t2", Name: "read", Input: json.RawMessage(`{"path":"b.go"}`)},
		),
		message.NewMessage(message.RoleUser,
			message.ToolResultPart{ToolUseID: "t1", Content: message.Parts{message.TextPart{Text: "a contents"}}},
			message.ToolResultPart{ToolUseID: "t2", Content: message.Parts{message.TextPart{Text: "b contents"}}},
		),
	}

	m.SetHistory(history)

	view := stripANSI(m.View())
	if !strings.Contains(view, "Batch 2/2 succeeded") {
		t.Fatalf("View() = %q, want history reconstructed as a completed tool batch", view)
	}
	if strings.Count(view, "▸ read") != 2 {
		t.Fatalf("View() = %q, want both historical child tool rows visible", view)
	}

	m.ToggleDetails()
	view = stripANSI(m.View())
	for _, want := range []string{"a contents", "b contents"} {
		if !strings.Contains(view, want) {
			t.Fatalf("View() = %q, want historical child result %q inspectable", view, want)
		}
	}
}

package provider

import (
	"errors"
	"io"
	"testing"
)

func TestEventTypeString(t *testing.T) {
	tests := []struct {
		name string
		typ  EventType
		want string
	}{
		{"text delta", EventTextDelta, "text_delta"},
		{"tool call start", EventToolCallStart, "tool_call_start"},
		{"tool args delta", EventToolArgsDelta, "tool_args_delta"},
		{"tool call end", EventToolCallEnd, "tool_call_end"},
		{"usage", EventUsage, "usage"},
		{"done", EventDone, "done"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := tt.typ.String(); got != tt.want {
				t.Fatalf("EventType.String() = %q, want %q", got, tt.want)
			}
		})
	}
}

// scriptedReader replays a fixed sequence of StreamEvent values, returning
// io.EOF once the script is exhausted, mirroring the StreamReader contract
// without depending on the Provider/StreamReader interfaces (WU-2).
type scriptedReader struct {
	events []StreamEvent
	pos    int
}

func (r *scriptedReader) Recv() (StreamEvent, error) {
	if r.pos >= len(r.events) {
		return StreamEvent{}, io.EOF
	}
	ev := r.events[r.pos]
	r.pos++
	return ev, nil
}

func TestStreamEventFieldValidityByType(t *testing.T) {
	script := []StreamEvent{
		{Type: EventTextDelta, Text: "hello"},
		{Type: EventToolCallStart, ToolCallID: "call-1", ToolName: "search"},
		{Type: EventToolArgsDelta, ToolCallID: "call-1", ArgsDelta: `{"q":`},
		{Type: EventToolCallEnd, ToolCallID: "call-1"},
		{Type: EventUsage, Usage: &Usage{InputTokens: 10, OutputTokens: 5}},
		{Type: EventDone, StopReason: "end_turn"},
	}
	reader := &scriptedReader{events: script}

	var got []StreamEvent
	for {
		ev, err := reader.Recv()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			t.Fatalf("Recv() unexpected error = %v", err)
		}
		got = append(got, ev)
	}

	if len(got) != len(script) {
		t.Fatalf("Recv() delivered %d events, want %d", len(got), len(script))
	}

	for i, ev := range got {
		switch ev.Type {
		case EventTextDelta:
			if ev.Text != "hello" {
				t.Errorf("event %d: Text = %q, want %q", i, ev.Text, "hello")
			}
			if ev.ToolCallID != "" || ev.ToolName != "" || ev.ArgsDelta != "" || ev.Usage != nil || ev.StopReason != "" {
				t.Errorf("event %d: unrelated fields set on EventTextDelta: %+v", i, ev)
			}
		case EventToolCallStart:
			if ev.ToolCallID != "call-1" || ev.ToolName != "search" {
				t.Errorf("event %d: ToolCallID/ToolName = %q/%q, want %q/%q", i, ev.ToolCallID, ev.ToolName, "call-1", "search")
			}
			if ev.Text != "" || ev.ArgsDelta != "" || ev.Usage != nil || ev.StopReason != "" {
				t.Errorf("event %d: unrelated fields set on EventToolCallStart: %+v", i, ev)
			}
		case EventToolArgsDelta:
			if ev.ToolCallID != "call-1" || ev.ArgsDelta != `{"q":` {
				t.Errorf("event %d: ToolCallID/ArgsDelta = %q/%q, want %q/%q", i, ev.ToolCallID, ev.ArgsDelta, "call-1", `{"q":`)
			}
			if ev.Text != "" || ev.ToolName != "" || ev.Usage != nil || ev.StopReason != "" {
				t.Errorf("event %d: unrelated fields set on EventToolArgsDelta: %+v", i, ev)
			}
		case EventToolCallEnd:
			if ev.ToolCallID != "call-1" {
				t.Errorf("event %d: ToolCallID = %q, want %q", i, ev.ToolCallID, "call-1")
			}
			if ev.Text != "" || ev.ArgsDelta != "" || ev.Usage != nil || ev.StopReason != "" {
				t.Errorf("event %d: unrelated fields set on EventToolCallEnd: %+v", i, ev)
			}
		case EventUsage:
			if ev.Usage == nil || ev.Usage.InputTokens != 10 || ev.Usage.OutputTokens != 5 {
				t.Errorf("event %d: Usage = %+v, want {InputTokens:10 OutputTokens:5}", i, ev.Usage)
			}
			if ev.Text != "" || ev.ToolCallID != "" || ev.ToolName != "" || ev.ArgsDelta != "" || ev.StopReason != "" {
				t.Errorf("event %d: unrelated fields set on EventUsage: %+v", i, ev)
			}
		case EventDone:
			if ev.StopReason != "end_turn" {
				t.Errorf("event %d: StopReason = %q, want %q", i, ev.StopReason, "end_turn")
			}
			if ev.Text != "" || ev.ToolCallID != "" || ev.ToolName != "" || ev.ArgsDelta != "" || ev.Usage != nil {
				t.Errorf("event %d: unrelated fields set on EventDone: %+v", i, ev)
			}
		default:
			t.Errorf("event %d: unexpected Type = %v", i, ev.Type)
		}
	}

	if _, err := reader.Recv(); !errors.Is(err, io.EOF) {
		t.Fatalf("Recv() after script exhausted error = %v, want io.EOF", err)
	}
}

package agentloop

import (
	"context"
	"encoding/json"
	"errors"
	"testing"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

func splitParts(t *testing.T, parts message.Parts) (string, []message.ToolUsePart) {
	t.Helper()

	var text string
	var toolParts []message.ToolUsePart

	for _, p := range parts {
		switch v := p.(type) {
		case message.TextPart:
			text = v.Text
		case message.ToolUsePart:
			toolParts = append(toolParts, v)
		default:
			t.Fatalf("unexpected part kind %T", p)
		}
	}

	return text, toolParts
}

func eventKinds(events []LoopEvent) []LoopEventKind {
	kinds := make([]LoopEventKind, len(events))
	for i, e := range events {
		kinds[i] = e.Kind
	}
	return kinds
}

func equalKinds(got, want []LoopEventKind) bool {
	if len(got) != len(want) {
		return false
	}
	for i := range got {
		if got[i] != want[i] {
			return false
		}
	}
	return true
}

func TestAssemble_Success(t *testing.T) {
	usage := &provider.Usage{InputTokens: 10, OutputTokens: 5}

	tests := []struct {
		name           string
		steps          []streamStep
		wantText       string
		wantToolParts  []message.ToolUsePart
		wantStopReason string
		wantUsage      *provider.Usage
		wantEventKinds []LoopEventKind
	}{
		{
			name: "text-only turn",
			steps: []streamStep{
				{ev: provider.StreamEvent{Type: provider.EventTextDelta, Text: "Hello, "}},
				{ev: provider.StreamEvent{Type: provider.EventTextDelta, Text: "world"}},
				{ev: provider.StreamEvent{Type: provider.EventDone, StopReason: "stop"}},
			},
			wantText:       "Hello, world",
			wantStopReason: "stop",
			wantEventKinds: []LoopEventKind{LoopTextDelta, LoopTextDelta},
		},
		{
			name: "fragmented tool call arg deltas",
			steps: []streamStep{
				{ev: provider.StreamEvent{Type: provider.EventToolCallStart, ToolCallID: "call_1", ToolName: "get_weather"}},
				{ev: provider.StreamEvent{Type: provider.EventToolArgsDelta, ToolCallID: "call_1", ArgsDelta: `{"loc`}},
				{ev: provider.StreamEvent{Type: provider.EventToolArgsDelta, ToolCallID: "call_1", ArgsDelta: `ation":"NYC"}`}},
				{ev: provider.StreamEvent{Type: provider.EventToolCallEnd, ToolCallID: "call_1"}},
				{ev: provider.StreamEvent{Type: provider.EventDone, StopReason: "tool_calls"}},
			},
			wantToolParts: []message.ToolUsePart{
				{ID: "call_1", Name: "get_weather", Input: json.RawMessage(`{"location":"NYC"}`)},
			},
			wantStopReason: "tool_calls",
			wantEventKinds: []LoopEventKind{LoopToolCallStarted},
		},
		{
			name: "parallel tool calls preserve order of appearance",
			steps: []streamStep{
				{ev: provider.StreamEvent{Type: provider.EventToolCallStart, ToolCallID: "call_b", ToolName: "b"}},
				{ev: provider.StreamEvent{Type: provider.EventToolCallStart, ToolCallID: "call_a", ToolName: "a"}},
				{ev: provider.StreamEvent{Type: provider.EventToolArgsDelta, ToolCallID: "call_a", ArgsDelta: `{"x":1}`}},
				{ev: provider.StreamEvent{Type: provider.EventToolArgsDelta, ToolCallID: "call_b", ArgsDelta: `{"y":2}`}},
				{ev: provider.StreamEvent{Type: provider.EventToolCallEnd, ToolCallID: "call_b"}},
				{ev: provider.StreamEvent{Type: provider.EventToolCallEnd, ToolCallID: "call_a"}},
				{ev: provider.StreamEvent{Type: provider.EventDone, StopReason: "tool_calls"}},
			},
			wantToolParts: []message.ToolUsePart{
				{ID: "call_b", Name: "b", Input: json.RawMessage(`{"y":2}`)},
				{ID: "call_a", Name: "a", Input: json.RawMessage(`{"x":1}`)},
			},
			wantStopReason: "tool_calls",
			wantEventKinds: []LoopEventKind{LoopToolCallStarted, LoopToolCallStarted},
		},
		{
			name: "empty args finalize as empty object",
			steps: []streamStep{
				{ev: provider.StreamEvent{Type: provider.EventToolCallStart, ToolCallID: "call_1", ToolName: "noop"}},
				{ev: provider.StreamEvent{Type: provider.EventToolCallEnd, ToolCallID: "call_1"}},
				{ev: provider.StreamEvent{Type: provider.EventDone, StopReason: "tool_calls"}},
			},
			wantToolParts: []message.ToolUsePart{
				{ID: "call_1", Name: "noop", Input: json.RawMessage(`{}`)},
			},
			wantStopReason: "tool_calls",
			wantEventKinds: []LoopEventKind{LoopToolCallStarted},
		},
		{
			name: "usage arriving after done is not lost",
			steps: []streamStep{
				{ev: provider.StreamEvent{Type: provider.EventTextDelta, Text: "hi"}},
				{ev: provider.StreamEvent{Type: provider.EventDone, StopReason: "stop"}},
				{ev: provider.StreamEvent{Type: provider.EventUsage, Usage: usage}},
			},
			wantText:       "hi",
			wantStopReason: "stop",
			wantUsage:      usage,
			wantEventKinds: []LoopEventKind{LoopTextDelta, LoopUsage},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			stream := newScriptedStream(tt.steps)
			var events []LoopEvent
			emit := func(e LoopEvent) { events = append(events, e) }

			msg, gotUsage, err := assemble(context.Background(), stream, "gpt-test", 1, emit)
			if err != nil {
				t.Fatalf("assemble() error = %v, want nil", err)
			}

			if stream.closeCount != 1 {
				t.Fatalf("Close() called %d times, want 1", stream.closeCount)
			}

			if msg.Role != message.RoleAssistant {
				t.Fatalf("Role = %q, want %q", msg.Role, message.RoleAssistant)
			}
			if msg.Model != "gpt-test" {
				t.Fatalf("Model = %q, want %q", msg.Model, "gpt-test")
			}
			if msg.StopReason != tt.wantStopReason {
				t.Fatalf("StopReason = %q, want %q", msg.StopReason, tt.wantStopReason)
			}

			gotText, gotToolParts := splitParts(t, msg.Parts)
			if gotText != tt.wantText {
				t.Fatalf("text = %q, want %q", gotText, tt.wantText)
			}
			if len(gotToolParts) != len(tt.wantToolParts) {
				t.Fatalf("tool parts = %+v, want %+v", gotToolParts, tt.wantToolParts)
			}
			for i := range gotToolParts {
				want := tt.wantToolParts[i]
				got := gotToolParts[i]
				if got.ID != want.ID || got.Name != want.Name || string(got.Input) != string(want.Input) {
					t.Fatalf("tool part[%d] = %+v, want %+v", i, got, want)
				}
			}

			if (gotUsage == nil) != (tt.wantUsage == nil) {
				t.Fatalf("usage = %v, want %v", gotUsage, tt.wantUsage)
			}
			if gotUsage != nil && *gotUsage != *tt.wantUsage {
				t.Fatalf("usage = %+v, want %+v", gotUsage, tt.wantUsage)
			}

			gotKinds := eventKinds(events)
			if !equalKinds(gotKinds, tt.wantEventKinds) {
				t.Fatalf("event kinds = %v, want %v", gotKinds, tt.wantEventKinds)
			}
		})
	}
}

func TestAssemble_HardErrors(t *testing.T) {
	tests := []struct {
		name  string
		steps []streamStep
	}{
		{
			name: "invalid JSON tool call arguments",
			steps: []streamStep{
				{ev: provider.StreamEvent{Type: provider.EventToolCallStart, ToolCallID: "call_1", ToolName: "broken"}},
				{ev: provider.StreamEvent{Type: provider.EventToolArgsDelta, ToolCallID: "call_1", ArgsDelta: `{not valid`}},
				{ev: provider.StreamEvent{Type: provider.EventToolCallEnd, ToolCallID: "call_1"}},
				{ev: provider.StreamEvent{Type: provider.EventDone, StopReason: "tool_calls"}},
			},
		},
		{
			name: "args delta for unknown tool call id",
			steps: []streamStep{
				{ev: provider.StreamEvent{Type: provider.EventToolArgsDelta, ToolCallID: "call_unknown", ArgsDelta: "x"}},
			},
		},
		{
			name: "tool call end for unknown tool call id",
			steps: []streamStep{
				{ev: provider.StreamEvent{Type: provider.EventToolCallEnd, ToolCallID: "call_unknown"}},
			},
		},
		{
			name: "duplicate tool call start for the same id",
			steps: []streamStep{
				{ev: provider.StreamEvent{Type: provider.EventToolCallStart, ToolCallID: "call_1", ToolName: "a"}},
				{ev: provider.StreamEvent{Type: provider.EventToolCallStart, ToolCallID: "call_1", ToolName: "a"}},
			},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			stream := newScriptedStream(tt.steps)

			_, _, err := assemble(context.Background(), stream, "gpt-test", 1, func(LoopEvent) {})
			if err == nil {
				t.Fatalf("assemble() error = nil, want a hard error")
			}
			if stream.closeCount != 1 {
				t.Fatalf("Close() called %d times, want 1", stream.closeCount)
			}
		})
	}
}

func TestAssemble_RecvErrorPrioritizesCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	streamErr := errors.New("transport closed")
	stream := newScriptedStream([]streamStep{{err: streamErr}})

	_, _, err := assemble(ctx, stream, "gpt-test", 1, func(LoopEvent) {})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("assemble() error = %v, want errors.Is(err, context.Canceled)", err)
	}
	if errors.Is(err, streamErr) {
		t.Fatalf("assemble() error = %v, want the raw stream error superseded by ctx.Err()", err)
	}
	if stream.closeCount != 1 {
		t.Fatalf("Close() called %d times, want 1", stream.closeCount)
	}
}

func TestAssemble_RecvErrorWithoutCancellationIsWrapped(t *testing.T) {
	streamErr := errors.New("transport closed")
	stream := newScriptedStream([]streamStep{{err: streamErr}})

	_, _, err := assemble(context.Background(), stream, "gpt-test", 1, func(LoopEvent) {})
	if !errors.Is(err, streamErr) {
		t.Fatalf("assemble() error = %v, want wrapping %v", err, streamErr)
	}
	if stream.closeCount != 1 {
		t.Fatalf("Close() called %d times, want 1", stream.closeCount)
	}
}

func TestFakeProvider_StreamRecordsRequestAndReturnsScriptedStream(t *testing.T) {
	p := &fakeProvider{
		steps: []streamStep{
			{ev: provider.StreamEvent{Type: provider.EventTextDelta, Text: "hi"}},
			{ev: provider.StreamEvent{Type: provider.EventDone, StopReason: "stop"}},
		},
	}

	req := provider.ChatRequest{Model: "gpt-test"}
	reader, err := p.Stream(context.Background(), req)
	if err != nil {
		t.Fatalf("Stream() error = %v, want nil", err)
	}
	if p.lastRequest.Model != "gpt-test" {
		t.Fatalf("lastRequest.Model = %q, want %q", p.lastRequest.Model, "gpt-test")
	}

	ev, err := reader.Recv()
	if err != nil || ev.Type != provider.EventTextDelta {
		t.Fatalf("Recv() = %+v, %v, want a text delta event with nil error", ev, err)
	}
}

func TestFakeToolRunner_SpecsAndRun(t *testing.T) {
	call := message.ToolUsePart{ID: "call_1", Name: "echo", Input: json.RawMessage(`{}`)}
	want := message.ToolResultPart{ToolUseID: "call_1", Content: message.Parts{message.TextPart{Text: "ok"}}}

	r := &fakeToolRunner{
		specs:     []provider.ToolSpec{{Name: "echo"}},
		responses: map[string]message.ToolResultPart{"echo": want},
	}

	if len(r.Specs()) != 1 || r.Specs()[0].Name != "echo" {
		t.Fatalf("Specs() = %+v, want one spec named %q", r.Specs(), "echo")
	}

	got, err := r.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if got.ToolUseID != want.ToolUseID {
		t.Fatalf("Run() = %+v, want %+v", got, want)
	}
	if len(r.calls) != 1 || r.calls[0].ID != call.ID {
		t.Fatalf("calls = %+v, want one recorded call with ID %q", r.calls, call.ID)
	}
	if len(r.ctxs) != 1 {
		t.Fatalf("ctxs recorded = %d, want 1", len(r.ctxs))
	}
}

func TestFakeToolRunner_RunReturnsConfiguredError(t *testing.T) {
	wantErr := errors.New("boom")
	r := &fakeToolRunner{errs: map[string]error{"broken": wantErr}}

	_, err := r.Run(context.Background(), message.ToolUsePart{ID: "call_1", Name: "broken"})
	if !errors.Is(err, wantErr) {
		t.Fatalf("Run() error = %v, want %v", err, wantErr)
	}
}

func TestFakeToolRunner_BlocksUntilChannelOrContextDone(t *testing.T) {
	block := make(chan struct{})
	r := &fakeToolRunner{block: block}

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	var gotErr error
	go func() {
		_, gotErr = r.Run(ctx, message.ToolUsePart{ID: "call_1", Name: "slow"})
		close(done)
	}()

	cancel()
	<-done

	if !errors.Is(gotErr, context.Canceled) {
		t.Fatalf("Run() error = %v, want context.Canceled", gotErr)
	}
}

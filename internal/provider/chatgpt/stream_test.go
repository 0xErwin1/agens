package chatgpt

import (
	"errors"
	"io"
	"strings"
	"testing"

	"github.com/iperez/agens/internal/provider"
)

// sseScript joins each already-formatted "data: {...}" JSON line into one
// SSE body, mirroring how a real /responses stream separates events with a
// blank line.
func sseScript(lines ...string) string {
	return strings.Join(lines, "\n\n") + "\n\n"
}

func drainResponsesStream(t *testing.T, script string) ([]provider.StreamEvent, error) {
	t.Helper()

	s := newResponsesStream(io.NopCloser(strings.NewReader(script)))
	var events []provider.StreamEvent
	for {
		ev, err := s.Recv()
		if err != nil {
			return events, err
		}
		events = append(events, ev)
	}
}

func eventsEqual(t *testing.T, got, want []provider.StreamEvent) {
	t.Helper()

	if len(got) != len(want) {
		t.Fatalf("events = %+v, want %+v", got, want)
	}
	for i := range got {
		g, w := got[i], want[i]
		if g.Type != w.Type || g.Text != w.Text || g.ToolCallID != w.ToolCallID ||
			g.ToolName != w.ToolName || g.ArgsDelta != w.ArgsDelta || g.StopReason != w.StopReason {
			t.Fatalf("event[%d] = %+v, want %+v", i, g, w)
		}
		if (g.Usage == nil) != (w.Usage == nil) {
			t.Fatalf("event[%d].Usage = %v, want %v", i, g.Usage, w.Usage)
		}
		if g.Usage != nil && *g.Usage != *w.Usage {
			t.Fatalf("event[%d].Usage = %+v, want %+v", i, g.Usage, w.Usage)
		}
	}
}

func TestResponsesStream_Recv_TextThenCompleted(t *testing.T) {
	script := sseScript(
		`data: {"type":"response.created","response":{"id":"resp_1"}}`,
		`data: {"type":"response.output_text.delta","delta":"Hel"}`,
		`data: {"type":"response.output_text.delta","delta":"lo"}`,
		`data: {"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":5}}}`,
	)

	got, err := drainResponsesStream(t, script)

	eventsEqual(t, got, []provider.StreamEvent{
		{Type: provider.EventTextDelta, Text: "Hel"},
		{Type: provider.EventTextDelta, Text: "lo"},
		{Type: provider.EventUsage, Usage: &provider.Usage{InputTokens: 10, OutputTokens: 5}},
		{Type: provider.EventDone, StopReason: "stop"},
	})
	if !errors.Is(err, io.EOF) {
		t.Fatalf("drain error = %v, want io.EOF", err)
	}
}

func TestResponsesStream_Recv_FunctionCallArgumentsArriveWholeInDone(t *testing.T) {
	script := sseScript(
		`data: {"type":"response.output_item.added","item":{"type":"function_call","name":"get_weather","call_id":"call_1","arguments":""}}`,
		`data: {"type":"response.output_item.done","item":{"type":"function_call","name":"get_weather","call_id":"call_1","arguments":"{\"location\":\"NYC\"}"}}`,
		`data: {"type":"response.completed","response":{}}`,
	)

	got, err := drainResponsesStream(t, script)

	eventsEqual(t, got, []provider.StreamEvent{
		{Type: provider.EventToolCallStart, ToolCallID: "call_1", ToolName: "get_weather"},
		{Type: provider.EventToolArgsDelta, ToolCallID: "call_1", ArgsDelta: `{"location":"NYC"}`},
		{Type: provider.EventToolCallEnd, ToolCallID: "call_1"},
		{Type: provider.EventDone, StopReason: "tool_calls"},
	})
	if !errors.Is(err, io.EOF) {
		t.Fatalf("drain error = %v, want io.EOF", err)
	}
}

func TestResponsesStream_Recv_MessageOutputItemsAreSkipped(t *testing.T) {
	script := sseScript(
		`data: {"type":"response.output_item.added","item":{"type":"message"}}`,
		`data: {"type":"response.output_text.delta","delta":"hi"}`,
		`data: {"type":"response.output_item.done","item":{"type":"message"}}`,
		`data: {"type":"response.completed","response":{}}`,
	)

	got, err := drainResponsesStream(t, script)

	eventsEqual(t, got, []provider.StreamEvent{
		{Type: provider.EventTextDelta, Text: "hi"},
		{Type: provider.EventDone, StopReason: "stop"},
	})
	if !errors.Is(err, io.EOF) {
		t.Fatalf("drain error = %v, want io.EOF", err)
	}
}

func TestResponsesStream_Recv_UnknownEventTypeIsSkipped(t *testing.T) {
	script := sseScript(
		`data: {"type":"response.some_unknown_event"}`,
		`data: {"type":"response.output_text.delta","delta":"hi"}`,
		`data: {"type":"response.completed","response":{}}`,
	)

	got, err := drainResponsesStream(t, script)

	eventsEqual(t, got, []provider.StreamEvent{
		{Type: provider.EventTextDelta, Text: "hi"},
		{Type: provider.EventDone, StopReason: "stop"},
	})
	if !errors.Is(err, io.EOF) {
		t.Fatalf("drain error = %v, want io.EOF", err)
	}
}

func TestResponsesStream_Recv_ResponseFailedReturnsResponseError(t *testing.T) {
	script := sseScript(
		`data: {"type":"response.failed","response":{"error":{"code":"server_error","message":"boom"}}}`,
	)

	_, err := drainResponsesStream(t, script)

	var respErr *ResponseError
	if !errors.As(err, &respErr) {
		t.Fatalf("drain error = %v (%T), want *ResponseError via errors.As", err, err)
	}
	if respErr.Code != "server_error" || respErr.Message != "boom" {
		t.Fatalf("ResponseError = %+v, want Code=%q Message=%q", respErr, "server_error", "boom")
	}
}

func TestResponsesStream_Recv_ResponseIncompleteReturnsTypedError(t *testing.T) {
	script := sseScript(
		`data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}`,
	)

	_, err := drainResponsesStream(t, script)

	var incompleteErr *IncompleteResponseError
	if !errors.As(err, &incompleteErr) {
		t.Fatalf("drain error = %v (%T), want *IncompleteResponseError via errors.As", err, err)
	}
	if incompleteErr.Reason != "max_output_tokens" {
		t.Fatalf("IncompleteResponseError.Reason = %q, want %q", incompleteErr.Reason, "max_output_tokens")
	}
	if !strings.Contains(incompleteErr.Error(), "max_output_tokens") {
		t.Fatalf("Error() = %q, want it to contain the reason", incompleteErr.Error())
	}
}

func TestResponsesStream_Recv_PrematureCloseWrapsUnexpectedEOF(t *testing.T) {
	script := `data: {"type":"response.output_text.delta","delta":"hi"}` + "\n\n"

	_, err := drainResponsesStream(t, script)

	if !errors.Is(err, io.ErrUnexpectedEOF) {
		t.Fatalf("drain error = %v, want wrapping io.ErrUnexpectedEOF", err)
	}
}

func TestResponsesStream_Close_Idempotent(t *testing.T) {
	body := io.NopCloser(strings.NewReader(sseScript(`data: {"type":"response.completed","response":{}}`)))
	s := newResponsesStream(body)

	if err := s.Close(); err != nil {
		t.Fatalf("first Close() error = %v, want nil", err)
	}
	if err := s.Close(); err != nil {
		t.Fatalf("second Close() error = %v, want nil", err)
	}
}

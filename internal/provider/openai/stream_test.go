package openai

import (
	"bufio"
	"errors"
	"io"
	"strings"
	"testing"

	"github.com/0xErwin1/agens/internal/provider"
)

func newFramer(script string) *sseFramer {
	return &sseFramer{r: bufio.NewReader(strings.NewReader(script))}
}

func TestSSEFramer_Next(t *testing.T) {
	tests := []struct {
		name        string
		script      string
		wantPayload []string
		wantDone    bool
		wantErr     error
	}{
		{
			name:        "single data line",
			script:      "data: {\"a\":1}\n\ndata: [DONE]\n\n",
			wantPayload: []string{`{"a":1}`},
			wantDone:    true,
		},
		{
			name:        "blank lines are skipped",
			script:      "\n\ndata: {\"a\":1}\n\n\ndata: [DONE]\n\n",
			wantPayload: []string{`{"a":1}`},
			wantDone:    true,
		},
		{
			name:        "comment lines are skipped",
			script:      ": keep-alive\ndata: {\"a\":1}\n\ndata: [DONE]\n\n",
			wantPayload: []string{`{"a":1}`},
			wantDone:    true,
		},
		{
			name:        "done marker",
			script:      "data: [DONE]\n\n",
			wantPayload: []string{},
			wantDone:    true,
		},
		{
			name:        "multiple data lines in order",
			script:      "data: {\"a\":1}\n\ndata: {\"a\":2}\n\ndata: [DONE]\n\n",
			wantPayload: []string{`{"a":1}`, `{"a":2}`},
			wantDone:    true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			f := newFramer(tt.script)

			var gotPayloads []string
			var gotDone bool
			var gotErr error

			for {
				payload, done, err := f.next()
				if err != nil {
					gotErr = err
					break
				}
				if done {
					gotDone = true
					break
				}
				gotPayloads = append(gotPayloads, payload)
			}

			if gotErr != nil {
				t.Fatalf("next() returned unexpected error: %v", gotErr)
			}
			if gotDone != tt.wantDone {
				t.Fatalf("done = %v, want %v", gotDone, tt.wantDone)
			}
			if len(gotPayloads) != len(tt.wantPayload) {
				t.Fatalf("payloads = %v, want %v", gotPayloads, tt.wantPayload)
			}
			for i, p := range gotPayloads {
				if p != tt.wantPayload[i] {
					t.Fatalf("payload[%d] = %q, want %q", i, p, tt.wantPayload[i])
				}
			}
		})
	}
}

func TestSSEFramer_Next_LongLineNotTruncated(t *testing.T) {
	huge := strings.Repeat("x", 70*1024)
	script := "data: " + huge + "\n\n"
	f := newFramer(script)

	payload, done, err := f.next()
	if err != nil {
		t.Fatalf("next() returned unexpected error: %v", err)
	}
	if done {
		t.Fatalf("done = true, want false")
	}
	if payload != huge {
		t.Fatalf("payload length = %d, want %d", len(payload), len(huge))
	}
}

func TestSSEFramer_Next_PrematureEOF(t *testing.T) {
	script := "data: {\"a\":1}\n\n"
	f := newFramer(script)

	if _, done, err := f.next(); err != nil || done {
		t.Fatalf("first next() = done=%v err=%v, want a payload", done, err)
	}

	_, _, err := f.next()
	if err == nil {
		t.Fatalf("next() after premature EOF returned nil error")
	}
	if !errors.Is(err, io.ErrUnexpectedEOF) {
		t.Fatalf("next() error = %v, want wrapping io.ErrUnexpectedEOF", err)
	}
}

// drain reads every event from a chatCompletionsStream built from script
// until Recv returns an error (io.EOF included), and returns the events
// collected before that error alongside the error itself.
func drain(t *testing.T, script string) ([]provider.StreamEvent, error) {
	t.Helper()

	s := newStream(io.NopCloser(strings.NewReader(script)))
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

func TestChatCompletionsStream_Recv(t *testing.T) {
	tests := []struct {
		name       string
		script     string
		wantEvents []provider.StreamEvent
		wantErr    error
	}{
		{
			name:   "role-only chunk emits no event",
			script: `data: {"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}` + "\n\n" + `data: {"choices":[{"delta":{},"finish_reason":"stop"}]}` + "\n\n" + "data: [DONE]\n\n",
			wantEvents: []provider.StreamEvent{
				{Type: provider.EventDone, StopReason: "stop"},
			},
			wantErr: io.EOF,
		},
		{
			name:   "single chunk text delta",
			script: `data: {"choices":[{"delta":{"content":"Hola"},"finish_reason":null}]}` + "\n\n" + `data: {"choices":[{"delta":{},"finish_reason":"stop"}]}` + "\n\n" + "data: [DONE]\n\n",
			wantEvents: []provider.StreamEvent{
				{Type: provider.EventTextDelta, Text: "Hola"},
				{Type: provider.EventDone, StopReason: "stop"},
			},
			wantErr: io.EOF,
		},
		{
			name:   "multi chunk text delta",
			script: `data: {"choices":[{"delta":{"content":"He"},"finish_reason":null}]}` + "\n\n" + `data: {"choices":[{"delta":{"content":"llo"},"finish_reason":null}]}` + "\n\n" + `data: {"choices":[{"delta":{},"finish_reason":"stop"}]}` + "\n\n" + "data: [DONE]\n\n",
			wantEvents: []provider.StreamEvent{
				{Type: provider.EventTextDelta, Text: "He"},
				{Type: provider.EventTextDelta, Text: "llo"},
				{Type: provider.EventDone, StopReason: "stop"},
			},
			wantErr: io.EOF,
		},
		{
			name: "tool call across three or more chunks",
			script: strings.Join([]string{
				`data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}`,
				`data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"loc"}}]},"finish_reason":null}]}`,
				`data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ation\":\"NYC\"}"}}]},"finish_reason":null}]}`,
				`data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}`,
				"data: [DONE]",
			}, "\n\n") + "\n\n",
			wantEvents: []provider.StreamEvent{
				{Type: provider.EventToolCallStart, ToolCallID: "call_1", ToolName: "get_weather"},
				{Type: provider.EventToolArgsDelta, ToolCallID: "call_1", ArgsDelta: `{"loc`},
				{Type: provider.EventToolArgsDelta, ToolCallID: "call_1", ArgsDelta: `ation":"NYC"}`},
				{Type: provider.EventToolCallEnd, ToolCallID: "call_1"},
				{Type: provider.EventDone, StopReason: "tool_calls"},
			},
			wantErr: io.EOF,
		},
		{
			name: "two parallel tool calls interleaved by index",
			script: strings.Join([]string{
				`data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_0","function":{"name":"a","arguments":""}}]},"finish_reason":null}]}`,
				`data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_1","function":{"name":"b","arguments":""}}]},"finish_reason":null}]}`,
				`data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"X"}}]},"finish_reason":null}]}`,
				`data: {"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"Y"}}]},"finish_reason":null}]}`,
				`data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}`,
				"data: [DONE]",
			}, "\n\n") + "\n\n",
			wantEvents: []provider.StreamEvent{
				{Type: provider.EventToolCallStart, ToolCallID: "call_0", ToolName: "a"},
				{Type: provider.EventToolCallStart, ToolCallID: "call_1", ToolName: "b"},
				{Type: provider.EventToolArgsDelta, ToolCallID: "call_0", ArgsDelta: "X"},
				{Type: provider.EventToolArgsDelta, ToolCallID: "call_1", ArgsDelta: "Y"},
				{Type: provider.EventToolCallEnd, ToolCallID: "call_0"},
				{Type: provider.EventToolCallEnd, ToolCallID: "call_1"},
				{Type: provider.EventDone, StopReason: "tool_calls"},
			},
			wantErr: io.EOF,
		},
		{
			name: "usage chunk after finish reason is still delivered",
			script: strings.Join([]string{
				`data: {"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}`,
				`data: {"choices":[{"delta":{},"finish_reason":"stop"}]}`,
				`data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}`,
				"data: [DONE]",
			}, "\n\n") + "\n\n",
			wantEvents: []provider.StreamEvent{
				{Type: provider.EventTextDelta, Text: "hi"},
				{Type: provider.EventDone, StopReason: "stop"},
				{Type: provider.EventUsage, Usage: &provider.Usage{InputTokens: 10, OutputTokens: 5}},
			},
			wantErr: io.EOF,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			gotEvents, gotErr := drain(t, tt.script)
			eventsEqual(t, gotEvents, tt.wantEvents)
			if !errors.Is(gotErr, tt.wantErr) {
				t.Fatalf("drain error = %v, want %v", gotErr, tt.wantErr)
			}
		})
	}
}

func TestChatCompletionsStream_Recv_MalformedJSON(t *testing.T) {
	script := "data: {not valid json\n\ndata: [DONE]\n\n"

	_, err := drain(t, script)
	if err == nil {
		t.Fatalf("drain error = nil, want a non-EOF error")
	}
	if errors.Is(err, io.EOF) {
		t.Fatalf("drain error = %v, want a non-EOF error", err)
	}
}

func TestChatCompletionsStream_Recv_DoneWithoutFinishReason(t *testing.T) {
	script := `data: {"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}` + "\n\n" + "data: [DONE]\n\n"

	_, err := drain(t, script)
	if err == nil {
		t.Fatalf("drain error = nil, want a non-EOF error")
	}
	if errors.Is(err, io.EOF) {
		t.Fatalf("drain error = %v, want a non-EOF error", err)
	}
}

func TestChatCompletionsStream_Recv_PrematureClose(t *testing.T) {
	script := `data: {"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}` + "\n\n"

	_, err := drain(t, script)
	if err == nil {
		t.Fatalf("drain error = nil, want a non-EOF error")
	}
	if errors.Is(err, io.EOF) {
		t.Fatalf("drain error = %v, want a non-EOF error", err)
	}
}

func TestChatCompletionsStream_Recv_StickyError(t *testing.T) {
	script := "data: {not valid json\n\ndata: [DONE]\n\n"
	s := newStream(io.NopCloser(strings.NewReader(script)))

	_, first := s.Recv()
	if first == nil {
		t.Fatalf("first Recv() error = nil, want a non-EOF error")
	}

	_, second := s.Recv()
	if !errors.Is(second, first) {
		t.Fatalf("second Recv() error = %v, want the same sticky error %v", second, first)
	}
}

func TestChatCompletionsStream_Close_Idempotent(t *testing.T) {
	body := io.NopCloser(strings.NewReader("data: [DONE]\n\n"))
	s := newStream(body)

	if err := s.Close(); err != nil {
		t.Fatalf("first Close() error = %v, want nil", err)
	}
	if err := s.Close(); err != nil {
		t.Fatalf("second Close() error = %v, want nil", err)
	}
}

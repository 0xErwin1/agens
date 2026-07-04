package chatgpt

import (
	"io"
	"strings"
	"testing"

	"github.com/iperez/agens/internal/provider"
)

func TestEncodeRequest_Reasoning(t *testing.T) {
	withEffort, err := encodeRequest(provider.ChatRequest{Model: "gpt-5.5", Effort: "high"})
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}
	if withEffort.Reasoning == nil {
		t.Fatal("wire.Reasoning = nil, want a reasoning block")
	}
	if withEffort.Reasoning.Summary != "auto" {
		t.Fatalf("Reasoning.Summary = %q, want %q so the thinking streams", withEffort.Reasoning.Summary, "auto")
	}
	if withEffort.Reasoning.Effort != "high" {
		t.Fatalf("Reasoning.Effort = %q, want %q", withEffort.Reasoning.Effort, "high")
	}

	noEffort, err := encodeRequest(provider.ChatRequest{Model: "gpt-5.5"})
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}
	if noEffort.Reasoning.Effort != "" {
		t.Fatalf("Reasoning.Effort = %q, want empty when no effort is set", noEffort.Reasoning.Effort)
	}
}

func TestResponsesStream_ReasoningDeltaBecomesReasoningEvent(t *testing.T) {
	script := sseScript(
		`data: {"type":"response.reasoning_summary_text.delta","delta":"thinking about it"}`,
		`data: {"type":"response.output_text.delta","delta":"the answer"}`,
		`data: {"type":"response.completed","response":{}}`,
	)
	s := newResponsesStream(io.NopCloser(strings.NewReader(script)))

	ev, err := s.Recv()
	if err != nil {
		t.Fatalf("Recv() error = %v", err)
	}
	if ev.Type != provider.EventReasoningDelta || ev.Text != "thinking about it" {
		t.Fatalf("first event = %v/%q, want EventReasoningDelta/%q", ev.Type, ev.Text, "thinking about it")
	}

	ev, err = s.Recv()
	if err != nil {
		t.Fatalf("Recv() error = %v", err)
	}
	if ev.Type != provider.EventTextDelta || ev.Text != "the answer" {
		t.Fatalf("second event = %v/%q, want EventTextDelta/%q", ev.Type, ev.Text, "the answer")
	}
}

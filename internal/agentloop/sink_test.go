package agentloop

import (
	"context"
	"testing"
)

func TestEventSink_RoundTrip(t *testing.T) {
	if got := EventSink(context.Background()); got != nil {
		t.Fatal("EventSink(bare ctx) != nil, want nil when no sink is installed")
	}

	var seen []LoopEvent
	ctx := WithEventSink(context.Background(), func(ev LoopEvent) { seen = append(seen, ev) })

	emit := EventSink(ctx)
	if emit == nil {
		t.Fatal("EventSink() = nil after WithEventSink, want the installed sink")
	}

	emit(LoopEvent{Kind: LoopSubagentStarted, Subagent: Subagent{ID: "s1"}})
	if len(seen) != 1 || seen[0].Subagent.ID != "s1" {
		t.Fatalf("sink received %+v, want the emitted subagent event", seen)
	}
}

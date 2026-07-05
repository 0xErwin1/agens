package agentloop

import "context"

// sinkKey is the private context key under which a Loop stores its event sink.
type sinkKey struct{}

// WithEventSink returns a context carrying emit so code reached through an opaque
// tool.Execute — a subagent's task tool — can emit LoopEvents into the same
// stream the Loop is driving, without threading a sink through the Tool
// interface. EventSink retrieves it.
func WithEventSink(ctx context.Context, emit func(LoopEvent)) context.Context {
	return context.WithValue(ctx, sinkKey{}, emit)
}

// EventSink returns the event sink installed by the running Loop, or nil when
// none is present (no Loop above, or the Loop was driven with a nil sink).
func EventSink(ctx context.Context) func(LoopEvent) {
	emit, _ := ctx.Value(sinkKey{}).(func(LoopEvent))
	return emit
}

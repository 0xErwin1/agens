package provider

// EventType discriminates which StreamEvent fields are meaningful.
type EventType int

const (
	EventTextDelta EventType = iota
	EventToolCallStart
	EventToolArgsDelta
	EventToolCallEnd
	EventUsage
	EventDone
)

// String returns the snake_case wire name for t.
func (t EventType) String() string {
	switch t {
	case EventTextDelta:
		return "text_delta"
	case EventToolCallStart:
		return "tool_call_start"
	case EventToolArgsDelta:
		return "tool_args_delta"
	case EventToolCallEnd:
		return "tool_call_end"
	case EventUsage:
		return "usage"
	case EventDone:
		return "done"
	default:
		return "unknown"
	}
}

// StreamEvent is a single incremental delta from a provider stream. This
// package performs NO assembly: accumulating events into finalized
// message.Part values (tracking in-flight tool calls by ToolCallID,
// concatenating ArgsDelta chunks, parsing the final JSON) is owned by the
// agent loop (AGN-8).
//
// Field validity by Type:
//
//	EventTextDelta:     Text
//	EventToolCallStart: ToolCallID, ToolName
//	EventToolArgsDelta: ToolCallID, ArgsDelta
//	EventToolCallEnd:   ToolCallID (ToolName may be echoed, not guaranteed)
//	EventUsage:         Usage
//	EventDone:          StopReason
//
// All other fields hold their zero value.
type StreamEvent struct {
	Type       EventType
	Text       string
	ToolCallID string
	ToolName   string
	ArgsDelta  string
	Usage      *Usage
	StopReason string
}

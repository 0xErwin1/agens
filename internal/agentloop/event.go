// Package agentloop drives one synchronous agent turn loop: it streams a
// provider response, assembles it into a finalized message.Message, and
// dispatches any requested tool calls, repeating until the model stops
// requesting tools or a limit is reached.
//
// This package depends only on internal/message, internal/provider, and the
// standard library.
package agentloop

import (
	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// LoopEventKind discriminates which LoopEvent fields are meaningful.
type LoopEventKind int

const (
	LoopIterationStart LoopEventKind = iota
	LoopTextDelta
	LoopReasoningDelta
	LoopToolCallStarted
	LoopToolResult
	LoopToolBatchStarted
	LoopToolBatchFinished
	LoopUsage
	LoopMessageDone
)

// ToolBatch reports aggregate progress for a same-turn group of tool calls.
// Completed counts materialized child results. Failed counts child error
// results, and is also non-zero when the batch aborts before producing a
// normal result message.
type ToolBatch struct {
	ID        string
	Total     int
	Completed int
	Failed    int
}

// LoopEvent is a single incremental notification emitted while a Loop runs.
// It mirrors provider.StreamEvent's flat-struct-plus-enum shape so the enum
// can grow additively without breaking existing switches.
//
// Field validity by Kind:
//
//	LoopIterationStart:  Iteration
//	LoopTextDelta:       Iteration, Text
//	LoopReasoningDelta:  Iteration, Text (the model's streamed reasoning summary)
//	LoopToolCallStarted: Iteration, ToolCall (ID+Name only; Input not yet known)
//	LoopToolResult:      Iteration, ToolResult
//	LoopToolBatchStarted: Iteration, ToolBatch
//	LoopToolBatchFinished: Iteration, ToolBatch
//	LoopUsage:           Iteration, Usage
//	LoopMessageDone:     Iteration, Message
//
// All other fields hold their zero value.
type LoopEvent struct {
	Kind       LoopEventKind
	Iteration  int
	Text       string
	ToolCall   message.ToolUsePart
	ToolResult message.ToolResultPart
	ToolBatch  ToolBatch
	Usage      *provider.Usage
	Message    *message.Message
}

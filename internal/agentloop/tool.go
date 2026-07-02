package agentloop

import (
	"context"
	"fmt"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// ToolRunner is the seam through which a Loop discovers the tools available
// to the model and dispatches the tool calls the model requests.
type ToolRunner interface {
	Specs() []provider.ToolSpec
	Run(ctx context.Context, call message.ToolUsePart) (message.ToolResultPart, error)
}

// runTools dispatches calls sequentially against runner and returns a
// single RoleUser message.Message carrying one message.ToolResultPart per
// call, in the same order as calls.
//
// ctx.Err() is checked before every call; cancellation is the only path
// that aborts the dispatch early, and any results already produced are
// discarded. A non-ctx error returned by runner.Run is recorded as an
// IsError result instead of aborting, so the model can see the failure and
// react to it on its next turn.
func runTools(ctx context.Context, calls []message.ToolUsePart, runner ToolRunner, iteration int, emit func(LoopEvent)) (message.Message, error) {
	if runner == nil && len(calls) > 0 {
		return message.Message{}, fmt.Errorf("agentloop: model requested tool %q but no tools are available", calls[0].Name)
	}

	results := make(message.Parts, 0, len(calls))

	for _, call := range calls {
		if err := ctx.Err(); err != nil {
			return message.Message{}, err
		}

		result, err := runner.Run(ctx, call)
		if err != nil {
			if ctx.Err() != nil {
				return message.Message{}, ctx.Err()
			}
			result = message.ToolResultPart{
				ToolUseID: call.ID,
				IsError:   true,
				Content:   message.Parts{message.TextPart{Text: err.Error()}},
			}
		}
		result.ToolUseID = call.ID

		emit(LoopEvent{Kind: LoopToolResult, Iteration: iteration, ToolResult: result})
		results = append(results, result)
	}

	return message.NewMessage(message.RoleUser, results...), nil
}

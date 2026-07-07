package agentloop

import (
	"context"
	"fmt"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/provider"
)

// ToolRunner is the seam through which a Loop discovers the tools available
// to the model and dispatches the tool calls the model requests.
type ToolRunner interface {
	Specs() []provider.ToolSpec
	Run(ctx context.Context, call message.ToolUsePart) (message.ToolResultPart, error)
}

// batchRunner is an optional extension for runners that need to prepare a
// whole same-turn tool batch before any child executes. Implementations must
// return one result per call in call order and must set each result's
// ToolUseID to the corresponding call ID. onResult, when non-nil, is called
// exactly once per materialized result in the same order.
type batchRunner interface {
	RunBatch(ctx context.Context, calls []message.ToolUsePart, onResult func(message.ToolResultPart)) ([]message.ToolResultPart, error)
}

// runTools dispatches calls and returns a single RoleUser message.Message
// carrying one message.ToolResultPart per call, in the same order as calls.
// Runners may opt into whole-batch preflight through batchRunner; execution
// still remains ordered and non-concurrent in this package.
//
// ctx.Err() is checked before dispatch. Cancellation is the only path that
// aborts the dispatch early, and any results already produced are discarded.
// A non-ctx error returned by runner.Run is recorded as an IsError result
// instead of aborting, so the model can see the failure and react to it on
// its next turn.
func runTools(ctx context.Context, calls []message.ToolUsePart, runner ToolRunner, iteration int, emit func(LoopEvent)) (message.Message, error) {
	if runner == nil && len(calls) > 0 {
		return message.Message{}, fmt.Errorf("agentloop: model requested tool %q but no tools are available", calls[0].Name)
	}

	batch := ToolBatch{ID: fmt.Sprintf("iteration-%d-tools", iteration), Total: len(calls)}
	isBatch := len(calls) > 1
	if isBatch {
		emit(LoopEvent{Kind: LoopToolBatchStarted, Iteration: iteration, ToolBatch: batch})
	}

	results, err := dispatchTools(ctx, calls, runner, iteration, emit, &batch)
	if isBatch {
		if err != nil && batch.Failed == 0 {
			batch.Failed = 1
		}
		emit(LoopEvent{Kind: LoopToolBatchFinished, Iteration: iteration, ToolBatch: batch})
	}
	if err != nil {
		return message.Message{}, err
	}

	return message.NewMessage(message.RoleUser, results...), nil
}

func dispatchTools(ctx context.Context, calls []message.ToolUsePart, runner ToolRunner, iteration int, emit func(LoopEvent), batch *ToolBatch) (message.Parts, error) {
	if br, ok := runner.(batchRunner); ok && len(calls) > 1 {
		return dispatchToolBatch(ctx, calls, br, iteration, emit, batch)
	}

	results := make(message.Parts, 0, len(calls))
	for _, call := range calls {
		if err := ctx.Err(); err != nil {
			return nil, err
		}

		result, err := runner.Run(ctx, call)
		if err != nil {
			if ctx.Err() != nil {
				return nil, ctx.Err()
			}
			result = toolErrorResult(call, err)
		}
		result.ToolUseID = call.ID
		emitToolResult(iteration, result, emit, batch)
		results = append(results, result)
	}
	return results, nil
}

func dispatchToolBatch(ctx context.Context, calls []message.ToolUsePart, runner batchRunner, iteration int, emit func(LoopEvent), batch *ToolBatch) (message.Parts, error) {
	results := make(message.Parts, 0, len(calls))
	emitted := 0
	onResult := func(result message.ToolResultPart) {
		emitted++
		emitToolResult(iteration, result, emit, batch)
	}

	batchResults, err := runner.RunBatch(ctx, calls, onResult)
	if err != nil {
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}
		return nil, err
	}
	for _, result := range batchResults {
		results = append(results, result)
	}
	if emitted == 0 {
		for _, part := range results {
			result := part.(message.ToolResultPart)
			emitToolResult(iteration, result, emit, batch)
		}
	}
	return results, nil
}

func emitToolResult(iteration int, result message.ToolResultPart, emit func(LoopEvent), batch *ToolBatch) {
	if batch != nil && batch.Total > 1 {
		batch.Completed++
		if result.IsError {
			batch.Failed++
		}
	}
	emit(LoopEvent{Kind: LoopToolResult, Iteration: iteration, ToolResult: result})
}

func toolErrorResult(call message.ToolUsePart, err error) message.ToolResultPart {
	return message.ToolResultPart{
		ToolUseID: call.ID,
		IsError:   true,
		Content:   message.Parts{message.TextPart{Text: err.Error()}},
	}
}

package agentloop

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

var _ ToolRunner = (*fakeToolRunner)(nil)

// cancelingToolRunner is a ToolRunner that cancels its own context after the
// first call it receives, so tests can assert that runTools checks ctx.Err()
// before every call, not only before the first one.
type cancelingToolRunner struct {
	cancel context.CancelFunc
	calls  []message.ToolUsePart
}

func (r *cancelingToolRunner) Specs() []provider.ToolSpec { return nil }

func (r *cancelingToolRunner) Run(_ context.Context, call message.ToolUsePart) (message.ToolResultPart, error) {
	r.calls = append(r.calls, call)
	r.cancel()
	return message.ToolResultPart{}, nil
}

func toolResultParts(t *testing.T, parts message.Parts) []message.ToolResultPart {
	t.Helper()

	results := make([]message.ToolResultPart, len(parts))
	for i, p := range parts {
		r, ok := p.(message.ToolResultPart)
		if !ok {
			t.Fatalf("part[%d] = %T, want message.ToolResultPart", i, p)
		}
		results[i] = r
	}
	return results
}

type fakeBatchToolRunner struct {
	fakeToolRunner
	batchCalls [][]message.ToolUsePart
}

type failingBatchToolRunner struct {
	err error
}

type cancelingBatchToolRunner struct {
	cancel context.CancelFunc
}

func (r *failingBatchToolRunner) Specs() []provider.ToolSpec { return nil }

func (r *failingBatchToolRunner) Run(context.Context, message.ToolUsePart) (message.ToolResultPart, error) {
	return message.ToolResultPart{}, errors.New("Run should not be called for a multi-tool batch")
}

func (r *failingBatchToolRunner) RunBatch(context.Context, []message.ToolUsePart, func(message.ToolResultPart)) ([]message.ToolResultPart, error) {
	return nil, r.err
}

func (r *cancelingBatchToolRunner) Specs() []provider.ToolSpec { return nil }

func (r *cancelingBatchToolRunner) Run(context.Context, message.ToolUsePart) (message.ToolResultPart, error) {
	return message.ToolResultPart{}, errors.New("Run should not be called for a multi-tool batch")
}

func (r *cancelingBatchToolRunner) RunBatch(ctx context.Context, _ []message.ToolUsePart, _ func(message.ToolResultPart)) ([]message.ToolResultPart, error) {
	r.cancel()
	return nil, ctx.Err()
}

func (r *fakeBatchToolRunner) RunBatch(ctx context.Context, calls []message.ToolUsePart, onResult func(message.ToolResultPart)) ([]message.ToolResultPart, error) {
	r.batchCalls = append(r.batchCalls, append([]message.ToolUsePart(nil), calls...))
	results := make([]message.ToolResultPart, 0, len(calls))
	for _, call := range calls {
		result, err := r.Run(ctx, call)
		if err != nil {
			return nil, err
		}
		result.ToolUseID = call.ID
		results = append(results, result)
		onResult(result)
	}
	return results, nil
}

func TestRunTools_MultipleToolsUsesBatchRunnerAndEmitsBatchEvents(t *testing.T) {
	runner := &fakeBatchToolRunner{fakeToolRunner: fakeToolRunner{responses: map[string]message.ToolResultPart{
		"a": {Content: message.Parts{message.TextPart{Text: "a-result"}}},
		"b": {Content: message.Parts{message.TextPart{Text: "b-result"}}},
	}}}
	calls := []message.ToolUsePart{{ID: "call_a", Name: "a"}, {ID: "call_b", Name: "b"}}

	var events []LoopEvent
	msg, err := runTools(context.Background(), calls, runner, 4, func(e LoopEvent) { events = append(events, e) })
	if err != nil {
		t.Fatalf("runTools() error = %v, want nil", err)
	}

	if len(runner.batchCalls) != 1 || len(runner.batchCalls[0]) != 2 {
		t.Fatalf("RunBatch calls = %+v, want one batch with two calls", runner.batchCalls)
	}
	if len(runner.calls) != 2 {
		t.Fatalf("runner.calls = %+v, want RunBatch to execute both calls through the runner", runner.calls)
	}

	results := toolResultParts(t, msg.Parts)
	if len(results) != 2 || results[0].ToolUseID != "call_a" || results[1].ToolUseID != "call_b" {
		t.Fatalf("results = %+v, want call_a then call_b", results)
	}

	wantKinds := []LoopEventKind{LoopToolBatchStarted, LoopToolResult, LoopToolResult, LoopToolBatchFinished}
	if got := eventKinds(events); !equalKinds(got, wantKinds) {
		t.Fatalf("event kinds = %v, want %v", got, wantKinds)
	}
	if events[0].ToolBatch.Total != 2 {
		t.Fatalf("start ToolBatch = %+v, want Total=2", events[0].ToolBatch)
	}
	if events[len(events)-1].ToolBatch.Completed != 2 || events[len(events)-1].ToolBatch.Failed != 0 {
		t.Fatalf("finish ToolBatch = %+v, want Completed=2 Failed=0", events[len(events)-1].ToolBatch)
	}
}

func TestRunTools_BatchRunnerCancellationEmitsNonSuccessFinish(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	runner := &cancelingBatchToolRunner{cancel: cancel}
	calls := []message.ToolUsePart{{ID: "call_1", Name: "write"}, {ID: "call_2", Name: "bash"}}

	var events []LoopEvent
	_, err := runTools(ctx, calls, runner, 2, func(e LoopEvent) { events = append(events, e) })
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("runTools() error = %v, want context.Canceled", err)
	}

	finish := events[len(events)-1]
	if finish.Kind != LoopToolBatchFinished || finish.ToolBatch.Failed == 0 {
		t.Fatalf("finish event = %+v, want non-success batch terminal event on batch runner cancellation", finish)
	}
}

func TestRunTools_BatchRunnerErrorEmitsNonSuccessFinish(t *testing.T) {
	runner := &failingBatchToolRunner{err: errors.New("permission canceled")}
	calls := []message.ToolUsePart{{ID: "call_1", Name: "write"}, {ID: "call_2", Name: "bash"}}

	var events []LoopEvent
	_, err := runTools(context.Background(), calls, runner, 2, func(e LoopEvent) { events = append(events, e) })
	if err == nil {
		t.Fatal("runTools() error = nil, want batch runner error")
	}

	finish := events[len(events)-1]
	if finish.Kind != LoopToolBatchFinished || finish.ToolBatch.Failed == 0 {
		t.Fatalf("finish event = %+v, want non-success batch terminal event on batch runner error", finish)
	}
}

func TestRunTools_BatchFinishCountsFailures(t *testing.T) {
	runner := &fakeToolRunner{
		responses: map[string]message.ToolResultPart{"ok": {Content: message.Parts{message.TextPart{Text: "ok"}}}},
		errs:      map[string]error{"bad": errors.New("boom")},
	}
	calls := []message.ToolUsePart{{ID: "call_1", Name: "bad"}, {ID: "call_2", Name: "ok"}}

	var events []LoopEvent
	_, err := runTools(context.Background(), calls, runner, 2, func(e LoopEvent) { events = append(events, e) })
	if err != nil {
		t.Fatalf("runTools() error = %v, want nil", err)
	}

	finish := events[len(events)-1]
	if finish.Kind != LoopToolBatchFinished || finish.ToolBatch.Completed != 2 || finish.ToolBatch.Failed != 1 {
		t.Fatalf("finish event = %+v, want batch finished with Completed=2 Failed=1", finish)
	}
}

func TestRunTools_SingleToolRoundTrip(t *testing.T) {
	runner := &fakeToolRunner{
		responses: map[string]message.ToolResultPart{
			"get_weather": {Content: message.Parts{message.TextPart{Text: "sunny"}}},
		},
	}
	calls := []message.ToolUsePart{
		{ID: "call_1", Name: "get_weather", Input: json.RawMessage(`{}`)},
	}

	var events []LoopEvent
	msg, err := runTools(context.Background(), calls, runner, 3, func(e LoopEvent) { events = append(events, e) })
	if err != nil {
		t.Fatalf("runTools() error = %v, want nil", err)
	}

	if msg.Role != message.RoleUser {
		t.Fatalf("Role = %q, want %q", msg.Role, message.RoleUser)
	}

	results := toolResultParts(t, msg.Parts)
	if len(results) != 1 {
		t.Fatalf("results = %+v, want exactly 1", results)
	}
	if results[0].ToolUseID != "call_1" {
		t.Fatalf("ToolUseID = %q, want %q", results[0].ToolUseID, "call_1")
	}
	if results[0].IsError {
		t.Fatalf("IsError = true, want false")
	}

	if len(events) != 1 {
		t.Fatalf("events = %+v, want exactly 1", events)
	}
	if events[0].Kind != LoopToolResult {
		t.Fatalf("Kind = %v, want %v", events[0].Kind, LoopToolResult)
	}
	if events[0].Iteration != 3 {
		t.Fatalf("Iteration = %d, want 3", events[0].Iteration)
	}
	if events[0].ToolResult.ToolUseID != "call_1" {
		t.Fatalf("ToolResult.ToolUseID = %q, want %q", events[0].ToolResult.ToolUseID, "call_1")
	}
}

func TestRunTools_MultipleToolsOneMessage(t *testing.T) {
	runner := &fakeToolRunner{
		responses: map[string]message.ToolResultPart{
			"a": {Content: message.Parts{message.TextPart{Text: "a-result"}}},
			"b": {Content: message.Parts{message.TextPart{Text: "b-result"}}},
			"c": {Content: message.Parts{message.TextPart{Text: "c-result"}}},
		},
	}
	calls := []message.ToolUsePart{
		{ID: "call_a", Name: "a"},
		{ID: "call_b", Name: "b"},
		{ID: "call_c", Name: "c"},
	}

	var events []LoopEvent
	msg, err := runTools(context.Background(), calls, runner, 1, func(e LoopEvent) { events = append(events, e) })
	if err != nil {
		t.Fatalf("runTools() error = %v, want nil", err)
	}

	results := toolResultParts(t, msg.Parts)
	if len(results) != 3 {
		t.Fatalf("results = %+v, want exactly 3", results)
	}

	wantIDs := []string{"call_a", "call_b", "call_c"}
	for i, want := range wantIDs {
		if results[i].ToolUseID != want {
			t.Fatalf("results[%d].ToolUseID = %q, want %q", i, results[i].ToolUseID, want)
		}
	}

	wantKinds := []LoopEventKind{LoopToolBatchStarted, LoopToolResult, LoopToolResult, LoopToolResult, LoopToolBatchFinished}
	if got := eventKinds(events); !equalKinds(got, wantKinds) {
		t.Fatalf("event kinds = %v, want %v", got, wantKinds)
	}
	for i, want := range wantIDs {
		event := events[i+1]
		if event.ToolResult.ToolUseID != want {
			t.Fatalf("events[%d].ToolResult.ToolUseID = %q, want %q", i+1, event.ToolResult.ToolUseID, want)
		}
	}
}

func TestRunTools_ToolErrorBecomesIsErrorAndContinues(t *testing.T) {
	runner := &fakeToolRunner{
		responses: map[string]message.ToolResultPart{
			"ok_tool": {Content: message.Parts{message.TextPart{Text: "fine"}}},
		},
		errs: map[string]error{
			"broken_tool": errors.New("boom"),
		},
	}
	calls := []message.ToolUsePart{
		{ID: "call_1", Name: "broken_tool"},
		{ID: "call_2", Name: "ok_tool"},
	}

	msg, err := runTools(context.Background(), calls, runner, 1, func(LoopEvent) {})
	if err != nil {
		t.Fatalf("runTools() error = %v, want nil (non-ctx tool errors must not abort)", err)
	}

	results := toolResultParts(t, msg.Parts)
	if len(results) != 2 {
		t.Fatalf("results = %+v, want exactly 2", results)
	}

	if !results[0].IsError {
		t.Fatalf("results[0].IsError = false, want true")
	}
	if results[0].ToolUseID != "call_1" {
		t.Fatalf("results[0].ToolUseID = %q, want %q", results[0].ToolUseID, "call_1")
	}
	if len(results[0].Content) != 1 {
		t.Fatalf("results[0].Content = %+v, want exactly 1 part", results[0].Content)
	}
	text, ok := results[0].Content[0].(message.TextPart)
	if !ok || text.Text != "boom" {
		t.Fatalf("results[0].Content[0] = %+v, want TextPart{Text: %q}", results[0].Content[0], "boom")
	}

	if results[1].IsError {
		t.Fatalf("results[1].IsError = true, want false")
	}
	if results[1].ToolUseID != "call_2" {
		t.Fatalf("results[1].ToolUseID = %q, want %q", results[1].ToolUseID, "call_2")
	}

	if len(runner.calls) != 2 {
		t.Fatalf("runner.calls = %+v, want exactly 2 (dispatch must continue past a tool error)", runner.calls)
	}
}

func TestRunTools_ToolUseIDIsAlwaysForced(t *testing.T) {
	runner := &fakeToolRunner{
		responses: map[string]message.ToolResultPart{
			"mistagged": {ToolUseID: "wrong-id", Content: message.Parts{message.TextPart{Text: "ok"}}},
		},
	}
	calls := []message.ToolUsePart{{ID: "call_1", Name: "mistagged"}}

	msg, err := runTools(context.Background(), calls, runner, 1, func(LoopEvent) {})
	if err != nil {
		t.Fatalf("runTools() error = %v, want nil", err)
	}

	results := toolResultParts(t, msg.Parts)
	if results[0].ToolUseID != "call_1" {
		t.Fatalf("ToolUseID = %q, want %q (must be forced to the call's ID)", results[0].ToolUseID, "call_1")
	}
}

func TestRunTools_CtxCanceledBeforeFirstCall(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	runner := &fakeToolRunner{
		responses: map[string]message.ToolResultPart{"a": {}},
	}
	calls := []message.ToolUsePart{{ID: "call_1", Name: "a"}}

	msg, err := runTools(ctx, calls, runner, 1, func(LoopEvent) {})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("runTools() error = %v, want errors.Is(err, context.Canceled)", err)
	}
	if !msg.CreatedAt.IsZero() || msg.Role != "" {
		t.Fatalf("msg = %+v, want the zero Message on cancellation", msg)
	}
	if len(runner.calls) != 0 {
		t.Fatalf("runner.calls = %+v, want no calls made", runner.calls)
	}
}

func TestRunTools_CtxCanceledBetweenCalls(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	runner := &cancelingToolRunner{cancel: cancel}
	calls := []message.ToolUsePart{
		{ID: "call_1", Name: "a"},
		{ID: "call_2", Name: "b"},
	}

	var events []LoopEvent
	_, err := runTools(ctx, calls, runner, 1, func(e LoopEvent) { events = append(events, e) })
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("runTools() error = %v, want errors.Is(err, context.Canceled)", err)
	}
	if len(runner.calls) != 1 {
		t.Fatalf("runner.calls = %+v, want exactly 1 (ctx must be checked before every call)", runner.calls)
	}
	finish := events[len(events)-1]
	if finish.Kind != LoopToolBatchFinished || finish.ToolBatch.Failed == 0 {
		t.Fatalf("finish event = %+v, want non-success batch terminal event on cancellation", finish)
	}
}

func TestRunTools_NilRunnerWithCallsIsAnError(t *testing.T) {
	calls := []message.ToolUsePart{{ID: "call_1", Name: "get_weather"}}

	_, err := runTools(context.Background(), calls, nil, 1, func(LoopEvent) {})
	if err == nil {
		t.Fatalf("runTools() error = nil, want an error")
	}
	if !strings.Contains(err.Error(), "get_weather") {
		t.Fatalf("runTools() error = %v, want it to mention the requested tool name", err)
	}
}

func TestRunTools_NilRunnerWithNoCallsIsNotAnError(t *testing.T) {
	msg, err := runTools(context.Background(), nil, nil, 1, func(LoopEvent) {})
	if err != nil {
		t.Fatalf("runTools() error = %v, want nil", err)
	}
	if msg.Role != message.RoleUser {
		t.Fatalf("Role = %q, want %q", msg.Role, message.RoleUser)
	}
	if len(msg.Parts) != 0 {
		t.Fatalf("Parts = %+v, want empty", msg.Parts)
	}
}

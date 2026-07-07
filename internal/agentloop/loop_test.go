package agentloop

import (
	"context"
	"errors"
	"testing"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/provider"
)

// textOnlySteps scripts a stream that emits a single text delta followed by
// EventDone.
func textOnlySteps(text string) []streamStep {
	return []streamStep{
		{ev: provider.StreamEvent{Type: provider.EventTextDelta, Text: text}},
		{ev: provider.StreamEvent{Type: provider.EventDone, StopReason: "stop"}},
	}
}

// toolCallSteps scripts a stream that emits a start+end pair for each call,
// in order, followed by EventDone with StopReason "tool_calls".
func toolCallSteps(calls ...message.ToolUsePart) []streamStep {
	steps := make([]streamStep, 0, len(calls)*2+1)
	for _, c := range calls {
		steps = append(steps,
			streamStep{ev: provider.StreamEvent{Type: provider.EventToolCallStart, ToolCallID: c.ID, ToolName: c.Name}},
			streamStep{ev: provider.StreamEvent{Type: provider.EventToolCallEnd, ToolCallID: c.ID}},
		)
	}
	steps = append(steps, streamStep{ev: provider.StreamEvent{Type: provider.EventDone, StopReason: "tool_calls"}})
	return steps
}

// sequencingProvider is a provider.Provider that returns a different
// scripted provider.StreamReader on each successive Stream call, drawn from
// a fixed slice of scripts, and records every ChatRequest it receives.
type sequencingProvider struct {
	scripts [][]streamStep
	idx     int

	requests []provider.ChatRequest
}

var _ provider.Provider = (*sequencingProvider)(nil)

func (p *sequencingProvider) ID() string { return "sequencing-fake-provider" }

func (p *sequencingProvider) Models(context.Context) ([]provider.ModelInfo, error) {
	return nil, nil
}

func (p *sequencingProvider) EffortLevels() []string { return nil }

func (p *sequencingProvider) Stream(_ context.Context, req provider.ChatRequest) (provider.StreamReader, error) {
	p.requests = append(p.requests, req)

	if p.idx >= len(p.scripts) {
		return nil, errors.New("sequencingProvider: no more scripted streams")
	}
	steps := p.scripts[p.idx]
	p.idx++
	return newScriptedStream(steps), nil
}

// cancelingStreamProvider is a provider.Provider whose Stream returns a
// cancelingStream, so tests can exercise cancellation observed mid-stream.
type cancelingStreamProvider struct {
	cancel context.CancelFunc
}

var _ provider.Provider = (*cancelingStreamProvider)(nil)

func (p *cancelingStreamProvider) ID() string { return "canceling-stream-provider" }

func (p *cancelingStreamProvider) Models(context.Context) ([]provider.ModelInfo, error) {
	return nil, nil
}

func (p *cancelingStreamProvider) EffortLevels() []string { return nil }

func (p *cancelingStreamProvider) Stream(context.Context, provider.ChatRequest) (provider.StreamReader, error) {
	return &cancelingStream{cancel: p.cancel}, nil
}

// cancelingStream cancels its owning context on the first Recv call, then
// reports a non-EOF error, so the caller observes cancellation rather than a
// transport failure.
type cancelingStream struct {
	cancel context.CancelFunc
}

func (s *cancelingStream) Recv() (provider.StreamEvent, error) {
	s.cancel()
	return provider.StreamEvent{}, errors.New("agentloop test: stream aborted")
}

func (s *cancelingStream) Close() error { return nil }

// specsCountingToolRunner wraps a fakeToolRunner to count how many times
// Specs is called.
type specsCountingToolRunner struct {
	*fakeToolRunner
	specsCalls int
}

var _ ToolRunner = (*specsCountingToolRunner)(nil)

func (r *specsCountingToolRunner) Specs() []provider.ToolSpec {
	r.specsCalls++
	return r.fakeToolRunner.Specs()
}

func TestNew_PanicsOnNilProvider(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatalf("New(nil, ...) did not panic")
		}
	}()
	New(nil, nil)
}

func TestWithMaxIterations_PanicsOnLessThanOne(t *testing.T) {
	for _, n := range []int{0, -1} {
		func() {
			defer func() {
				if recover() == nil {
					t.Fatalf("WithMaxIterations(%d) did not panic", n)
				}
			}()
			WithMaxIterations(n)
		}()
	}
}

func TestRun_EmptyModelIsAnError(t *testing.T) {
	p := &fakeProvider{}
	loop := New(p, nil)

	history := []message.Message{message.NewMessage(message.RoleUser, message.TextPart{Text: "hi"})}
	got, err := loop.Run(context.Background(), history, nil)
	if err == nil {
		t.Fatalf("Run() error = nil, want an error for an empty model")
	}
	if p.lastRequest.Model != "" || len(p.lastRequest.Messages) != 0 {
		t.Fatalf("provider.Stream was called, want it untouched: %+v", p.lastRequest)
	}
	if len(got) != len(history) {
		t.Fatalf("history = %+v, want it unchanged", got)
	}
}

func TestRun_SingleTurnNoTools(t *testing.T) {
	p := &fakeProvider{steps: textOnlySteps("hello")}
	loop := New(p, nil, WithModel("gpt-test"))

	original := []message.Message{message.NewMessage(message.RoleUser, message.TextPart{Text: "hi"})}
	got, err := loop.Run(context.Background(), original, nil)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if len(got) != 2 {
		t.Fatalf("history = %+v, want original + 1 assistant message", got)
	}
	if got[1].Role != message.RoleAssistant {
		t.Fatalf("got[1].Role = %q, want %q", got[1].Role, message.RoleAssistant)
	}
	text, _ := splitParts(t, got[1].Parts)
	if text != "hello" {
		t.Fatalf("assistant text = %q, want %q", text, "hello")
	}
}

func TestRun_MultiIterationUntilNoTools(t *testing.T) {
	call := message.ToolUsePart{ID: "call_1", Name: "get_weather"}
	p := &sequencingProvider{scripts: [][]streamStep{
		toolCallSteps(call),
		textOnlySteps("done"),
	}}
	tools := &fakeToolRunner{responses: map[string]message.ToolResultPart{
		"get_weather": {Content: message.Parts{message.TextPart{Text: "sunny"}}},
	}}
	loop := New(p, tools, WithModel("gpt-test"))

	original := []message.Message{message.NewMessage(message.RoleUser, message.TextPart{Text: "weather?"})}
	got, err := loop.Run(context.Background(), original, nil)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if len(got) != 4 {
		t.Fatalf("history = %+v, want 4 messages (original, assistant1, tool results, assistant2)", got)
	}
	if got[1].Role != message.RoleAssistant {
		t.Fatalf("got[1].Role = %q, want assistant", got[1].Role)
	}
	if got[2].Role != message.RoleUser {
		t.Fatalf("got[2].Role = %q, want user (tool results)", got[2].Role)
	}
	if got[3].Role != message.RoleAssistant {
		t.Fatalf("got[3].Role = %q, want assistant", got[3].Role)
	}
	_, toolParts := splitParts(t, got[1].Parts)
	if len(toolParts) != 1 || toolParts[0].Name != "get_weather" {
		t.Fatalf("assistant1 tool parts = %+v, want exactly one get_weather call", toolParts)
	}
	finalText, _ := splitParts(t, got[3].Parts)
	if finalText != "done" {
		t.Fatalf("assistant2 text = %q, want %q", finalText, "done")
	}
}

func TestRun_ErrMaxIterationsWithPartialHistory(t *testing.T) {
	call := message.ToolUsePart{ID: "call_1", Name: "loop_tool"}
	p := &fakeProvider{steps: toolCallSteps(call)}
	tools := &fakeToolRunner{responses: map[string]message.ToolResultPart{
		"loop_tool": {Content: message.Parts{message.TextPart{Text: "again"}}},
	}}
	loop := New(p, tools, WithModel("gpt-test"), WithMaxIterations(2))

	got, err := loop.Run(context.Background(), nil, nil)
	if !errors.Is(err, ErrMaxIterations) {
		t.Fatalf("Run() error = %v, want ErrMaxIterations", err)
	}
	if len(got) != 4 {
		t.Fatalf("history = %+v, want 4 messages (2 assistants + 2 tool-result messages)", got)
	}
}

func TestRun_DefaultMaxIterationsIsSixty(t *testing.T) {
	call := message.ToolUsePart{ID: "call_1", Name: "loop_tool"}
	p := &fakeProvider{steps: toolCallSteps(call)}
	tools := &fakeToolRunner{responses: map[string]message.ToolResultPart{
		"loop_tool": {Content: message.Parts{message.TextPart{Text: "again"}}},
	}}
	loop := New(p, tools, WithModel("gpt-test"))

	got, err := loop.Run(context.Background(), nil, nil)
	if !errors.Is(err, ErrMaxIterations) {
		t.Fatalf("Run() error = %v, want ErrMaxIterations", err)
	}
	if len(got) != 120 {
		t.Fatalf("history length = %d, want 120 (default max iterations = 60, 2 messages per iteration)", len(got))
	}
}

func TestRun_CancelMidStream(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	p := &cancelingStreamProvider{cancel: cancel}
	loop := New(p, nil, WithModel("gpt-test"))

	got, err := loop.Run(ctx, nil, nil)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Run() error = %v, want errors.Is(err, context.Canceled)", err)
	}
	if len(got) != 0 {
		t.Fatalf("history = %+v, want empty (no partial assistant message)", got)
	}
}

func TestRun_CancelMidTool(t *testing.T) {
	calls := []message.ToolUsePart{
		{ID: "call_1", Name: "a"},
		{ID: "call_2", Name: "b"},
	}
	p := &fakeProvider{steps: toolCallSteps(calls...)}
	ctx, cancel := context.WithCancel(context.Background())
	tools := &cancelingToolRunner{cancel: cancel}
	loop := New(p, tools, WithModel("gpt-test"))

	got, err := loop.Run(ctx, nil, nil)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Run() error = %v, want errors.Is(err, context.Canceled)", err)
	}
	if len(got) != 1 {
		t.Fatalf("history = %+v, want 1 message (the assistant with tool calls, no tool results)", got)
	}
	if got[0].Role != message.RoleAssistant {
		t.Fatalf("got[0].Role = %q, want assistant", got[0].Role)
	}
	if len(tools.calls) != 1 {
		t.Fatalf("tools.calls = %+v, want exactly 1 (ctx must be checked before the second call)", tools.calls)
	}
}

func TestRun_SystemPromptPrependedButNotInHistory(t *testing.T) {
	call := message.ToolUsePart{ID: "call_1", Name: "get_weather"}
	p := &sequencingProvider{scripts: [][]streamStep{
		toolCallSteps(call),
		textOnlySteps("done"),
	}}
	tools := &fakeToolRunner{responses: map[string]message.ToolResultPart{
		"get_weather": {Content: message.Parts{message.TextPart{Text: "sunny"}}},
	}}
	loop := New(p, tools, WithModel("gpt-test"), WithSystemPrompt("be nice"))

	original := []message.Message{message.NewMessage(message.RoleUser, message.TextPart{Text: "weather?"})}
	got, err := loop.Run(context.Background(), original, nil)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}

	for i, msg := range got {
		if msg.Role == message.RoleSystem {
			t.Fatalf("returned history[%d] has RoleSystem, want it never present in the returned history", i)
		}
	}

	if len(p.requests) != 2 {
		t.Fatalf("requests = %d, want exactly 2", len(p.requests))
	}
	var systemMessages []message.Message
	for i, req := range p.requests {
		if len(req.Messages) == 0 || req.Messages[0].Role != message.RoleSystem {
			t.Fatalf("request[%d].Messages[0] = %+v, want RoleSystem prepended to every request", i, req.Messages)
		}
		systemMessages = append(systemMessages, req.Messages[0])
	}
	if systemMessages[0].ID != systemMessages[1].ID {
		t.Fatalf("system message built more than once: %+v vs %+v, want the same message.Message built once in New", systemMessages[0], systemMessages[1])
	}
}

func TestRun_NoSystemPromptMeansNoSystemMessage(t *testing.T) {
	p := &fakeProvider{steps: textOnlySteps("hi")}
	loop := New(p, nil, WithModel("gpt-test"))

	if _, err := loop.Run(context.Background(), nil, nil); err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if len(p.lastRequest.Messages) != 0 {
		t.Fatalf("request.Messages = %+v, want no RoleSystem message prepended", p.lastRequest.Messages)
	}
}

func TestRun_NilToolsMeansNilSpecsInRequest(t *testing.T) {
	p := &fakeProvider{steps: textOnlySteps("hi")}
	loop := New(p, nil, WithModel("gpt-test"))

	if _, err := loop.Run(context.Background(), nil, nil); err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if p.lastRequest.Tools != nil {
		t.Fatalf("request.Tools = %+v, want nil when no ToolRunner is configured", p.lastRequest.Tools)
	}
}

func TestRun_ToolCallAgainstNilRunnerIsAnError(t *testing.T) {
	call := message.ToolUsePart{ID: "call_1", Name: "get_weather"}
	p := &fakeProvider{steps: toolCallSteps(call)}
	loop := New(p, nil, WithModel("gpt-test"))

	got, err := loop.Run(context.Background(), nil, nil)
	if err == nil {
		t.Fatalf("Run() error = nil, want an error (model requested a tool but no ToolRunner is configured)")
	}
	if len(got) != 1 {
		t.Fatalf("history = %+v, want 1 message (the assistant message with the tool call; dispatch never happened)", got)
	}
}

func TestRun_SinkNilProducesIdenticalResultToNonNilSink(t *testing.T) {
	call := message.ToolUsePart{ID: "call_1", Name: "get_weather"}
	newFixture := func() (*sequencingProvider, *fakeToolRunner) {
		p := &sequencingProvider{scripts: [][]streamStep{toolCallSteps(call), textOnlySteps("done")}}
		tools := &fakeToolRunner{responses: map[string]message.ToolResultPart{
			"get_weather": {Content: message.Parts{message.TextPart{Text: "sunny"}}},
		}}
		return p, tools
	}
	original := []message.Message{message.NewMessage(message.RoleUser, message.TextPart{Text: "weather?"})}

	pNil, toolsNil := newFixture()
	loopNilSink := New(pNil, toolsNil, WithModel("gpt-test"))
	gotNilSink, errNilSink := loopNilSink.Run(context.Background(), original, nil)

	pSink, toolsSink := newFixture()
	loopWithSink := New(pSink, toolsSink, WithModel("gpt-test"))
	gotWithSink, errWithSink := loopWithSink.Run(context.Background(), original, func(LoopEvent) {})

	if (errNilSink == nil) != (errWithSink == nil) {
		t.Fatalf("errors differ: %v vs %v", errNilSink, errWithSink)
	}
	if len(gotNilSink) != len(gotWithSink) {
		t.Fatalf("history lengths differ: %d vs %d", len(gotNilSink), len(gotWithSink))
	}
	for i := range gotNilSink {
		if gotNilSink[i].Role != gotWithSink[i].Role {
			t.Fatalf("history[%d].Role differs: %q vs %q", i, gotNilSink[i].Role, gotWithSink[i].Role)
		}
		if len(gotNilSink[i].Parts) != len(gotWithSink[i].Parts) {
			t.Fatalf("history[%d].Parts differ: %+v vs %+v", i, gotNilSink[i].Parts, gotWithSink[i].Parts)
		}
	}
}

func TestRun_SinkReceivesExpectedEventKinds(t *testing.T) {
	usage := &provider.Usage{InputTokens: 3, OutputTokens: 7}
	call := message.ToolUsePart{ID: "call_1", Name: "get_weather"}
	firstTurn := []streamStep{
		{ev: provider.StreamEvent{Type: provider.EventTextDelta, Text: "checking "}},
		{ev: provider.StreamEvent{Type: provider.EventToolCallStart, ToolCallID: call.ID, ToolName: call.Name}},
		{ev: provider.StreamEvent{Type: provider.EventToolCallEnd, ToolCallID: call.ID}},
		{ev: provider.StreamEvent{Type: provider.EventUsage, Usage: usage}},
		{ev: provider.StreamEvent{Type: provider.EventDone, StopReason: "tool_calls"}},
	}
	p := &sequencingProvider{scripts: [][]streamStep{firstTurn, textOnlySteps("done")}}
	tools := &fakeToolRunner{responses: map[string]message.ToolResultPart{
		"get_weather": {Content: message.Parts{message.TextPart{Text: "sunny"}}},
	}}
	loop := New(p, tools, WithModel("gpt-test"))

	var events []LoopEvent
	if _, err := loop.Run(context.Background(), nil, func(e LoopEvent) { events = append(events, e) }); err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}

	want := []LoopEventKind{
		LoopIterationStart, LoopTextDelta, LoopToolCallStarted, LoopUsage, LoopMessageDone, LoopToolResult,
	}
	got := map[LoopEventKind]bool{}
	for _, e := range events {
		got[e.Kind] = true
	}
	for _, kind := range want {
		if !got[kind] {
			t.Fatalf("events %v missing kind %v", eventKinds(events), kind)
		}
	}
}

func TestRun_DefaultsParallelToolCallsOnRequests(t *testing.T) {
	p := &fakeProvider{steps: textOnlySteps("hi")}
	loop := New(p, nil, WithModel("gpt-test"))

	if _, err := loop.Run(context.Background(), nil, nil); err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if !p.lastRequest.ParallelToolCalls {
		t.Fatalf("ChatRequest.ParallelToolCalls = false, want true by default")
	}
}

func TestRun_WithParallelToolCallsFalseDisablesRequestFlag(t *testing.T) {
	p := &fakeProvider{steps: textOnlySteps("hi")}
	loop := New(p, nil, WithModel("gpt-test"), WithParallelToolCalls(false))

	if _, err := loop.Run(context.Background(), nil, nil); err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if p.lastRequest.ParallelToolCalls {
		t.Fatalf("ChatRequest.ParallelToolCalls = true, want false")
	}
}

func TestRun_ToolsSpecsCalledExactlyOncePerRun(t *testing.T) {
	call := message.ToolUsePart{ID: "call_1", Name: "loop_tool"}
	p := &fakeProvider{steps: toolCallSteps(call)}
	tools := &specsCountingToolRunner{fakeToolRunner: &fakeToolRunner{
		responses: map[string]message.ToolResultPart{"loop_tool": {}},
	}}
	loop := New(p, tools, WithModel("gpt-test"), WithMaxIterations(3))

	if _, err := loop.Run(context.Background(), nil, nil); !errors.Is(err, ErrMaxIterations) {
		t.Fatalf("Run() error = %v, want ErrMaxIterations", err)
	}
	if tools.specsCalls != 1 {
		t.Fatalf("Specs() called %d times, want exactly 1 (once per Run, not once per iteration)", tools.specsCalls)
	}
}

func TestRun_DoesNotMutateInputHistorySlice(t *testing.T) {
	p := &fakeProvider{steps: textOnlySteps("hi")}
	loop := New(p, nil, WithModel("gpt-test"))

	original := make([]message.Message, 1, 4)
	original[0] = message.NewMessage(message.RoleUser, message.TextPart{Text: "hello"})

	got, err := loop.Run(context.Background(), original, nil)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if len(got) < 2 {
		t.Fatalf("history = %+v, want at least 2 messages", got)
	}

	extended := original[:cap(original)]
	if extended[1].Role != "" {
		t.Fatalf("Run wrote into the caller's backing array at index 1: %+v, want it untouched (Run must copy before appending)", extended[1])
	}
}

package agentloop

import (
	"context"
	"io"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// streamStep is one scripted Recv() outcome: either an event or an error,
// never both.
type streamStep struct {
	ev  provider.StreamEvent
	err error
}

// scriptedStream is a provider.StreamReader over a fixed slice of steps,
// mirroring the pattern established by openai's chatCompletionsStream. It
// returns io.EOF once every step has been consumed.
type scriptedStream struct {
	steps []streamStep
	idx   int

	closeCount int
}

var _ provider.StreamReader = (*scriptedStream)(nil)

func newScriptedStream(steps []streamStep) *scriptedStream {
	return &scriptedStream{steps: steps}
}

func (s *scriptedStream) Recv() (provider.StreamEvent, error) {
	if s.idx >= len(s.steps) {
		return provider.StreamEvent{}, io.EOF
	}

	step := s.steps[s.idx]
	s.idx++

	if step.err != nil {
		return provider.StreamEvent{}, step.err
	}
	return step.ev, nil
}

// Close is idempotent, matching provider.StreamReader's contract; every
// call is counted so tests can assert it was called exactly once.
func (s *scriptedStream) Close() error {
	s.closeCount++
	return nil
}

// fakeProvider is a provider.Provider whose Stream returns a scriptedStream
// built from a fixed slice of steps, recording the ChatRequest it received.
type fakeProvider struct {
	steps []streamStep

	lastRequest provider.ChatRequest
}

var _ provider.Provider = (*fakeProvider)(nil)

func (p *fakeProvider) ID() string { return "fake-provider" }

func (p *fakeProvider) Models(ctx context.Context) ([]provider.ModelInfo, error) {
	return nil, nil
}

func (p *fakeProvider) Stream(ctx context.Context, req provider.ChatRequest) (provider.StreamReader, error) {
	p.lastRequest = req
	return newScriptedStream(p.steps), nil
}

// fakeToolRunner is a ToolRunner double: it returns a configured
// message.ToolResultPart or error per tool name, records every call and the
// context it was invoked with, and can optionally block until a channel is
// closed or the context is done, to support cancel-mid-tool tests.
type fakeToolRunner struct {
	specs     []provider.ToolSpec
	responses map[string]message.ToolResultPart
	errs      map[string]error
	block     <-chan struct{}

	calls []message.ToolUsePart
	ctxs  []context.Context
}

func (r *fakeToolRunner) Specs() []provider.ToolSpec {
	return r.specs
}

func (r *fakeToolRunner) Run(ctx context.Context, call message.ToolUsePart) (message.ToolResultPart, error) {
	r.calls = append(r.calls, call)
	r.ctxs = append(r.ctxs, ctx)

	if r.block != nil {
		select {
		case <-r.block:
		case <-ctx.Done():
			return message.ToolResultPart{}, ctx.Err()
		}
	}

	if err, ok := r.errs[call.Name]; ok {
		return message.ToolResultPart{}, err
	}
	return r.responses[call.Name], nil
}

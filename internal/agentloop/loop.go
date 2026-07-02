package agentloop

import (
	"context"
	"errors"
	"fmt"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// ErrMaxIterations is returned by Run when the configured iteration limit is
// reached without the model producing a turn with zero tool calls.
var ErrMaxIterations = errors.New("agentloop: max iterations reached")

// errModelRequired is returned by Run when no model has been configured via
// WithModel.
var errModelRequired = errors.New("agentloop: a model is required, configure one with WithModel")

// defaultMaxIterations is used when WithMaxIterations is not supplied.
const defaultMaxIterations = 20

// Loop drives one synchronous agent turn loop: per iteration it builds a
// provider.ChatRequest from the configured system prompt and the current
// history, streams and assembles the response, and dispatches any requested
// tool calls, repeating until the model produces a turn with no tool calls
// or the iteration limit is reached.
type Loop struct {
	provider  provider.Provider
	tools     ToolRunner
	systemMsg *message.Message
	model     string
	maxIter   int
}

// Option configures a Loop constructed by New.
type Option func(*Loop)

// WithSystemPrompt configures a system prompt. The resulting
// message.RoleSystem message.Message is built once, here, and is prepended
// to every provider.ChatRequest built by Run; it never appears in the
// history Run returns.
func WithSystemPrompt(prompt string) Option {
	msg := message.NewMessage(message.RoleSystem, message.TextPart{Text: prompt})
	return func(l *Loop) {
		l.systemMsg = &msg
	}
}

// WithModel sets the model identifier sent on every provider.ChatRequest.
// Run returns an error if no model has been configured.
func WithModel(model string) Option {
	return func(l *Loop) {
		l.model = model
	}
}

// WithMaxIterations overrides the default iteration limit. It panics if n is
// less than 1, since a Loop that can never run a single iteration is a
// programmer error, not a runtime condition.
func WithMaxIterations(n int) Option {
	if n < 1 {
		panic("agentloop: WithMaxIterations requires n >= 1")
	}
	return func(l *Loop) {
		l.maxIter = n
	}
}

// New constructs a Loop. It panics if p is nil, since a Loop with no
// provider can never run — a programmer error, not a runtime condition.
// tools may be nil, meaning no tools are available to the model.
func New(p provider.Provider, tools ToolRunner, opts ...Option) *Loop {
	if p == nil {
		panic("agentloop: New requires a non-nil provider.Provider")
	}

	l := &Loop{
		provider: p,
		tools:    tools,
		maxIter:  defaultMaxIterations,
	}
	for _, opt := range opts {
		opt(l)
	}
	return l
}

// Run drives the agent loop starting from history until the model produces
// a turn with no tool calls, the configured iteration limit is reached
// (ErrMaxIterations), ctx is canceled, or a stream or tool error occurs. It
// always returns the history grown so far, containing only complete
// messages: a canceled or errored iteration's partial assistant message, or
// partial tool results, are discarded. history itself is never mutated.
func (l *Loop) Run(ctx context.Context, history []message.Message, sink func(LoopEvent)) ([]message.Message, error) {
	if l.model == "" {
		return history, errModelRequired
	}

	current := make([]message.Message, len(history))
	copy(current, history)

	emit := func(ev LoopEvent) {
		if sink != nil {
			sink(ev)
		}
	}

	var specs []provider.ToolSpec
	if l.tools != nil {
		specs = l.tools.Specs()
	}

	for i := 1; i <= l.maxIter; i++ {
		if err := ctx.Err(); err != nil {
			return current, err
		}

		emit(LoopEvent{Kind: LoopIterationStart, Iteration: i})

		assistant, err := l.runIteration(ctx, current, specs, i, emit)
		if err != nil {
			return current, err
		}
		current = append(current, assistant)
		emit(LoopEvent{Kind: LoopMessageDone, Iteration: i, Message: &assistant})

		calls := toolUseParts(assistant)
		if len(calls) == 0 {
			return current, nil
		}

		toolMsg, err := runTools(ctx, calls, l.tools, i, emit)
		if err != nil {
			return current, err
		}
		current = append(current, toolMsg)
	}

	return current, ErrMaxIterations
}

// runIteration builds and sends one provider.ChatRequest and assembles the
// streamed response into a finalized assistant message.Message.
func (l *Loop) runIteration(ctx context.Context, history []message.Message, specs []provider.ToolSpec, iteration int, emit func(LoopEvent)) (message.Message, error) {
	msgs := history
	if l.systemMsg != nil {
		msgs = make([]message.Message, 0, len(history)+1)
		msgs = append(msgs, *l.systemMsg)
		msgs = append(msgs, history...)
	}

	req := provider.ChatRequest{Model: l.model, Messages: msgs, Tools: specs}

	reader, err := l.provider.Stream(ctx, req)
	if err != nil {
		if ctx.Err() != nil {
			return message.Message{}, ctx.Err()
		}
		return message.Message{}, fmt.Errorf("agentloop: open stream: %w", err)
	}

	assistant, _, err := assemble(ctx, reader, l.model, iteration, emit)
	if err != nil {
		return message.Message{}, err
	}

	return assistant, nil
}

// toolUseParts extracts the message.ToolUsePart values from msg.Parts, in
// order, ignoring any other part kind.
func toolUseParts(msg message.Message) []message.ToolUsePart {
	var calls []message.ToolUsePart
	for _, p := range msg.Parts {
		if call, ok := p.(message.ToolUsePart); ok {
			calls = append(calls, call)
		}
	}
	return calls
}

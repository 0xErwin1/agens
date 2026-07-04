package agentloop

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"strings"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// toolCallBuilder accumulates one in-flight tool call's fragmented
// arguments until the stream is finalized.
type toolCallBuilder struct {
	id, name string
	args     strings.Builder
}

// assembler accumulates one iteration's provider.StreamEvent values into a
// finalized assistant message.Message.
type assembler struct {
	text       strings.Builder
	builders   map[string]*toolCallBuilder
	order      []string
	stopReason string
	usage      *provider.Usage
}

// assemble drains r until io.EOF, translating each provider.StreamEvent
// into the matching LoopEvent (delivered via emit) and accumulating the
// finalized assistant message.Message for iteration. r is always closed
// before assemble returns.
//
// The loop never stops at EventDone: a trailing EventUsage can arrive
// after it, and stopping early would lose it. A Recv error other than
// io.EOF is reported as ctx.Err() when ctx has already been canceled, so
// callers can distinguish cancellation from a transport failure with
// errors.Is; otherwise it is wrapped and returned as-is.
func assemble(ctx context.Context, r provider.StreamReader, model string, iteration int, emit func(LoopEvent)) (message.Message, *provider.Usage, error) {
	defer func() { _ = r.Close() }()

	a := &assembler{builders: make(map[string]*toolCallBuilder)}

	for {
		ev, err := r.Recv()
		if err != nil {
			if errors.Is(err, io.EOF) {
				return a.finalize(model)
			}
			if ctx.Err() != nil {
				return message.Message{}, nil, ctx.Err()
			}
			return message.Message{}, nil, fmt.Errorf("agentloop: receive stream event: %w", err)
		}

		if err := a.apply(ev, iteration, emit); err != nil {
			return message.Message{}, nil, err
		}
	}
}

// apply folds one provider.StreamEvent into the assembler's in-progress
// state and emits the matching LoopEvent, if any.
func (a *assembler) apply(ev provider.StreamEvent, iteration int, emit func(LoopEvent)) error {
	switch ev.Type {
	case provider.EventTextDelta:
		a.text.WriteString(ev.Text)
		emit(LoopEvent{Kind: LoopTextDelta, Iteration: iteration, Text: ev.Text})

	case provider.EventReasoningDelta:
		// Reasoning is surfaced live but never folded into the finalized
		// message: it is the model's ephemeral thinking, not its answer.
		emit(LoopEvent{Kind: LoopReasoningDelta, Iteration: iteration, Text: ev.Text})

	case provider.EventToolCallStart:
		if _, exists := a.builders[ev.ToolCallID]; exists {
			return fmt.Errorf("agentloop: duplicate tool call start for id %q", ev.ToolCallID)
		}
		a.builders[ev.ToolCallID] = &toolCallBuilder{id: ev.ToolCallID, name: ev.ToolName}
		a.order = append(a.order, ev.ToolCallID)
		emit(LoopEvent{
			Kind:      LoopToolCallStarted,
			Iteration: iteration,
			ToolCall:  message.ToolUsePart{ID: ev.ToolCallID, Name: ev.ToolName},
		})

	case provider.EventToolArgsDelta:
		b, ok := a.builders[ev.ToolCallID]
		if !ok {
			return fmt.Errorf("agentloop: tool args delta for unknown tool call id %q", ev.ToolCallID)
		}
		b.args.WriteString(ev.ArgsDelta)

	case provider.EventToolCallEnd:
		if _, ok := a.builders[ev.ToolCallID]; !ok {
			return fmt.Errorf("agentloop: tool call end for unknown tool call id %q", ev.ToolCallID)
		}

	case provider.EventUsage:
		a.usage = ev.Usage
		emit(LoopEvent{Kind: LoopUsage, Iteration: iteration, Usage: ev.Usage})

	case provider.EventDone:
		a.stopReason = ev.StopReason
	}

	return nil
}

// finalize builds the finished assistant message.Message from the
// accumulated text and tool calls. A zero-parts assistant message is
// valid. An empty argument string finalizes as "{}"; anything else that is
// not valid JSON is a hard error, since it would otherwise re-enter the
// conversation history and fail far from its cause on the next provider
// request.
func (a *assembler) finalize(model string) (message.Message, *provider.Usage, error) {
	var parts message.Parts

	if a.text.Len() > 0 {
		parts = append(parts, message.TextPart{Text: a.text.String()})
	}

	for _, id := range a.order {
		b := a.builders[id]

		raw := b.args.String()
		if raw == "" {
			raw = "{}"
		}
		input := json.RawMessage(raw)
		if !json.Valid(input) {
			return message.Message{}, nil, fmt.Errorf("agentloop: tool call %q: invalid arguments JSON", id)
		}

		parts = append(parts, message.ToolUsePart{ID: b.id, Name: b.name, Input: input})
	}

	msg := message.NewMessage(message.RoleAssistant, parts...)
	msg.Model = model
	msg.StopReason = a.stopReason

	return msg, a.usage, nil
}

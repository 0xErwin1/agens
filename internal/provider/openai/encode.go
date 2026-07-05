package openai

import (
	"fmt"
	"strings"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// encodeRequest translates a provider-neutral ChatRequest into the
// chat-completions wire body. Streaming with usage reporting is always
// requested; Temperature and MaxOutputTokens follow the omission rules
// documented on wireRequest.
func encodeRequest(req provider.ChatRequest) (wireRequest, error) {
	wire := wireRequest{
		Model:               req.Model,
		Stream:              true,
		StreamOptions:       &wireStreamOptions{IncludeUsage: true},
		ParallelToolCalls:   req.ParallelToolCalls,
		MaxCompletionTokens: req.MaxOutputTokens,
		Temperature:         req.Temperature,
	}

	for _, msg := range req.Messages {
		encoded, err := encodeMessage(msg)
		if err != nil {
			return wireRequest{}, err
		}
		wire.Messages = append(wire.Messages, encoded...)
	}

	for _, tool := range req.Tools {
		wire.Tools = append(wire.Tools, wireTool{
			Type: "function",
			Function: wireFunction{
				Name:        tool.Name,
				Description: tool.Description,
				Parameters:  tool.InputSchema,
			},
		})
	}

	return wire, nil
}

// encodeMessage dispatches a message.Message to its role-specific encoder.
// A user message may fan out into more than one wire message; every other
// role produces exactly one.
func encodeMessage(msg message.Message) ([]wireMessage, error) {
	switch msg.Role {
	case message.RoleSystem:
		return encodeSystemMessage(msg)
	case message.RoleAssistant:
		return encodeAssistantMessage(msg)
	case message.RoleUser:
		return encodeUserMessage(msg)
	default:
		return nil, fmt.Errorf("openai: encode: unknown role %q", msg.Role)
	}
}

func encodeSystemMessage(msg message.Message) ([]wireMessage, error) {
	var texts []string
	for _, part := range msg.Parts {
		text, ok := part.(message.TextPart)
		if !ok {
			return nil, fmt.Errorf("openai: encode: system message contains disallowed part kind %T", part)
		}
		texts = append(texts, text.Text)
	}

	content := strings.Join(texts, "\n")
	return []wireMessage{{Role: "system", Content: &content}}, nil
}

// encodeAssistantMessage produces one wire message carrying both the
// assistant's text (if any) and its tool_calls (if any). Content is left
// nil, rather than an empty string, when there is no text part, so an
// assistant message that only requests tool calls omits "content" on the
// wire per wireMessage's documented semantics.
func encodeAssistantMessage(msg message.Message) ([]wireMessage, error) {
	var texts []string
	var toolCalls []wireToolCall

	for _, part := range msg.Parts {
		switch p := part.(type) {
		case message.TextPart:
			texts = append(texts, p.Text)
		case message.ToolUsePart:
			toolCalls = append(toolCalls, wireToolCall{
				ID:   p.ID,
				Type: "function",
				Function: wireCallFunction{
					Name:      p.Name,
					Arguments: string(p.Input),
				},
			})
		default:
			return nil, fmt.Errorf("openai: encode: assistant message contains disallowed part kind %T", part)
		}
	}

	wire := wireMessage{Role: "assistant", ToolCalls: toolCalls}
	if len(texts) > 0 {
		content := strings.Join(texts, "\n")
		wire.Content = &content
	}
	return []wireMessage{wire}, nil
}

// encodeUserMessage fans a single user message out to 1..N wire messages:
// one role:"tool" message per ToolResultPart, in the order they appear,
// followed by a single role:"user" message carrying the joined text parts
// (if any). Tool messages precede the user text message unconditionally,
// since the wire requires them to immediately follow the assistant
// tool_calls they answer.
func encodeUserMessage(msg message.Message) ([]wireMessage, error) {
	var wireMessages []wireMessage
	var texts []string

	for _, part := range msg.Parts {
		switch p := part.(type) {
		case message.TextPart:
			texts = append(texts, p.Text)
		case message.ToolResultPart:
			wireMessages = append(wireMessages, encodeToolResult(p))
		default:
			return nil, fmt.Errorf("openai: encode: user message contains disallowed part kind %T", part)
		}
	}

	if len(texts) > 0 {
		content := strings.Join(texts, "\n")
		wireMessages = append(wireMessages, wireMessage{Role: "user", Content: &content})
	}

	return wireMessages, nil
}

// encodeToolResult flattens a ToolResultPart's TextPart content into a
// single role:"tool" wire message. An IsError result is prefixed with
// "Error: " — a lossy mapping, since the wire has no separate error flag
// for tool messages.
func encodeToolResult(part message.ToolResultPart) wireMessage {
	var texts []string
	for _, p := range part.Content {
		if text, ok := p.(message.TextPart); ok {
			texts = append(texts, text.Text)
		}
	}

	content := strings.Join(texts, "\n")
	if part.IsError {
		content = "Error: " + content
	}

	return wireMessage{
		Role:       "tool",
		Content:    &content,
		ToolCallID: part.ToolUseID,
	}
}

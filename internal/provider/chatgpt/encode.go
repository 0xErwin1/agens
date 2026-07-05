package chatgpt

import (
	"fmt"
	"strings"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// toolChoiceAuto is the only tool_choice value this wire ever sends: the
// model is always left free to decide whether to call a tool.
const toolChoiceAuto = "auto"

// encodeRequest translates a provider-neutral ChatRequest into the
// /responses request body. Temperature and MaxOutputTokens are intentionally
// never copied onto the wire: the Responses API endpoint this provider
// targets is invoked without per-call sampling/length overrides, so the
// wire always relies on the model's own defaults.
func encodeRequest(req provider.ChatRequest) (wireRequest, error) {
	wire := wireRequest{
		Model:             req.Model,
		Input:             []wireInputItem{},
		ToolChoice:        toolChoiceAuto,
		ParallelToolCalls: req.ParallelToolCalls,
		Store:             false,
		Stream:            true,
		Include:           []string{},
		// Always request a reasoning summary so the "thinking" can be shown;
		// Effort is included only when the caller set one.
		Reasoning: &wireReasoning{Effort: req.Effort, Summary: "auto"},
	}

	var instructionParts []string

	for _, msg := range req.Messages {
		if msg.Role == message.RoleSystem {
			text, err := encodeSystemInstructions(msg)
			if err != nil {
				return wireRequest{}, err
			}
			if text != "" {
				instructionParts = append(instructionParts, text)
			}
			continue
		}

		items, err := encodeMessage(msg)
		if err != nil {
			return wireRequest{}, err
		}
		wire.Input = append(wire.Input, items...)
	}

	if len(instructionParts) > 0 {
		wire.Instructions = strings.Join(instructionParts, "\n")
	}

	for _, tool := range req.Tools {
		wire.Tools = append(wire.Tools, wireTool{
			Type:        "function",
			Name:        tool.Name,
			Description: tool.Description,
			Parameters:  tool.InputSchema,
		})
	}

	return wire, nil
}

// encodeSystemInstructions joins a system message's text parts into a single
// string destined for the request's top-level "instructions" field. A
// system message on this wire is never an input item.
func encodeSystemInstructions(msg message.Message) (string, error) {
	var texts []string
	for _, part := range msg.Parts {
		text, ok := part.(message.TextPart)
		if !ok {
			return "", fmt.Errorf("chatgpt: encode: system message contains disallowed part kind %T", part)
		}
		texts = append(texts, text.Text)
	}

	return strings.Join(texts, "\n"), nil
}

// encodeMessage dispatches a non-system message.Message to its role-specific
// encoder. A message may fan out into more than one wireInputItem: one item
// is emitted per part, in order.
func encodeMessage(msg message.Message) ([]wireInputItem, error) {
	switch msg.Role {
	case message.RoleUser:
		return encodeUserMessage(msg)
	case message.RoleAssistant:
		return encodeAssistantMessage(msg)
	default:
		return nil, fmt.Errorf("chatgpt: encode: unknown role %q", msg.Role)
	}
}

// encodeUserMessage emits one wireInputItem per part, in order: a
// message/input_text item for each TextPart, and a function_call_output
// item for each ToolResultPart.
func encodeUserMessage(msg message.Message) ([]wireInputItem, error) {
	var items []wireInputItem

	for _, part := range msg.Parts {
		switch p := part.(type) {
		case message.TextPart:
			items = append(items, wireInputItem{
				Type: responseItemTypeMessage,
				Role: "user",
				Content: []wireContentItem{
					{Type: contentTypeInputText, Text: p.Text},
				},
			})
		case message.ToolResultPart:
			items = append(items, encodeToolResult(p))
		default:
			return nil, fmt.Errorf("chatgpt: encode: user message contains disallowed part kind %T", part)
		}
	}

	return items, nil
}

// encodeAssistantMessage emits one wireInputItem per part, in order: a
// message/output_text item for each TextPart, and a function_call item for
// each ToolUsePart.
func encodeAssistantMessage(msg message.Message) ([]wireInputItem, error) {
	var items []wireInputItem

	for _, part := range msg.Parts {
		switch p := part.(type) {
		case message.TextPart:
			items = append(items, wireInputItem{
				Type: responseItemTypeMessage,
				Role: "assistant",
				Content: []wireContentItem{
					{Type: contentTypeOutputText, Text: p.Text},
				},
			})
		case message.ToolUsePart:
			items = append(items, wireInputItem{
				Type:      responseItemTypeFunctionCall,
				Name:      p.Name,
				Arguments: string(p.Input),
				CallID:    p.ID,
			})
		default:
			return nil, fmt.Errorf("chatgpt: encode: assistant message contains disallowed part kind %T", part)
		}
	}

	return items, nil
}

// encodeToolResult flattens a ToolResultPart's TextPart content into a
// single function_call_output item. An IsError result is prefixed with
// "Error: ", mirroring the openai provider's tool-result convention — a
// lossy mapping, since this wire has no separate error flag for tool
// outputs. Content parts other than TextPart are silently skipped, again
// matching the openai provider's convention.
func encodeToolResult(part message.ToolResultPart) wireInputItem {
	var texts []string
	for _, p := range part.Content {
		if text, ok := p.(message.TextPart); ok {
			texts = append(texts, text.Text)
		}
	}

	output := strings.Join(texts, "\n")
	if part.IsError {
		output = "Error: " + output
	}

	return wireInputItem{
		Type:   responseItemTypeFunctionCallOutput,
		CallID: part.ToolUseID,
		Output: output,
	}
}

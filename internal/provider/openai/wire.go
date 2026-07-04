// Package openai implements provider.Provider for OpenAI's chat-completions
// API, authenticated with a static API key.
package openai

import "encoding/json"

// wireRequest is the chat-completions request body.
type wireRequest struct {
	Model               string             `json:"model"`
	Messages            []wireMessage      `json:"messages"`
	Tools               []wireTool         `json:"tools,omitempty"`
	Stream              bool               `json:"stream"`
	StreamOptions       *wireStreamOptions `json:"stream_options,omitempty"`
	MaxCompletionTokens int                `json:"max_completion_tokens,omitempty"`
	Temperature         *float64           `json:"temperature,omitempty"`
}

// wireMessage is one message in a chat-completions request body.
//
// Content is a pointer so an explicit empty string ("") can be distinguished
// from an absent field: a tool-result message must emit content:"" while an
// assistant message carrying only tool_calls must omit content entirely.
type wireMessage struct {
	Role       string         `json:"role"`
	Content    *string        `json:"content,omitempty"`
	ToolCalls  []wireToolCall `json:"tool_calls,omitempty"`
	ToolCallID string         `json:"tool_call_id,omitempty"`
}

// wireTool describes one callable function offered to the model.
type wireTool struct {
	Type     string       `json:"type"`
	Function wireFunction `json:"function"`
}

// wireFunction is the function schema carried by wireTool.
type wireFunction struct {
	Name        string          `json:"name"`
	Description string          `json:"description,omitempty"`
	Parameters  json.RawMessage `json:"parameters"`
}

// wireToolCall is a fully-formed tool call in an assistant request message.
type wireToolCall struct {
	ID       string           `json:"id"`
	Type     string           `json:"type"`
	Function wireCallFunction `json:"function"`
}

// wireCallFunction is the function invocation payload shared by request-side
// tool calls and response-side tool call deltas.
type wireCallFunction struct {
	Name      string `json:"name"`
	Arguments string `json:"arguments"`
}

// wireStreamOptions requests that a final usage-only chunk be sent.
type wireStreamOptions struct {
	IncludeUsage bool `json:"include_usage"`
}

// wireChunk is one streamed chat-completions response chunk.
type wireChunk struct {
	Choices []wireChoice `json:"choices"`
	Usage   *wireUsage   `json:"usage"`
}

// wireChoice is one choice within a streamed response chunk.
//
// FinishReason is a pointer because its presence, not just its value,
// signals that the choice has ended.
type wireChoice struct {
	Delta        wireDelta `json:"delta"`
	FinishReason *string   `json:"finish_reason"`
}

// wireDelta is the incremental content of one streamed choice.
type wireDelta struct {
	Content   string              `json:"content"`
	ToolCalls []wireToolCallDelta `json:"tool_calls"`
}

// wireToolCallDelta is one incremental tool call fragment. Only the first
// fragment for a given Index carries ID and Function.Name; later fragments
// for the same Index carry only Index and Function.Arguments.
type wireToolCallDelta struct {
	Index    int              `json:"index"`
	ID       string           `json:"id"`
	Function wireCallFunction `json:"function"`
}

// wireUsage is the token accounting delivered in the final usage-only chunk.
type wireUsage struct {
	PromptTokens     int `json:"prompt_tokens"`
	CompletionTokens int `json:"completion_tokens"`
}

// wireErrorEnvelope is the body of a non-2xx chat-completions response.
type wireErrorEnvelope struct {
	Error struct {
		Message string `json:"message"`
		Type    string `json:"type"`
		Code    string `json:"code"`
	} `json:"error"`
}

// wireModelsResponse is the /models response body.
type wireModelsResponse struct {
	Data []wireModel `json:"data"`
}

// wireModel is one entry of a /models response's "data" array. It carries
// only the id field Models maps into provider.ModelInfo; the endpoint also
// returns object, created, and owned_by, which are intentionally ignored.
type wireModel struct {
	ID string `json:"id"`
}

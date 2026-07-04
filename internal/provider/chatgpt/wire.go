package chatgpt

import "encoding/json"

// The responseItemType* constants are the wire discriminator values written
// to a wireInputItem's "type" field.
const (
	responseItemTypeMessage            = "message"
	responseItemTypeFunctionCall       = "function_call"
	responseItemTypeFunctionCallOutput = "function_call_output"
)

// The contentType* constants are the wire discriminator values written to a
// wireContentItem's "type" field. The Responses API distinguishes text
// supplied to the model (input_text) from text produced by it
// (output_text), even though both carry a plain string.
const (
	contentTypeInputText  = "input_text"
	contentTypeOutputText = "output_text"
)

// wireRequest is the /responses request body. Unlike the chat-completions
// wire, Temperature, MaxOutputTokens, TopP, and PreviousResponseID have no
// corresponding fields here: this type only ever emits the fields declared
// below, so those values can never leak onto the wire by accident.
type wireRequest struct {
	Model             string          `json:"model"`
	Instructions      string          `json:"instructions,omitempty"`
	Input             []wireInputItem `json:"input"`
	Tools             []wireTool      `json:"tools,omitempty"`
	ToolChoice        string          `json:"tool_choice"`
	ParallelToolCalls bool            `json:"parallel_tool_calls"`
	Store             bool            `json:"store"`
	Stream            bool            `json:"stream"`
	Include           []string        `json:"include"`
}

// wireInputItem is one entry of a /responses request's "input" array (a
// ResponseItem in the Responses API's own terminology). Which fields are
// meaningful depends on Type:
//
//	message:               Role, Content
//	function_call:         Name, Arguments, CallID
//	function_call_output:  CallID, Output
//
// All other fields hold their zero value and are omitted from the wire.
type wireInputItem struct {
	Type      string            `json:"type"`
	Role      string            `json:"role,omitempty"`
	Content   []wireContentItem `json:"content,omitempty"`
	Name      string            `json:"name,omitempty"`
	Arguments string            `json:"arguments,omitempty"`
	CallID    string            `json:"call_id,omitempty"`
	Output    string            `json:"output,omitempty"`
}

// wireContentItem is one element of a "message" wireInputItem's content
// array.
type wireContentItem struct {
	Type string `json:"type"`
	Text string `json:"text"`
}

// wireTool describes one callable function offered to the model, in the
// Responses API's flat shape (as opposed to chat-completions' nested
// {"type":"function","function":{...}}).
type wireTool struct {
	Type        string          `json:"type"`
	Name        string          `json:"name"`
	Description string          `json:"description,omitempty"`
	Parameters  json.RawMessage `json:"parameters"`
}

// wireModelsResponse is the /models response body.
type wireModelsResponse struct {
	Models []wireModel `json:"models"`
}

// wireModel is one entry of a /models response's "models" array. It carries
// only the fields Models maps into provider.ModelInfo; the backend sends
// many other fields that are intentionally ignored.
type wireModel struct {
	Slug          string `json:"slug"`
	DisplayName   string `json:"display_name"`
	ContextWindow int    `json:"context_window"`
	Visibility    string `json:"visibility"`
}

// wireStreamEvent is one decoded /responses SSE event. Dispatch is driven
// entirely by Type; unlike chat-completions' "event:" line, the Responses
// API repeats its event kind inside the JSON body, and that is the value
// this type captures.
type wireStreamEvent struct {
	Type     string               `json:"type"`
	Delta    string               `json:"delta"`
	Item     *wireResponseItem    `json:"item"`
	Response *wireResponsePayload `json:"response"`
}

// wireResponseItem is the item carried by a response.output_item.added or
// response.output_item.done event. Only function_call items are meaningful
// to this provider; Name and Arguments are populated for that type only.
type wireResponseItem struct {
	Type      string `json:"type"`
	CallID    string `json:"call_id"`
	Name      string `json:"name"`
	Arguments string `json:"arguments"`
}

// wireResponsePayload is the "response" object nested in response.completed,
// response.failed, and response.incomplete events. Which field is populated
// depends on which of those three event types carries it.
type wireResponsePayload struct {
	Usage             *wireResponseUsage     `json:"usage"`
	Error             *wireResponseError     `json:"error"`
	IncompleteDetails *wireIncompleteDetails `json:"incomplete_details"`
}

// wireResponseUsage is the token accounting delivered on response.completed.
type wireResponseUsage struct {
	InputTokens  int `json:"input_tokens"`
	OutputTokens int `json:"output_tokens"`
}

// wireResponseError is the error envelope delivered on response.failed.
type wireResponseError struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

// wireIncompleteDetails is the reason envelope delivered on
// response.incomplete.
type wireIncompleteDetails struct {
	Reason string `json:"reason"`
}

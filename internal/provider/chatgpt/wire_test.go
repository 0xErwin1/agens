package chatgpt

import (
	"encoding/json"
	"testing"
)

// marshalToMap marshals v and unmarshals it back into a generic map for
// structural comparison, so tests assert on JSON shape rather than exact
// struct field order or Go-side representation.
func marshalToMap(t *testing.T, v any) map[string]any {
	t.Helper()

	data, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("json.Marshal(%T) error = %v", v, err)
	}

	var decoded map[string]any
	if err := json.Unmarshal(data, &decoded); err != nil {
		t.Fatalf("json.Unmarshal(%T bytes) error = %v", v, err)
	}
	return decoded
}

func baseWireRequest() wireRequest {
	return wireRequest{
		Model:             "gpt-5",
		Input:             []wireInputItem{},
		ToolChoice:        "auto",
		ParallelToolCalls: false,
		Store:             false,
		Stream:            true,
		Include:           []string{},
	}
}

func TestWireRequest_FixedFieldsAlwaysPresent(t *testing.T) {
	got := marshalToMap(t, baseWireRequest())

	if got["tool_choice"] != "auto" {
		t.Fatalf("tool_choice = %v, want %q", got["tool_choice"], "auto")
	}
	if got["parallel_tool_calls"] != false {
		t.Fatalf("parallel_tool_calls = %v, want false", got["parallel_tool_calls"])
	}
	if got["store"] != false {
		t.Fatalf("store = %v, want false", got["store"])
	}
	if got["stream"] != true {
		t.Fatalf("stream = %v, want true", got["stream"])
	}

	include, ok := got["include"].([]any)
	if !ok {
		t.Fatalf("include = %v (%T), want an empty JSON array", got["include"], got["include"])
	}
	if len(include) != 0 {
		t.Fatalf("include = %v, want empty", include)
	}
}

func TestWireRequest_InstructionsOmittedWhenEmpty(t *testing.T) {
	got := marshalToMap(t, baseWireRequest())

	if _, has := got["instructions"]; has {
		t.Fatalf("instructions = %v, want omitted when empty", got["instructions"])
	}
}

func TestWireRequest_InstructionsIncludedWhenSet(t *testing.T) {
	wire := baseWireRequest()
	wire.Instructions = "be terse"

	got := marshalToMap(t, wire)

	if got["instructions"] != "be terse" {
		t.Fatalf("instructions = %v, want %q", got["instructions"], "be terse")
	}
}

func TestWireRequest_ToolsOmittedWhenNone(t *testing.T) {
	got := marshalToMap(t, baseWireRequest())

	if _, has := got["tools"]; has {
		t.Fatalf("tools = %v, want omitted when none", got["tools"])
	}
}

func TestWireRequest_ForbiddenFieldsNeverPresent(t *testing.T) {
	got := marshalToMap(t, baseWireRequest())

	for _, key := range []string{"temperature", "top_p", "max_output_tokens", "previous_response_id"} {
		if _, has := got[key]; has {
			t.Fatalf("wireRequest JSON has key %q, want it never emitted on this wire", key)
		}
	}
}

func TestWireInputItem_MessageUserInputText(t *testing.T) {
	item := wireInputItem{
		Type: responseItemTypeMessage,
		Role: "user",
		Content: []wireContentItem{
			{Type: contentTypeInputText, Text: "hi"},
		},
	}

	got := marshalToMap(t, item)

	if got["type"] != "message" || got["role"] != "user" {
		t.Fatalf("item = %v, want type=message role=user", got)
	}
	content, ok := got["content"].([]any)
	if !ok || len(content) != 1 {
		t.Fatalf("content = %v, want 1 content item", got["content"])
	}
	c0 := content[0].(map[string]any)
	if c0["type"] != "input_text" || c0["text"] != "hi" {
		t.Fatalf("content[0] = %v, want type=input_text text=hi", c0)
	}
}

func TestWireInputItem_MessageAssistantOutputText(t *testing.T) {
	item := wireInputItem{
		Type: responseItemTypeMessage,
		Role: "assistant",
		Content: []wireContentItem{
			{Type: contentTypeOutputText, Text: "sure thing"},
		},
	}

	got := marshalToMap(t, item)

	content := got["content"].([]any)
	c0 := content[0].(map[string]any)
	if c0["type"] != "output_text" || c0["text"] != "sure thing" {
		t.Fatalf("content[0] = %v, want type=output_text text=%q", c0, "sure thing")
	}
}

func TestWireInputItem_FunctionCallShape(t *testing.T) {
	item := wireInputItem{
		Type:      responseItemTypeFunctionCall,
		Name:      "get_weather",
		Arguments: `{"loc":"NYC"}`,
		CallID:    "call_1",
	}

	got := marshalToMap(t, item)

	if got["type"] != "function_call" || got["name"] != "get_weather" || got["arguments"] != `{"loc":"NYC"}` || got["call_id"] != "call_1" {
		t.Fatalf("item = %v, want type=function_call name=get_weather arguments={\"loc\":\"NYC\"} call_id=call_1", got)
	}
	if _, has := got["role"]; has {
		t.Fatalf("function_call item has role = %v, want omitted", got["role"])
	}
	if _, has := got["content"]; has {
		t.Fatalf("function_call item has content = %v, want omitted", got["content"])
	}
}

func TestWireInputItem_FunctionCallOutputShape(t *testing.T) {
	item := wireInputItem{
		Type:   responseItemTypeFunctionCallOutput,
		CallID: "call_1",
		Output: "sunny",
	}

	got := marshalToMap(t, item)

	if got["type"] != "function_call_output" || got["call_id"] != "call_1" || got["output"] != "sunny" {
		t.Fatalf("item = %v, want type=function_call_output call_id=call_1 output=sunny", got)
	}
}

func TestWireTool_FlatResponsesAPIShape(t *testing.T) {
	tool := wireTool{
		Type:        "function",
		Name:        "get_weather",
		Description: "fetch weather",
		Parameters:  json.RawMessage(`{"type":"object"}`),
	}

	got := marshalToMap(t, tool)

	if got["type"] != "function" || got["name"] != "get_weather" || got["description"] != "fetch weather" {
		t.Fatalf("tool = %v, want type=function name=get_weather description=%q", got, "fetch weather")
	}
	params, ok := got["parameters"].(map[string]any)
	if !ok || params["type"] != "object" {
		t.Fatalf("tool.parameters = %v, want {type:object}", got["parameters"])
	}
	if _, has := got["function"]; has {
		t.Fatalf("tool has nested %q key = %v, want the flat Responses API shape, not chat-completions nesting", "function", got["function"])
	}
}

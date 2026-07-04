package chatgpt

import (
	"encoding/json"
	"testing"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

func TestEncodeRequest_SystemMessageBecomesInstructions(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			message.NewMessage(message.RoleSystem, message.TextPart{Text: "be terse"}, message.TextPart{Text: "no yapping"}),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	if got["instructions"] != "be terse\nno yapping" {
		t.Fatalf("instructions = %v, want %q", got["instructions"], "be terse\nno yapping")
	}
	if _, has := got["input"]; !has {
		t.Fatalf("input missing from wire output")
	}
	input := got["input"].([]any)
	if len(input) != 0 {
		t.Fatalf("input = %v, want empty (system message must not become an input item)", input)
	}
}

func TestEncodeRequest_SystemMessageRejectsToolParts(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			message.NewMessage(message.RoleSystem, message.ToolUsePart{ID: "call_1", Name: "x", Input: json.RawMessage(`{}`)}),
		},
	}

	if _, err := encodeRequest(req); err == nil {
		t.Fatal("encodeRequest() error = nil, want error for misplaced tool_use part in system message")
	}
}

func TestEncodeRequest_UserTextBecomesInputTextMessage(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			message.NewMessage(message.RoleUser, message.TextPart{Text: "hi"}),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	input := got["input"].([]any)
	if len(input) != 1 {
		t.Fatalf("input = %v, want 1 item", input)
	}

	item := input[0].(map[string]any)
	if item["type"] != "message" || item["role"] != "user" {
		t.Fatalf("input[0] = %v, want type=message role=user", item)
	}
	content := item["content"].([]any)
	c0 := content[0].(map[string]any)
	if c0["type"] != "input_text" || c0["text"] != "hi" {
		t.Fatalf("input[0].content[0] = %v, want type=input_text text=hi", c0)
	}
}

func TestEncodeRequest_AssistantTextBecomesOutputTextMessage(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			message.NewMessage(message.RoleAssistant, message.TextPart{Text: "sure thing"}),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	input := got["input"].([]any)
	item := input[0].(map[string]any)
	if item["type"] != "message" || item["role"] != "assistant" {
		t.Fatalf("input[0] = %v, want type=message role=assistant", item)
	}
	content := item["content"].([]any)
	c0 := content[0].(map[string]any)
	if c0["type"] != "output_text" || c0["text"] != "sure thing" {
		t.Fatalf("input[0].content[0] = %v, want type=output_text text=%q", c0, "sure thing")
	}
}

func TestEncodeRequest_AssistantToolUseBecomesFunctionCall(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			message.NewMessage(message.RoleAssistant,
				message.ToolUsePart{ID: "call_1", Name: "get_weather", Input: json.RawMessage(`{"loc":"NYC"}`)},
			),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	input := got["input"].([]any)
	if len(input) != 1 {
		t.Fatalf("input = %v, want 1 item", input)
	}

	item := input[0].(map[string]any)
	if item["type"] != "function_call" || item["name"] != "get_weather" || item["call_id"] != "call_1" {
		t.Fatalf("input[0] = %v, want type=function_call name=get_weather call_id=call_1", item)
	}
	if item["arguments"] != `{"loc":"NYC"}` {
		t.Fatalf("input[0].arguments = %v, want the raw input as a JSON string %q", item["arguments"], `{"loc":"NYC"}`)
	}
}

func TestEncodeRequest_AssistantRejectsToolResultPart(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			message.NewMessage(message.RoleAssistant,
				message.ToolResultPart{ToolUseID: "call_1", Content: message.Parts{message.TextPart{Text: "42"}}},
			),
		},
	}

	if _, err := encodeRequest(req); err == nil {
		t.Fatal("encodeRequest() error = nil, want error for misplaced tool_result part in assistant message")
	}
}

func TestEncodeRequest_ToolResultBecomesFunctionCallOutput(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			message.NewMessage(message.RoleUser,
				message.ToolResultPart{ToolUseID: "call_1", Content: message.Parts{message.TextPart{Text: "sunny"}}},
			),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	input := got["input"].([]any)
	item := input[0].(map[string]any)
	if item["type"] != "function_call_output" || item["call_id"] != "call_1" || item["output"] != "sunny" {
		t.Fatalf("input[0] = %v, want type=function_call_output call_id=call_1 output=sunny", item)
	}
}

func TestEncodeRequest_ToolResultErrorMatchesOpenAIPrefixConvention(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			message.NewMessage(message.RoleUser,
				message.ToolResultPart{ToolUseID: "call_1", Content: message.Parts{message.TextPart{Text: "boom"}}, IsError: true},
			),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	item := got["input"].([]any)[0].(map[string]any)
	if item["output"] != "Error: boom" {
		t.Fatalf("input[0].output = %v, want %q", item["output"], "Error: boom")
	}
}

func TestEncodeRequest_UserRejectsToolUsePart(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			message.NewMessage(message.RoleUser,
				message.ToolUsePart{ID: "call_1", Name: "x", Input: json.RawMessage(`{}`)},
			),
		},
	}

	if _, err := encodeRequest(req); err == nil {
		t.Fatal("encodeRequest() error = nil, want error for misplaced tool_use part in user message")
	}
}

func TestEncodeRequest_MultiPartAssistantMessageEmitsItemsInOrder(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			message.NewMessage(message.RoleAssistant,
				message.TextPart{Text: "let me check"},
				message.ToolUsePart{ID: "call_1", Name: "get_weather", Input: json.RawMessage(`{}`)},
			),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	input := got["input"].([]any)
	if len(input) != 2 {
		t.Fatalf("input = %v, want 2 items (one per part, in order)", input)
	}

	first := input[0].(map[string]any)
	if first["type"] != "message" || first["role"] != "assistant" {
		t.Fatalf("input[0] = %v, want the text part first", first)
	}

	second := input[1].(map[string]any)
	if second["type"] != "function_call" || second["call_id"] != "call_1" {
		t.Fatalf("input[1] = %v, want the tool call second", second)
	}
}

func TestEncodeRequest_ToolsFlatShape(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Tools: []provider.ToolSpec{
			{Name: "get_weather", Description: "fetch weather", InputSchema: json.RawMessage(`{"type":"object"}`)},
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	tools, ok := got["tools"].([]any)
	if !ok || len(tools) != 1 {
		t.Fatalf("tools = %v, want 1 tool", got["tools"])
	}

	tool := tools[0].(map[string]any)
	if tool["type"] != "function" || tool["name"] != "get_weather" || tool["description"] != "fetch weather" {
		t.Fatalf("tools[0] = %v, want type=function name=get_weather description=%q", tool, "fetch weather")
	}
	if _, has := tool["function"]; has {
		t.Fatalf("tools[0] has nested function key = %v, want the flat Responses API shape", tool["function"])
	}
	params, ok := tool["parameters"].(map[string]any)
	if !ok || params["type"] != "object" {
		t.Fatalf("tools[0].parameters = %v, want {type:object}", tool["parameters"])
	}
}

func TestEncodeRequest_TemperatureAndMaxOutputTokensNeverLeakOntoWire(t *testing.T) {
	temp := 0.7
	req := provider.ChatRequest{
		Model:           "gpt-5",
		Temperature:     &temp,
		MaxOutputTokens: 128,
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	for _, key := range []string{"temperature", "top_p", "max_output_tokens", "previous_response_id"} {
		if _, has := got[key]; has {
			t.Fatalf("wire JSON has key %q = %v, want it never sent even when ChatRequest sets the equivalent field", key, got[key])
		}
	}
}

func TestEncodeRequest_FixedFieldsPresent(t *testing.T) {
	req := provider.ChatRequest{Model: "gpt-5"}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	if got["tool_choice"] != "auto" || got["parallel_tool_calls"] != false || got["store"] != false || got["stream"] != true {
		t.Fatalf("wire = %v, want tool_choice=auto parallel_tool_calls=false store=false stream=true", got)
	}
	include, ok := got["include"].([]any)
	if !ok || len(include) != 0 {
		t.Fatalf("include = %v, want an empty JSON array", got["include"])
	}
	if _, has := got["instructions"]; has {
		t.Fatalf("instructions = %v, want omitted with no system message", got["instructions"])
	}
	if _, has := got["tools"]; has {
		t.Fatalf("tools = %v, want omitted with no tools", got["tools"])
	}
}

func TestEncodeRequest_UnknownRoleRejected(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			{Role: message.Role("tool"), Parts: message.Parts{message.TextPart{Text: "x"}}},
		},
	}

	if _, err := encodeRequest(req); err == nil {
		t.Fatal("encodeRequest() error = nil, want error for unknown role")
	}
}

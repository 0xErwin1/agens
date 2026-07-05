package openai

import (
	"encoding/json"
	"testing"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// marshalToMap marshals wire and unmarshals it back into a generic map for
// structural comparison, so tests assert on JSON shape rather than exact
// struct field order or Go-side representation.
func marshalToMap(t *testing.T, wire wireRequest) map[string]any {
	t.Helper()

	data, err := json.Marshal(wire)
	if err != nil {
		t.Fatalf("json.Marshal(wireRequest) error = %v", err)
	}

	var decoded map[string]any
	if err := json.Unmarshal(data, &decoded); err != nil {
		t.Fatalf("json.Unmarshal(wireRequest bytes) error = %v", err)
	}
	return decoded
}

func TestEncodeRequest_SystemMessage(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-4.1",
		Messages: []message.Message{
			message.NewMessage(message.RoleSystem, message.TextPart{Text: "be terse"}, message.TextPart{Text: "no yapping"}),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	messages, ok := got["messages"].([]any)
	if !ok || len(messages) != 1 {
		t.Fatalf("messages = %v, want 1 message", got["messages"])
	}

	msg := messages[0].(map[string]any)
	if msg["role"] != "system" {
		t.Fatalf("messages[0].role = %v, want %q", msg["role"], "system")
	}
	if msg["content"] != "be terse\nno yapping" {
		t.Fatalf("messages[0].content = %v, want %q", msg["content"], "be terse\nno yapping")
	}
	if _, hasToolCalls := msg["tool_calls"]; hasToolCalls {
		t.Fatalf("messages[0] has tool_calls, want none")
	}
}

func TestEncodeRequest_SystemMessageRejectsToolParts(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-4.1",
		Messages: []message.Message{
			message.NewMessage(message.RoleSystem, message.ToolUsePart{ID: "call_1", Name: "x", Input: json.RawMessage(`{}`)}),
		},
	}

	if _, err := encodeRequest(req); err == nil {
		t.Fatal("encodeRequest() error = nil, want error for misplaced tool_use part in system message")
	}
}

func TestEncodeRequest_AssistantTextAndToolUse(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-4.1",
		Messages: []message.Message{
			message.NewMessage(message.RoleAssistant,
				message.TextPart{Text: "let me check"},
				message.ToolUsePart{ID: "call_1", Name: "get_weather", Input: json.RawMessage(`{"loc":"NYC"}`)},
			),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	messages := got["messages"].([]any)
	if len(messages) != 1 {
		t.Fatalf("messages = %v, want 1 message", messages)
	}

	msg := messages[0].(map[string]any)
	if msg["role"] != "assistant" {
		t.Fatalf("messages[0].role = %v, want %q", msg["role"], "assistant")
	}
	if msg["content"] != "let me check" {
		t.Fatalf("messages[0].content = %v, want %q", msg["content"], "let me check")
	}

	toolCalls, ok := msg["tool_calls"].([]any)
	if !ok || len(toolCalls) != 1 {
		t.Fatalf("messages[0].tool_calls = %v, want 1 tool call", msg["tool_calls"])
	}
	tc := toolCalls[0].(map[string]any)
	if tc["id"] != "call_1" || tc["type"] != "function" {
		t.Fatalf("tool_calls[0] = %v, want id=call_1 type=function", tc)
	}
	fn := tc["function"].(map[string]any)
	if fn["name"] != "get_weather" || fn["arguments"] != `{"loc":"NYC"}` {
		t.Fatalf("tool_calls[0].function = %v, want name=get_weather arguments={\"loc\":\"NYC\"}", fn)
	}
}

func TestEncodeRequest_AssistantToolCallsOnlyOmitsContent(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-4.1",
		Messages: []message.Message{
			message.NewMessage(message.RoleAssistant,
				message.ToolUsePart{ID: "call_1", Name: "get_weather", Input: json.RawMessage(`{}`)},
			),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	msg := got["messages"].([]any)[0].(map[string]any)
	if _, hasContent := msg["content"]; hasContent {
		t.Fatalf("messages[0] has content = %v, want omitted", msg["content"])
	}
}

func TestEncodeRequest_AssistantRejectsToolResultPart(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-4.1",
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

func TestEncodeRequest_UserTextOnly(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-4.1",
		Messages: []message.Message{
			message.NewMessage(message.RoleUser, message.TextPart{Text: "hi"}),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	messages := got["messages"].([]any)
	if len(messages) != 1 {
		t.Fatalf("messages = %v, want 1 message", messages)
	}
	msg := messages[0].(map[string]any)
	if msg["role"] != "user" || msg["content"] != "hi" {
		t.Fatalf("messages[0] = %v, want role=user content=hi", msg)
	}
}

func TestEncodeRequest_UserToolResultsOnlyFanOut(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-4.1",
		Messages: []message.Message{
			message.NewMessage(message.RoleUser,
				message.ToolResultPart{ToolUseID: "call_1", Content: message.Parts{message.TextPart{Text: "sunny"}}},
				message.ToolResultPart{ToolUseID: "call_2", Content: message.Parts{message.TextPart{Text: "boom"}}, IsError: true},
			),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	messages := got["messages"].([]any)
	if len(messages) != 2 {
		t.Fatalf("messages = %v, want 2 tool messages", messages)
	}

	first := messages[0].(map[string]any)
	if first["role"] != "tool" || first["tool_call_id"] != "call_1" || first["content"] != "sunny" {
		t.Fatalf("messages[0] = %v, want role=tool tool_call_id=call_1 content=sunny", first)
	}

	second := messages[1].(map[string]any)
	if second["role"] != "tool" || second["tool_call_id"] != "call_2" || second["content"] != "Error: boom" {
		t.Fatalf("messages[1] = %v, want role=tool tool_call_id=call_2 content=%q", second, "Error: boom")
	}
}

func TestEncodeRequest_UserMixedToolResultsAndTextFanOutOrder(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-4.1",
		Messages: []message.Message{
			message.NewMessage(message.RoleUser,
				message.TextPart{Text: "also, "},
				message.ToolResultPart{ToolUseID: "call_1", Content: message.Parts{message.TextPart{Text: "sunny"}}},
				message.TextPart{Text: "thanks"},
			),
		},
	}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	messages := got["messages"].([]any)
	if len(messages) != 2 {
		t.Fatalf("messages = %v, want 2 messages (tool before user)", messages)
	}

	first := messages[0].(map[string]any)
	if first["role"] != "tool" || first["tool_call_id"] != "call_1" {
		t.Fatalf("messages[0] = %v, want the tool message first", first)
	}

	second := messages[1].(map[string]any)
	if second["role"] != "user" || second["content"] != "also, \nthanks" {
		t.Fatalf("messages[1] = %v, want role=user content=%q", second, "also, \nthanks")
	}
}

func TestEncodeRequest_UserRejectsToolUsePart(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-4.1",
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

func TestEncodeRequest_Tools(t *testing.T) {
	req := provider.ChatRequest{
		Model: "gpt-4.1",
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
	if tool["type"] != "function" {
		t.Fatalf("tools[0].type = %v, want function", tool["type"])
	}
	fn := tool["function"].(map[string]any)
	if fn["name"] != "get_weather" || fn["description"] != "fetch weather" {
		t.Fatalf("tools[0].function = %v, want name=get_weather description=%q", fn, "fetch weather")
	}
	params, ok := fn["parameters"].(map[string]any)
	if !ok || params["type"] != "object" {
		t.Fatalf("tools[0].function.parameters = %v, want {type:object}", fn["parameters"])
	}
}

func TestEncodeRequest_ParallelToolCallsEncoded(t *testing.T) {
	for _, tc := range []struct {
		name string
		flag bool
	}{
		{name: "enabled", flag: true},
		{name: "disabled", flag: false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			req := provider.ChatRequest{Model: "gpt-4.1", ParallelToolCalls: tc.flag}

			wire, err := encodeRequest(req)
			if err != nil {
				t.Fatalf("encodeRequest() error = %v", err)
			}

			got := marshalToMap(t, wire)
			if got["parallel_tool_calls"] != tc.flag {
				t.Fatalf("parallel_tool_calls = %v, want %v", got["parallel_tool_calls"], tc.flag)
			}
		})
	}
}

func TestEncodeRequest_TemperatureNilOmitted(t *testing.T) {
	req := provider.ChatRequest{Model: "gpt-4.1"}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	if _, has := got["temperature"]; has {
		t.Fatalf("temperature = %v, want omitted", got["temperature"])
	}
}

func TestEncodeRequest_TemperatureSetIncluded(t *testing.T) {
	temp := 0.7
	req := provider.ChatRequest{Model: "gpt-4.1", Temperature: &temp}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	if got["temperature"] != 0.7 {
		t.Fatalf("temperature = %v, want 0.7", got["temperature"])
	}
}

func TestEncodeRequest_MaxOutputTokensZeroOmitted(t *testing.T) {
	req := provider.ChatRequest{Model: "gpt-4.1"}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	if _, has := got["max_completion_tokens"]; has {
		t.Fatalf("max_completion_tokens = %v, want omitted", got["max_completion_tokens"])
	}
}

func TestEncodeRequest_MaxOutputTokensSetIncluded(t *testing.T) {
	req := provider.ChatRequest{Model: "gpt-4.1", MaxOutputTokens: 128}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	if got["max_completion_tokens"] != float64(128) {
		t.Fatalf("max_completion_tokens = %v, want 128", got["max_completion_tokens"])
	}
}

func TestEncodeRequest_StreamAlwaysSet(t *testing.T) {
	req := provider.ChatRequest{Model: "gpt-4.1"}

	wire, err := encodeRequest(req)
	if err != nil {
		t.Fatalf("encodeRequest() error = %v", err)
	}

	got := marshalToMap(t, wire)
	if got["stream"] != true {
		t.Fatalf("stream = %v, want true", got["stream"])
	}
	streamOptions, ok := got["stream_options"].(map[string]any)
	if !ok || streamOptions["include_usage"] != true {
		t.Fatalf("stream_options = %v, want {include_usage:true}", got["stream_options"])
	}
}

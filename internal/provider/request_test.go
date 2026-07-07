package provider

import (
	"encoding/json"
	"testing"

	"github.com/0xErwin1/agens/internal/message"
)

func TestChatRequestCarriesSystemMessageInMessages(t *testing.T) {
	req := ChatRequest{
		Model: "gpt-5",
		Messages: []message.Message{
			message.NewMessage(message.RoleSystem, message.TextPart{Text: "be terse"}),
			message.NewMessage(message.RoleUser, message.TextPart{Text: "hi"}),
		},
	}

	if len(req.Messages) != 2 {
		t.Fatalf("len(Messages) = %d, want 2", len(req.Messages))
	}
	if req.Messages[0].Role != message.RoleSystem {
		t.Fatalf("Messages[0].Role = %q, want %q", req.Messages[0].Role, message.RoleSystem)
	}
}

func TestChatRequestTemperaturePointerDistinguishesUnsetFromZero(t *testing.T) {
	unset := ChatRequest{}
	if unset.Temperature != nil {
		t.Fatalf("zero-value ChatRequest.Temperature = %v, want nil", unset.Temperature)
	}

	zero := 0.0
	explicit := ChatRequest{Temperature: &zero}
	if explicit.Temperature == nil {
		t.Fatal("ChatRequest.Temperature = nil, want pointer to 0")
	}
	if *explicit.Temperature != 0 {
		t.Fatalf("*ChatRequest.Temperature = %v, want 0", *explicit.Temperature)
	}
}

func TestToolSpecAcceptsRawJSONSchema(t *testing.T) {
	spec := ToolSpec{
		Name:        "search",
		Description: "search the web",
		InputSchema: json.RawMessage(`{"type":"object","properties":{"query":{"type":"string"}}}`),
	}

	var decoded map[string]any
	if err := json.Unmarshal(spec.InputSchema, &decoded); err != nil {
		t.Fatalf("json.Unmarshal(InputSchema) error = %v", err)
	}
	if decoded["type"] != "object" {
		t.Fatalf("InputSchema[type] = %v, want %q", decoded["type"], "object")
	}
}

func TestModelInfoFields(t *testing.T) {
	info := ModelInfo{
		ID:              "gpt-5",
		DisplayName:     "GPT-5",
		ContextWindow:   200000,
		MaxOutputTokens: 8192,
		SupportsTools:   true,
	}

	if info.ID != "gpt-5" || info.DisplayName != "GPT-5" {
		t.Fatalf("ModelInfo = %+v, want ID/DisplayName set", info)
	}
	if info.ContextWindow != 200000 || info.MaxOutputTokens != 8192 {
		t.Fatalf("ModelInfo = %+v, want ContextWindow/MaxOutputTokens set", info)
	}
	if !info.SupportsTools {
		t.Fatalf("ModelInfo.SupportsTools = false, want true")
	}
}

func TestUsageFields(t *testing.T) {
	u := Usage{InputTokens: 12, OutputTokens: 34}

	if u.InputTokens != 12 || u.OutputTokens != 34 {
		t.Fatalf("Usage = %+v, want {InputTokens:12 OutputTokens:34}", u)
	}
}

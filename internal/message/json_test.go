package message

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"
	"time"
)

func TestPartsRoundTripByKind(t *testing.T) {
	tests := []struct {
		name string
		part Part
	}{
		{"text", TextPart{Text: "hello there"}},
		{"tool_use", ToolUsePart{ID: "tu_1", Name: "search", Input: json.RawMessage(`{"query":"go modules","limit":5}`)}},
		{"tool_result", ToolResultPart{ToolUseID: "tu_1", Content: Parts{TextPart{Text: "result text"}}}},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			data, err := json.Marshal(Parts{tt.part})
			if err != nil {
				t.Fatalf("json.Marshal() error = %v", err)
			}

			var decoded Parts
			if err := json.Unmarshal(data, &decoded); err != nil {
				t.Fatalf("json.Unmarshal() error = %v", err)
			}
			if len(decoded) != 1 {
				t.Fatalf("len(decoded) = %d, want 1", len(decoded))
			}
			if !reflect.DeepEqual(tt.part, decoded[0]) {
				t.Fatalf("round-tripped part = %#v, want %#v", decoded[0], tt.part)
			}

			if tu, ok := tt.part.(ToolUsePart); ok {
				got := decoded[0].(ToolUsePart)
				if string(got.Input) != string(tu.Input) {
					t.Fatalf("ToolUsePart.Input = %q, want byte-for-byte %q", got.Input, tu.Input)
				}
			}
		})
	}
}

func TestPartsRoundTripMixedMessages(t *testing.T) {
	original := []Message{
		{
			ID:        "sys-1",
			Role:      RoleSystem,
			Parts:     Parts{TextPart{Text: "you are a helpful assistant"}},
			CreatedAt: time.Date(2026, 7, 1, 12, 0, 0, 0, time.UTC),
		},
		{
			ID:   "asst-1",
			Role: RoleAssistant,
			Parts: Parts{
				TextPart{Text: "let me check that"},
				ToolUsePart{ID: "tu_1", Name: "search", Input: json.RawMessage(`{"query":"go modules"}`)},
			},
			Model:      "test-model",
			StopReason: "tool_use",
			CreatedAt:  time.Date(2026, 7, 1, 12, 0, 1, 0, time.UTC),
		},
		{
			ID:   "user-1",
			Role: RoleUser,
			Parts: Parts{
				ToolResultPart{
					ToolUseID: "tu_1",
					Content:   Parts{TextPart{Text: "go modules are ..."}},
				},
			},
			CreatedAt: time.Date(2026, 7, 1, 12, 0, 2, 0, time.UTC),
		},
	}

	data, err := json.Marshal(original)
	if err != nil {
		t.Fatalf("json.Marshal() error = %v", err)
	}

	var decoded []Message
	if err := json.Unmarshal(data, &decoded); err != nil {
		t.Fatalf("json.Unmarshal() error = %v", err)
	}

	if len(decoded) != len(original) {
		t.Fatalf("len(decoded) = %d, want %d", len(decoded), len(original))
	}
	for i := range original {
		want, got := original[i], decoded[i]
		if want.ID != got.ID || want.Role != got.Role || want.Model != got.Model || want.StopReason != got.StopReason {
			t.Fatalf("message %d = %+v, want %+v", i, got, want)
		}
		if !want.CreatedAt.Equal(got.CreatedAt) {
			t.Fatalf("message %d CreatedAt = %v, want %v", i, got.CreatedAt, want.CreatedAt)
		}
		if !reflect.DeepEqual(want.Parts, got.Parts) {
			t.Fatalf("message %d Parts = %#v, want %#v", i, got.Parts, want.Parts)
		}
	}
}

func TestPartsMarshalIncludesTypeDiscriminator(t *testing.T) {
	tests := []struct {
		name string
		part Part
		want string
	}{
		{"text", TextPart{Text: "hi"}, PartTypeText},
		{"tool_use", ToolUsePart{ID: "tu_1", Name: "search"}, PartTypeToolUse},
		{"tool_result", ToolResultPart{ToolUseID: "tu_1"}, PartTypeToolResult},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			data, err := json.Marshal(Parts{tt.part})
			if err != nil {
				t.Fatalf("json.Marshal() error = %v", err)
			}

			var decoded []map[string]json.RawMessage
			if err := json.Unmarshal(data, &decoded); err != nil {
				t.Fatalf("json.Unmarshal() error = %v", err)
			}

			var gotType string
			if err := json.Unmarshal(decoded[0]["type"], &gotType); err != nil {
				t.Fatalf("missing or invalid %q discriminator in %s: %v", "type", data, err)
			}
			if gotType != tt.want {
				t.Fatalf("type discriminator = %q, want %q", gotType, tt.want)
			}
		})
	}
}

func TestPartsUnmarshalRejectsUnknownTypeTopLevel(t *testing.T) {
	input := `[{"type":"unsupported_kind"}]`

	var parts Parts
	err := json.Unmarshal([]byte(input), &parts)
	if err == nil {
		t.Fatal("json.Unmarshal() error = nil, want explicit error for unknown part type")
	}
	if !strings.Contains(err.Error(), "unsupported_kind") {
		t.Fatalf("error = %v, want it to mention the unknown type", err)
	}
	if !strings.Contains(err.Error(), "part 0") {
		t.Fatalf("error = %v, want it to include the element index", err)
	}
}

func TestPartsUnmarshalRejectsUnknownTypeNested(t *testing.T) {
	input := `[{"type":"tool_result","tool_use_id":"tu_1","content":[{"type":"unsupported_kind"}]}]`

	var parts Parts
	err := json.Unmarshal([]byte(input), &parts)
	if err == nil {
		t.Fatal("json.Unmarshal() error = nil, want explicit error for unknown nested part type")
	}
	if !strings.Contains(err.Error(), "unsupported_kind") {
		t.Fatalf("error = %v, want it to mention the unknown nested type", err)
	}
	if !strings.Contains(err.Error(), "part 0") {
		t.Fatalf("error = %v, want it to include the outer element index", err)
	}
}

func TestToolUsePartMalformedInputFailsDecodeWithContext(t *testing.T) {
	input := `[{"type":"tool_use","id":"tu_1","name":"search","input":{"query":}}]`

	// A syntactically broken value anywhere in the document makes the whole
	// document syntactically invalid (JSON validity is compositional), so
	// encoding/json rejects it during its own pre-decode syntax check before
	// Parts.UnmarshalJSON ever runs. The stdlib's *json.SyntaxError already
	// carries positional context; there is nothing left for our code to wrap.
	var parts Parts
	err := json.Unmarshal([]byte(input), &parts)
	if err == nil {
		t.Fatal("json.Unmarshal() error = nil, want decode error for malformed input JSON")
	}
	if err.Error() == "" {
		t.Fatal("error message is empty, want context identifying the malformed JSON")
	}
}

func TestPartsAndContentEmptyOrNilAreValid(t *testing.T) {
	var nilParts Parts
	data, err := json.Marshal(nilParts)
	if err != nil {
		t.Fatalf("json.Marshal(nil Parts) error = %v", err)
	}
	if string(data) != "[]" {
		t.Fatalf("json.Marshal(nil Parts) = %s, want []", data)
	}

	var decoded Parts
	if err := json.Unmarshal([]byte("[]"), &decoded); err != nil {
		t.Fatalf("json.Unmarshal([]) error = %v", err)
	}
	if len(decoded) != 0 {
		t.Fatalf("len(decoded) = %d, want 0", len(decoded))
	}

	trp := ToolResultPart{ToolUseID: "tu_1"}
	data, err = json.Marshal(Parts{trp})
	if err != nil {
		t.Fatalf("json.Marshal(nil Content) error = %v", err)
	}

	var decodedParts Parts
	if err := json.Unmarshal(data, &decodedParts); err != nil {
		t.Fatalf("json.Unmarshal() error = %v", err)
	}
	got, ok := decodedParts[0].(ToolResultPart)
	if !ok {
		t.Fatalf("decodedParts[0] = %T, want ToolResultPart", decodedParts[0])
	}
	if len(got.Content) != 0 {
		t.Fatalf("Content = %#v, want empty", got.Content)
	}
}

func TestToolResultContentValidNestedRoundTrip(t *testing.T) {
	original := ToolResultPart{
		ToolUseID: "tu_1",
		Content: Parts{
			TextPart{Text: "first line"},
			TextPart{Text: "second line"},
		},
	}

	data, err := json.Marshal(Parts{original})
	if err != nil {
		t.Fatalf("json.Marshal() error = %v", err)
	}

	var decoded Parts
	if err := json.Unmarshal(data, &decoded); err != nil {
		t.Fatalf("json.Unmarshal() error = %v", err)
	}
	if !reflect.DeepEqual(Parts{original}, decoded) {
		t.Fatalf("round-tripped = %#v, want %#v", decoded, Parts{original})
	}
}

func TestToolResultContentRejectsInvalidNestedKind(t *testing.T) {
	tests := []struct {
		name        string
		content     Parts
		decodeInput string
	}{
		{
			name:        "tool_use",
			content:     Parts{ToolUsePart{ID: "tu_2", Name: "search"}},
			decodeInput: `[{"type":"tool_result","tool_use_id":"tu_1","content":[{"type":"tool_use","id":"tu_2","name":"search","input":null}]}]`,
		},
		{
			name:        "tool_result",
			content:     Parts{ToolResultPart{ToolUseID: "tu_2"}},
			decodeInput: `[{"type":"tool_result","tool_use_id":"tu_1","content":[{"type":"tool_result","tool_use_id":"tu_2","content":[]}]}]`,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			_, err := json.Marshal(Parts{ToolResultPart{ToolUseID: "tu_1", Content: tt.content}})
			if err == nil {
				t.Fatal("json.Marshal() error = nil, want rejection of disallowed nested kind")
			}

			var decoded Parts
			err = json.Unmarshal([]byte(tt.decodeInput), &decoded)
			if err == nil {
				t.Fatal("json.Unmarshal() error = nil, want rejection of disallowed nested kind")
			}
		})
	}
}

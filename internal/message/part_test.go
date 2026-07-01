package message

import (
	"encoding/json"
	"testing"
)

func TestPartTypeReturnsKindConstant(t *testing.T) {
	tests := []struct {
		name string
		part Part
		want string
	}{
		{"text", TextPart{Text: "hello"}, PartTypeText},
		{"tool_use", ToolUsePart{ID: "tu_1", Name: "search", Input: json.RawMessage(`{"query":"go"}`)}, PartTypeToolUse},
		{"tool_result", ToolResultPart{ToolUseID: "tu_1", Content: Parts{TextPart{Text: "result"}}}, PartTypeToolResult},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := tt.part.Type(); got != tt.want {
				t.Fatalf("Type() = %q, want %q", got, tt.want)
			}
		})
	}
}

// TestPartExhaustiveTypeSwitch exercises a type-switch over Part covering
// exactly the 3 closed kinds, with an error default. This documents the
// pattern consumers (e.g. the future TUI, AGN-19) rely on: any Part value
// must be one of these 3 concrete types, or the switch reports it as
// unexpected rather than dropping it silently.
func TestPartExhaustiveTypeSwitch(t *testing.T) {
	tests := []struct {
		name string
		part Part
	}{
		{"text", TextPart{Text: "hi"}},
		{"tool_use", ToolUsePart{ID: "tu_1", Name: "search"}},
		{"tool_result", ToolResultPart{ToolUseID: "tu_1"}},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			switch tt.part.(type) {
			case TextPart:
			case ToolUsePart:
			case ToolResultPart:
			default:
				t.Fatalf("Part kind %T not covered by the exhaustive switch over the 3 closed kinds", tt.part)
			}
		})
	}
}

// TestPartIsClosedToExternalImplementations documents the closed-union
// invariant enforced by Part's unexported isPart method: only types declared
// inside this package can satisfy Part, because no external package can
// define a method named isPart on the interface's behalf. This cannot be
// proven by a runtime assertion (the compiler itself rejects any attempt),
// so this test records the invariant via compile-time interface assertions
// for the 3 kinds that do implement it.
func TestPartIsClosedToExternalImplementations(t *testing.T) {
	var _ Part = TextPart{}
	var _ Part = ToolUsePart{}
	var _ Part = ToolResultPart{}
}

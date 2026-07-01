package message

import "encoding/json"

// Part is implemented only by the concrete part kinds declared within this
// package. The unexported isPart method closes the interface so no external
// package can satisfy it, keeping the union exhaustive at compile time.
type Part interface {
	Type() string
	isPart()
}

// Parts is a named slice of Part with a dedicated JSON codec.
type Parts []Part

// The PartType* constants are the wire discriminator values written to and
// read from each Part's "type" field by the Parts JSON codec.
const (
	PartTypeText       = "text"
	PartTypeToolUse    = "tool_use"
	PartTypeToolResult = "tool_result"
)

// TextPart is plain text content.
type TextPart struct {
	Text string `json:"text"`
}

func (TextPart) Type() string { return PartTypeText }

func (TextPart) isPart() {}

// ToolUsePart requests that a tool be invoked. Input carries the tool's
// arguments as raw, provider-defined JSON.
type ToolUsePart struct {
	ID    string          `json:"id"`
	Name  string          `json:"name"`
	Input json.RawMessage `json:"input"`
}

func (ToolUsePart) Type() string { return PartTypeToolUse }

func (ToolUsePart) isPart() {}

// ToolResultPart carries the result of a tool invocation back into the
// conversation. Content is structured/multimodal but restricted to a subset
// of Part kinds (TextPart today); the restriction is enforced by the Parts
// JSON codec, not by this type.
type ToolResultPart struct {
	ToolUseID string `json:"tool_use_id"`
	Content   Parts  `json:"content"`
	IsError   bool   `json:"is_error,omitempty"`
}

func (ToolResultPart) Type() string { return PartTypeToolResult }

func (ToolResultPart) isPart() {}

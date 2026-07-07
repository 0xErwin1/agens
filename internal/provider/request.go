package provider

import (
	"encoding/json"

	"github.com/0xErwin1/agens/internal/message"
)

// ChatRequest describes one streaming chat call. Messages carries the full
// turn history including any system message (message.RoleSystem); where the
// system content lands on the wire is each provider's encoding decision.
type ChatRequest struct {
	Model           string
	Messages        []message.Message
	Tools           []ToolSpec
	MaxOutputTokens int
	Temperature     *float64 // nil = provider default; distinguishes unset from 0

	// ParallelToolCalls asks providers that support same-turn tool calls to
	// let the model emit multiple independent tool calls in one assistant turn.
	ParallelToolCalls bool

	// Effort is the reasoning effort for reasoning-capable models (for
	// example "low", "medium", "high"). Empty means the model's default; a
	// provider that does not support it ignores the field.
	Effort string
}

// ToolSpec declares a tool to the model. Execution and registration are
// owned elsewhere (AGN-11); this is only the wire declaration shape.
type ToolSpec struct {
	Name        string
	Description string
	InputSchema json.RawMessage
}

// ModelInfo is intentionally minimal; AGN-7 (models.dev) extends it
// additively. Never remove or repurpose fields.
type ModelInfo struct {
	ID              string
	DisplayName     string
	ContextWindow   int
	MaxOutputTokens int
	SupportsTools   bool
}

// Usage reports token consumption. Additive-only.
type Usage struct {
	InputTokens  int
	OutputTokens int
}

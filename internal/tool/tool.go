// Package tool provides the uniform Tool contract and Registry the agent
// loop dispatches against. It has no dependency on internal/agentloop in
// production code; *Registry satisfies agentloop.ToolRunner structurally,
// verified only from a test file.
package tool

import (
	"context"
	"encoding/json"

	"github.com/google/jsonschema-go/jsonschema"
)

// Tool is a single capability the model can invoke. Implementations are
// registered with a Registry, which is the only place production code
// dispatches calls from.
type Tool interface {
	// Name is the stable identifier the model uses to request this tool. It
	// must be non-empty and is used as the Registry's lookup key.
	Name() string

	// Description is shown to the model to help it decide when to call this
	// tool.
	Description() string

	// Schema declares the JSON Schema the tool's input must satisfy. A nil
	// Schema means the tool accepts an unconstrained JSON object.
	Schema() *jsonschema.Schema

	// Execute runs the tool with the given raw JSON input and returns its
	// Result. An error return is distinct from Result.IsError: Execute
	// should return an error only for failures the caller must be able to
	// distinguish from a normal tool-level failure (for example ctx
	// cancellation); ordinary tool failures are reported via
	// Result{IsError: true}.
	Execute(ctx context.Context, input json.RawMessage) (Result, error)
}

// Result is the outcome of a tool invocation before it is mapped onto the
// wire representation (message.ToolResultPart). IsError marks a tool-level
// failure, as opposed to a Go error returned from Execute.
type Result struct {
	Text    string
	IsError bool
}

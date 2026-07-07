package tool

import (
	"context"
	"encoding/json"
	"fmt"

	"github.com/google/jsonschema-go/jsonschema"
	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/provider"
)

// fallbackInputSchema is the InputSchema cached for a Tool whose Schema()
// returns nil: an unconstrained-but-valid JSON object schema.
var fallbackInputSchema = json.RawMessage(`{"type":"object"}`)

// Registry is the uniform source of tools the agent loop dispatches
// against. It is build-once/read-only and NOT safe for concurrent use: a
// composition root populates it before serving and treats it as read-only
// afterward, same contract as provider.Registry.
type Registry struct {
	byName map[string]Tool
	order  []string                     // first-registration order; position is preserved on override
	specs  map[string]provider.ToolSpec // cached at Register time, keyed by name
}

// NewRegistry returns an empty, ready-to-use Registry.
func NewRegistry() *Registry {
	return &Registry{
		byName: make(map[string]Tool),
		specs:  make(map[string]provider.ToolSpec),
	}
}

// Register indexes t by t.Name().
//
// Unlike provider.Registry, which errors on a duplicate id, Register is
// last-registered-wins: registering a second Tool under a name already in
// use silently overwrites the first, and Lookup subsequently returns the
// second. This lets a composition root shadow a built-in tool with an
// MCP-provided or plugin-provided one of the same name without special
// casing. Register panics if t is nil or t.Name() is empty, since either is
// a wiring bug that must fail fast at startup.
func (r *Registry) Register(t Tool) {
	if t == nil {
		panic("tool: Register called with a nil Tool")
	}
	name := t.Name()
	if name == "" {
		panic("tool: Register called with an empty Tool.Name()")
	}

	if _, exists := r.byName[name]; !exists {
		r.order = append(r.order, name)
	}
	r.byName[name] = t
	r.specs[name] = provider.ToolSpec{
		Name:        name,
		Description: t.Description(),
		InputSchema: marshalSchema(t.Schema()),
	}
}

// Lookup returns the Tool registered under name, and whether it exists. A
// miss is a normal, recoverable condition (the model requested a name that
// is not registered), not an error.
func (r *Registry) Lookup(name string) (Tool, bool) {
	t, ok := r.byName[name]
	return t, ok
}

// List returns every registered Tool in first-registration order. A
// duplicate registration under an existing name keeps that name's original
// position in the returned order; it does not move it to the end.
func (r *Registry) List() []Tool {
	tools := make([]Tool, len(r.order))
	for i, name := range r.order {
		tools[i] = r.byName[name]
	}
	return tools
}

// Specs returns one provider.ToolSpec per registered Tool, in the same
// first-registration order as List, stable across repeated calls. It is
// pure: every InputSchema was already marshaled and cached by Register, so
// Specs never marshals and never fails.
func (r *Registry) Specs() []provider.ToolSpec {
	specs := make([]provider.ToolSpec, len(r.order))
	for i, name := range r.order {
		specs[i] = r.specs[name]
	}
	return specs
}

// Run dispatches call against the registered Tool named call.Name.
//
// ctx.Err() is checked before dispatch: cancellation is a real error that
// aborts the call before Execute is ever invoked. Once dispatch has
// started, a non-ctx error from Execute becomes an IsError result instead
// (so the model can see the failure and react), while a ctx cancellation
// observed after Execute returns is still surfaced as a real error.
func (r *Registry) Run(ctx context.Context, call message.ToolUsePart) (message.ToolResultPart, error) {
	if err := ctx.Err(); err != nil {
		return message.ToolResultPart{}, err
	}

	t, ok := r.byName[call.Name]
	if !ok {
		return resultToPart(call.ID, Result{
			Text:    fmt.Sprintf("unknown tool: %q", call.Name),
			IsError: true,
		}), nil
	}

	res, err := t.Execute(ctx, call.Input)
	if err != nil {
		if ctx.Err() != nil {
			return message.ToolResultPart{}, ctx.Err()
		}
		return resultToPart(call.ID, Result{Text: err.Error(), IsError: true}), nil
	}
	return resultToPart(call.ID, res), nil
}

// resultToPart maps a Tool's Result onto the message.ToolResultPart wire
// shape, forcing ToolUseID to id regardless of what Execute returned.
func resultToPart(id string, res Result) message.ToolResultPart {
	return message.ToolResultPart{
		ToolUseID: id,
		Content:   message.Parts{message.TextPart{Text: res.Text}},
		IsError:   res.IsError,
	}
}

// marshalSchema returns the InputSchema to cache for schema: fallbackInputSchema
// if schema is nil, otherwise schema marshaled to JSON.
func marshalSchema(schema *jsonschema.Schema) json.RawMessage {
	if schema == nil {
		return fallbackInputSchema
	}

	data, err := json.Marshal(schema)
	if err != nil {
		panic(fmt.Errorf("tool: marshal schema: %w", err))
	}
	return data
}

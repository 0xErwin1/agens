package tool

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/google/jsonschema-go/jsonschema"
	"github.com/0xErwin1/agens/internal/provider"
)

// fakeTool is a Tool double. Name, Description and Schema are plain fields
// for the common static case; execute is a function field so tests can
// script Execute's behavior, including capturing the ctx it receives.
type fakeTool struct {
	name        string
	description string
	schema      *jsonschema.Schema
	execute     func(ctx context.Context, input json.RawMessage) (Result, error)
}

func (f *fakeTool) Name() string { return f.name }

func (f *fakeTool) Description() string { return f.description }

func (f *fakeTool) Schema() *jsonschema.Schema { return f.schema }

func (f *fakeTool) Execute(ctx context.Context, input json.RawMessage) (Result, error) {
	if f.execute == nil {
		return Result{}, nil
	}
	return f.execute(ctx, input)
}

var _ Tool = (*fakeTool)(nil)

func TestRegister_LookupRoundTrip(t *testing.T) {
	r := NewRegistry()
	echo := &fakeTool{name: "echo"}

	r.Register(echo)

	got, ok := r.Lookup("echo")
	if !ok {
		t.Fatalf("Lookup(%q) ok = false, want true", "echo")
	}
	if got != Tool(echo) {
		t.Fatalf("Lookup(%q) = %+v, want %+v", "echo", got, echo)
	}
}

func TestLookup_Missing(t *testing.T) {
	r := NewRegistry()

	got, ok := r.Lookup("missing")
	if ok {
		t.Fatalf("Lookup(%q) ok = true, want false", "missing")
	}
	if got != nil {
		t.Fatalf("Lookup(%q) tool = %+v, want nil", "missing", got)
	}
}

func TestRegister_DuplicateLastWins(t *testing.T) {
	r := NewRegistry()
	a := &fakeTool{name: "write", description: "first"}
	b := &fakeTool{name: "write", description: "second"}

	r.Register(a)
	r.Register(b)

	got, ok := r.Lookup("write")
	if !ok {
		t.Fatalf("Lookup(%q) ok = false, want true", "write")
	}
	if got != Tool(b) {
		t.Fatalf("Lookup(%q) = %+v, want the second registration %+v", "write", got, b)
	}
}

func TestList_DeterministicOrder(t *testing.T) {
	r := NewRegistry()
	grep := &fakeTool{name: "grep"}
	bash := &fakeTool{name: "bash"}
	read := &fakeTool{name: "read"}

	r.Register(grep)
	r.Register(bash)
	r.Register(read)

	wantOrder := []string{"grep", "bash", "read"}

	for i := 0; i < 3; i++ {
		list := r.List()
		if len(list) != len(wantOrder) {
			t.Fatalf("List() = %+v, want %d tools", list, len(wantOrder))
		}
		for idx, name := range wantOrder {
			if list[idx].Name() != name {
				t.Fatalf("List()[%d].Name() = %q, want %q (first-registration order)", idx, list[idx].Name(), name)
			}
		}
	}
}

func TestList_OverridePreservesOriginalPosition(t *testing.T) {
	r := NewRegistry()
	first := &fakeTool{name: "grep", description: "first"}
	second := &fakeTool{name: "bash"}
	overridden := &fakeTool{name: "grep", description: "second"}

	r.Register(first)
	r.Register(second)
	r.Register(overridden)

	list := r.List()
	wantOrder := []string{"grep", "bash"}
	if len(list) != len(wantOrder) {
		t.Fatalf("List() = %+v, want %d tools", list, len(wantOrder))
	}
	for idx, name := range wantOrder {
		if list[idx].Name() != name {
			t.Fatalf("List()[%d].Name() = %q, want %q", idx, list[idx].Name(), name)
		}
	}
	if list[0] != Tool(overridden) {
		t.Fatalf("List()[0] = %+v, want the overriding registration %+v", list[0], overridden)
	}
}

func TestRegister_PanicsOnNilTool(t *testing.T) {
	r := NewRegistry()

	defer func() {
		if recover() == nil {
			t.Fatalf("Register(nil) did not panic, want a panic")
		}
	}()
	r.Register(nil)
}

func TestRegister_PanicsOnEmptyName(t *testing.T) {
	r := NewRegistry()

	defer func() {
		if recover() == nil {
			t.Fatalf("Register(tool with empty Name()) did not panic, want a panic")
		}
	}()
	r.Register(&fakeTool{name: ""})
}

func TestSpecs_NilSchemaFallback(t *testing.T) {
	r := NewRegistry()
	r.Register(&fakeTool{name: "no-schema", schema: nil})

	specs := r.Specs()
	if len(specs) != 1 {
		t.Fatalf("Specs() = %+v, want exactly 1", specs)
	}

	want := json.RawMessage(`{"type":"object"}`)
	if !equalJSON(t, specs[0].InputSchema, want) {
		t.Fatalf("Specs()[0].InputSchema = %s, want %s (fallback for a nil Schema())", specs[0].InputSchema, want)
	}
}

// equalJSON compares two json.RawMessage values by decoded value, not by
// byte-for-byte formatting.
func equalJSON(t *testing.T, a, b json.RawMessage) bool {
	t.Helper()

	var av, bv any
	if err := json.Unmarshal(a, &av); err != nil {
		t.Fatalf("json.Unmarshal(a) error = %v", err)
	}
	if err := json.Unmarshal(b, &bv); err != nil {
		t.Fatalf("json.Unmarshal(b) error = %v", err)
	}

	ab, err := json.Marshal(av)
	if err != nil {
		t.Fatalf("json.Marshal(av) error = %v", err)
	}
	bb, err := json.Marshal(bv)
	if err != nil {
		t.Fatalf("json.Marshal(bv) error = %v", err)
	}
	return string(ab) == string(bb)
}

func TestSpecs_MapsFields(t *testing.T) {
	r := NewRegistry()
	schema := &jsonschema.Schema{Type: "object"}
	r.Register(&fakeTool{name: "ls", description: "list", schema: schema})

	specs := r.Specs()
	if len(specs) != 1 {
		t.Fatalf("Specs() = %+v, want exactly 1", specs)
	}

	want := provider.ToolSpec{
		Name:        "ls",
		Description: "list",
	}
	got := specs[0]
	if got.Name != want.Name {
		t.Fatalf("Specs()[0].Name = %q, want %q", got.Name, want.Name)
	}
	if got.Description != want.Description {
		t.Fatalf("Specs()[0].Description = %q, want %q", got.Description, want.Description)
	}

	wantSchemaJSON, err := json.Marshal(schema)
	if err != nil {
		t.Fatalf("json.Marshal(schema) error = %v", err)
	}
	if !equalJSON(t, got.InputSchema, wantSchemaJSON) {
		t.Fatalf("Specs()[0].InputSchema = %s, want %s (round-trip of Tool.Schema())", got.InputSchema, wantSchemaJSON)
	}
}

func TestSpecs_Empty(t *testing.T) {
	r := NewRegistry()

	specs := r.Specs()
	if len(specs) != 0 {
		t.Fatalf("Specs() = %+v, want empty", specs)
	}
}

func TestSpecs_OrderStable(t *testing.T) {
	r := NewRegistry()
	r.Register(&fakeTool{name: "grep"})
	r.Register(&fakeTool{name: "bash"})
	r.Register(&fakeTool{name: "read"})

	wantOrder := []string{"grep", "bash", "read"}

	for i := 0; i < 3; i++ {
		specs := r.Specs()
		if len(specs) != len(wantOrder) {
			t.Fatalf("Specs() = %+v, want %d specs", specs, len(wantOrder))
		}
		for idx, name := range wantOrder {
			if specs[idx].Name != name {
				t.Fatalf("Specs()[%d].Name = %q, want %q", idx, specs[idx].Name, name)
			}
		}
	}
}

func TestRegister_PanicsOnUnmarshalableSchema(t *testing.T) {
	r := NewRegistry()
	// A Schema with both Type and Types set fails jsonschema.Schema's
	// basicChecks, which MarshalJSON runs before encoding — this is a real,
	// reachable marshal failure, not a synthetic one.
	badSchema := &jsonschema.Schema{Type: "string", Types: []string{"string", "number"}}

	defer func() {
		if recover() == nil {
			t.Fatalf("Register(tool with unmarshalable schema) did not panic, want a panic")
		}
	}()
	r.Register(&fakeTool{name: "bad", schema: badSchema})
}

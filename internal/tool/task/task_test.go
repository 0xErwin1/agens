package task

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/iperez/agens/internal/tool"
)

// fakeRunner is a scripted Runner: it records the request it was asked to run
// and returns the configured output/err pair.
type fakeRunner struct {
	out       string
	err       error
	gotReq    Request
	callCount int
}

func (f *fakeRunner) Run(_ context.Context, req Request) (string, error) {
	f.callCount++
	f.gotReq = req
	return f.out, f.err
}

func TestNew_PanicsOnNilRunner(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatal("New(nil) did not panic, want a panic")
		}
	}()
	New(nil, nil)
}

func TestTask_NameAndSchema(t *testing.T) {
	tl := New(&fakeRunner{}, nil)

	if tl.Name() != "task" {
		t.Fatalf("Name() = %q, want %q", tl.Name(), "task")
	}

	schema := tl.Schema()
	if schema == nil {
		t.Fatal("Schema() = nil, want a non-nil schema")
	}
	data, err := json.Marshal(schema)
	if err != nil {
		t.Fatalf("json.Marshal(schema) error = %v", err)
	}
	var decoded map[string]any
	if err := json.Unmarshal(data, &decoded); err != nil {
		t.Fatalf("json.Unmarshal error = %v", err)
	}
	required, ok := decoded["required"].([]any)
	if !ok || len(required) != 1 || required[0] != "description" {
		t.Fatalf("Schema() required = %v, want [\"description\"]", decoded["required"])
	}
}

func TestTask_ExecuteRunsSubagentAndReturnsResult(t *testing.T) {
	runner := &fakeRunner{out: "the subagent's final report"}
	tl := New(runner, nil)

	result, err := tl.Execute(context.Background(), json.RawMessage(`{"description":"investigate X"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if result.IsError {
		t.Fatalf("Execute() result = %+v, want a successful result", result)
	}
	if result.Text != "the subagent's final report" {
		t.Fatalf("Execute() Text = %q, want the subagent's report", result.Text)
	}
	if runner.gotReq.Description != "investigate X" {
		t.Fatalf("runner got description %q, want %q", runner.gotReq.Description, "investigate X")
	}
}

func TestTask_ExecuteEmptyDescriptionIsToolError(t *testing.T) {
	runner := &fakeRunner{out: "should not run"}
	tl := New(runner, nil)

	result, err := tl.Execute(context.Background(), json.RawMessage(`{"description":"   "}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil (domain failure must be IsError)", err)
	}
	if !result.IsError {
		t.Fatalf("Execute() result = %+v, want IsError for an empty description", result)
	}
	if runner.callCount != 0 {
		t.Fatalf("runner was called %d time(s) for an empty description, want 0", runner.callCount)
	}
}

func TestTask_ExecuteInvalidJSONIsToolError(t *testing.T) {
	result, err := New(&fakeRunner{}, nil).Execute(context.Background(), json.RawMessage(`{"description":`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if !result.IsError {
		t.Fatalf("Execute() result = %+v, want IsError for malformed input", result)
	}
}

func TestTask_ExecuteSubagentFailureIsToolError(t *testing.T) {
	tl := New(&fakeRunner{err: errors.New("provider exploded")}, nil)

	result, err := tl.Execute(context.Background(), json.RawMessage(`{"description":"do it"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil (a subagent failure is a tool-level error, not a Go error)", err)
	}
	if !result.IsError {
		t.Fatalf("Execute() result = %+v, want IsError when the subagent fails", result)
	}
	if !strings.Contains(result.Text, "provider exploded") {
		t.Fatalf("Execute() Text = %q, want it to carry the subagent failure", result.Text)
	}
}

func TestTask_ExecuteCancellationPropagatesAsGoError(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	tl := New(&fakeRunner{err: context.Canceled}, nil)

	_, err := tl.Execute(ctx, json.RawMessage(`{"description":"do it"}`))
	if err == nil {
		t.Fatal("Execute() error = nil, want the cancellation propagated as a Go error so the loop aborts")
	}
}

func TestTask_ExecuteEmptyResultIsNoted(t *testing.T) {
	tl := New(&fakeRunner{out: ""}, nil)

	result, err := tl.Execute(context.Background(), json.RawMessage(`{"description":"do it"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if result.IsError {
		t.Fatalf("Execute() result = %+v, want a non-error result even when the subagent is silent", result)
	}
	if result.Text == "" {
		t.Fatal("Execute() Text = \"\", want a placeholder note for an empty subagent result")
	}
}

// testAgents is a two-agent catalog: an unrestricted "build" and a
// model-restricted "plan", used to exercise the agent/model routing.
func testAgents() []Agent {
	return []Agent{
		{Name: "build", Description: "hands-on", Models: nil},
		{Name: "plan", Description: "read-only", Models: []string{"gpt-5.5", "gpt-4.1"}},
	}
}

func TestTask_SchemaAdvertisesAgentAndModelWhenOffered(t *testing.T) {
	schema := New(&fakeRunner{}, NewCatalog(testAgents())).Schema()

	data, err := json.Marshal(schema)
	if err != nil {
		t.Fatalf("json.Marshal(schema) error = %v", err)
	}
	var decoded struct {
		Properties map[string]struct {
			Enum []any `json:"enum"`
		} `json:"properties"`
	}
	if err := json.Unmarshal(data, &decoded); err != nil {
		t.Fatalf("json.Unmarshal error = %v", err)
	}

	agent, ok := decoded.Properties["agent"]
	if !ok {
		t.Fatal("Schema() has no agent property, want it when agents are offered")
	}
	if len(agent.Enum) != 2 || agent.Enum[0] != "build" || agent.Enum[1] != "plan" {
		t.Fatalf("agent enum = %v, want [build plan]", agent.Enum)
	}
	if _, ok := decoded.Properties["model"]; !ok {
		t.Fatal("Schema() has no model property, want it when agents are offered")
	}
}

func TestTask_ExecuteRoutesAgentAndModel(t *testing.T) {
	runner := &fakeRunner{out: "ok"}
	tl := New(runner, NewCatalog(testAgents()))

	_, err := tl.Execute(context.Background(), json.RawMessage(`{"description":"do it","agent":"plan","model":"gpt-4.1"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if runner.gotReq.Agent != "plan" || runner.gotReq.Model != "gpt-4.1" {
		t.Fatalf("runner got %+v, want agent plan model gpt-4.1", runner.gotReq)
	}
}

func TestTask_ExecuteDefaultsAgentToFirstWhenOmitted(t *testing.T) {
	runner := &fakeRunner{out: "ok"}
	tl := New(runner, NewCatalog(testAgents()))

	if _, err := tl.Execute(context.Background(), json.RawMessage(`{"description":"do it"}`)); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if runner.gotReq.Agent != "build" {
		t.Fatalf("runner got agent %q, want the first agent as the default", runner.gotReq.Agent)
	}
}

func TestTask_ExecuteUnknownAgentIsToolError(t *testing.T) {
	runner := &fakeRunner{out: "should not run"}
	tl := New(runner, NewCatalog(testAgents()))

	result, err := tl.Execute(context.Background(), json.RawMessage(`{"description":"do it","agent":"ghost"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if !result.IsError {
		t.Fatalf("Execute() result = %+v, want IsError for an unknown agent", result)
	}
	if runner.callCount != 0 {
		t.Fatalf("runner ran %d time(s) for an unknown agent, want 0", runner.callCount)
	}
}

func TestTask_ExecuteModelNotAllowedIsToolError(t *testing.T) {
	runner := &fakeRunner{out: "should not run"}
	tl := New(runner, NewCatalog(testAgents()))

	result, err := tl.Execute(context.Background(), json.RawMessage(`{"description":"do it","agent":"plan","model":"gpt-5-codex"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if !result.IsError {
		t.Fatalf("Execute() result = %+v, want IsError for a model outside the agent's allowed set", result)
	}
	if runner.callCount != 0 {
		t.Fatalf("runner ran %d time(s) for a disallowed model, want 0", runner.callCount)
	}
}

func TestTask_CatalogEditTakesEffectOnNextCall(t *testing.T) {
	runner := &fakeRunner{out: "ok"}
	catalog := NewCatalog(testAgents())
	tl := New(runner, catalog)

	// "build" starts unrestricted, so any model is accepted.
	if _, err := tl.Execute(context.Background(), json.RawMessage(`{"description":"x","agent":"build","model":"gpt-5-codex"}`)); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if runner.gotReq.Model != "gpt-5-codex" {
		t.Fatalf("first call model = %q, want it accepted while build is unrestricted", runner.gotReq.Model)
	}

	// Restrict build to a different model; the same delegation is now rejected.
	catalog.SetModels("build", []string{"gpt-5.5"})

	runner.callCount = 0
	result, err := tl.Execute(context.Background(), json.RawMessage(`{"description":"x","agent":"build","model":"gpt-5-codex"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if !result.IsError {
		t.Fatalf("Execute() result = %+v, want IsError once the catalog restricts build", result)
	}
	if runner.callCount != 0 {
		t.Fatalf("runner ran %d time(s) after the model was disallowed live, want 0", runner.callCount)
	}
}

var _ tool.Tool = (*Tool)(nil)

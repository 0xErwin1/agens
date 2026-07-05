package task

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/iperez/agens/internal/tool"
)

// fakeRunner is a scripted Runner: it records the description it was asked to
// run and returns the configured output/err pair.
type fakeRunner struct {
	out       string
	err       error
	gotDesc   string
	callCount int
}

func (f *fakeRunner) Run(_ context.Context, description string) (string, error) {
	f.callCount++
	f.gotDesc = description
	return f.out, f.err
}

func TestNew_PanicsOnNilRunner(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatal("New(nil) did not panic, want a panic")
		}
	}()
	New(nil)
}

func TestTask_NameAndSchema(t *testing.T) {
	tl := New(&fakeRunner{})

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
	tl := New(runner)

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
	if runner.gotDesc != "investigate X" {
		t.Fatalf("runner got description %q, want %q", runner.gotDesc, "investigate X")
	}
}

func TestTask_ExecuteEmptyDescriptionIsToolError(t *testing.T) {
	runner := &fakeRunner{out: "should not run"}
	tl := New(runner)

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
	result, err := New(&fakeRunner{}).Execute(context.Background(), json.RawMessage(`{"description":`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if !result.IsError {
		t.Fatalf("Execute() result = %+v, want IsError for malformed input", result)
	}
}

func TestTask_ExecuteSubagentFailureIsToolError(t *testing.T) {
	tl := New(&fakeRunner{err: errors.New("provider exploded")})

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

	tl := New(&fakeRunner{err: context.Canceled})

	_, err := tl.Execute(ctx, json.RawMessage(`{"description":"do it"}`))
	if err == nil {
		t.Fatal("Execute() error = nil, want the cancellation propagated as a Go error so the loop aborts")
	}
}

func TestTask_ExecuteEmptyResultIsNoted(t *testing.T) {
	tl := New(&fakeRunner{out: ""})

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

var _ tool.Tool = (*Tool)(nil)

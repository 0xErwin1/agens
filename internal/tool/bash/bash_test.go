package bash

import (
	"context"
	"encoding/json"
	"path/filepath"
	"strings"
	"testing"
)

func TestNew_PanicsOnEmptyProjectRoot(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatalf(`New("") did not panic, want a panic`)
		}
	}()
	New("")
}

func TestBash_Name(t *testing.T) {
	b := New(t.TempDir())
	if got := b.Name(); got != "bash" {
		t.Fatalf("Name() = %q, want %q", got, "bash")
	}
}

func TestBash_Description(t *testing.T) {
	b := New(t.TempDir())
	desc := b.Description()
	lower := strings.ToLower(desc)

	for _, want := range []string{"bash -c", "project root", "combined", "120", "not sandboxed", "ask"} {
		if !strings.Contains(lower, want) {
			t.Fatalf("Description() = %q, want it to mention %q", desc, want)
		}
	}
}

func TestBash_Schema(t *testing.T) {
	b := New(t.TempDir())
	schema := b.Schema()
	if schema == nil {
		t.Fatalf("Schema() = nil, want non-nil")
	}
	if schema.Type != "object" {
		t.Fatalf("Schema().Type = %q, want %q", schema.Type, "object")
	}
	if _, ok := schema.Properties["command"]; !ok {
		t.Fatalf("Schema().Properties missing %q", "command")
	}
	if _, ok := schema.Properties["timeout_seconds"]; !ok {
		t.Fatalf("Schema().Properties missing %q", "timeout_seconds")
	}
	if len(schema.Required) != 1 || schema.Required[0] != "command" {
		t.Fatalf("Schema().Required = %v, want [%q]", schema.Required, "command")
	}
}

func TestBash_Execute_InputValidation(t *testing.T) {
	b := New(t.TempDir())

	tests := []struct {
		name  string
		input string
	}{
		{name: "invalid JSON", input: `{"command":`},
		{name: "missing command", input: `{}`},
		{name: "empty command", input: `{"command":""}`},
		{name: "whitespace-only command", input: `{"command":"   "}`},
		{name: "negative timeout", input: `{"command":"echo hi","timeout_seconds":-1}`},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			res, err := b.Execute(context.Background(), json.RawMessage(tt.input))
			if err != nil {
				t.Fatalf("Execute() error = %v, want nil (domain failures must be IsError, not a Go error)", err)
			}
			if !res.IsError {
				t.Fatalf("Execute(%s) IsError = false, want true (Text = %q)", tt.input, res.Text)
			}
		})
	}
}

func TestBash_Execute_HappyPath(t *testing.T) {
	b := New(t.TempDir())

	res, err := b.Execute(context.Background(), json.RawMessage(`{"command":"echo hello"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if res.IsError {
		t.Fatalf("Execute() IsError = true, want false (Text = %q)", res.Text)
	}
	if !strings.Contains(res.Text, "hello") {
		t.Fatalf("Execute() Text = %q, want it to contain %q", res.Text, "hello")
	}
}

func TestBash_Execute_NonZeroExit(t *testing.T) {
	b := New(t.TempDir())

	res, err := b.Execute(context.Background(), json.RawMessage(`{"command":"echo before-exit; exit 3"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if !res.IsError {
		t.Fatalf("Execute() IsError = false, want true (Text = %q)", res.Text)
	}
	if !strings.Contains(res.Text, "before-exit") {
		t.Fatalf("Execute() Text = %q, want it to still contain the produced output", res.Text)
	}
	if !strings.Contains(res.Text, "exit status 3") {
		t.Fatalf("Execute() Text = %q, want it to contain %q", res.Text, "exit status 3")
	}
}

func TestBash_Execute_CombinedStreams(t *testing.T) {
	b := New(t.TempDir())

	res, err := b.Execute(context.Background(), json.RawMessage(`{"command":"echo out; echo err >&2"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if res.IsError {
		t.Fatalf("Execute() IsError = true, want false (Text = %q)", res.Text)
	}
	if !strings.Contains(res.Text, "out") {
		t.Fatalf("Execute() Text = %q, want it to contain stdout output %q", res.Text, "out")
	}
	if !strings.Contains(res.Text, "err") {
		t.Fatalf("Execute() Text = %q, want it to contain stderr output %q", res.Text, "err")
	}
}

func TestBash_Execute_Cwd(t *testing.T) {
	root := t.TempDir()
	want, err := filepath.EvalSymlinks(root)
	if err != nil {
		t.Fatalf("filepath.EvalSymlinks(%q) error = %v", root, err)
	}

	b := New(root)
	res, err := b.Execute(context.Background(), json.RawMessage(`{"command":"pwd"}`))
	if err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if res.IsError {
		t.Fatalf("Execute() IsError = true, want false (Text = %q)", res.Text)
	}
	if !strings.Contains(res.Text, want) {
		t.Fatalf("Execute() Text = %q, want it to contain cwd %q", res.Text, want)
	}
}

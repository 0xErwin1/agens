package agent

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/iperez/agens/internal/auth"
	"github.com/iperez/agens/internal/config"
	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/permission"
)

func validConfig() config.Config {
	cfg := config.DefaultConfig()
	cfg.Provider.Model = "gpt-4.1"
	cfg.Agent.SystemPrompt = "be helpful"
	return cfg
}

func validCreds() auth.File {
	return auth.File{
		defaultProviderID: {APIKey: "sk-test-key"},
	}
}

// validOptions returns an Options with a fresh, isolated ProjectRoot so
// every BuildLoop/buildGate call in this file opens a real confinement
// root instead of falling back to the test binary's working directory.
func validOptions(t *testing.T) Options {
	t.Helper()
	return Options{ProjectRoot: t.TempDir()}
}

func TestBuildLoop_Success(t *testing.T) {
	loop, err := BuildLoop(validConfig(), validCreds(), validOptions(t))
	if err != nil {
		t.Fatalf("BuildLoop() error = %v, want nil", err)
	}
	if loop == nil {
		t.Fatal("BuildLoop() loop = nil, want non-nil")
	}
}

func TestBuildLoop_ModelPrecedence(t *testing.T) {
	tests := []struct {
		name      string
		optsModel string
		cfgModel  string
		wantErr   bool
	}{
		{name: "opts overrides cfg", optsModel: "opt-model", cfgModel: "cfg-model", wantErr: false},
		{name: "falls back to cfg when opts empty", optsModel: "", cfgModel: "cfg-model", wantErr: false},
		{name: "errors when both empty", optsModel: "", cfgModel: "", wantErr: true},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := validConfig()
			cfg.Provider.Model = tt.cfgModel

			opts := validOptions(t)
			opts.Model = tt.optsModel
			loop, err := BuildLoop(cfg, validCreds(), opts)

			if tt.wantErr {
				if err == nil {
					t.Fatal("BuildLoop() error = nil, want an error for an empty resolved model")
				}
				if !strings.Contains(err.Error(), "no model configured") {
					t.Fatalf("BuildLoop() error = %q, want it to mention %q", err.Error(), "no model configured")
				}
				return
			}
			if err != nil {
				t.Fatalf("BuildLoop() error = %v, want nil", err)
			}
			if loop == nil {
				t.Fatal("BuildLoop() loop = nil, want non-nil")
			}
		})
	}
}

func TestBuildLoop_SystemPromptPrecedence(t *testing.T) {
	tests := []struct {
		name    string
		optsSys string
		cfgSys  string
	}{
		{name: "opts overrides cfg", optsSys: "opts prompt", cfgSys: "cfg prompt"},
		{name: "falls back to cfg when opts empty", optsSys: "", cfgSys: "cfg prompt"},
		{name: "falls back to built-in default when both empty", optsSys: "", cfgSys: ""},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := validConfig()
			cfg.Agent.SystemPrompt = tt.cfgSys

			opts := validOptions(t)
			opts.SystemPrompt = tt.optsSys
			loop, err := BuildLoop(cfg, validCreds(), opts)
			if err != nil {
				t.Fatalf("BuildLoop() error = %v, want nil", err)
			}
			if loop == nil {
				t.Fatal("BuildLoop() loop = nil, want non-nil")
			}
		})
	}
}

func TestBuildLoop_MissingAPIKeyErrors(t *testing.T) {
	creds := auth.File{}

	_, err := BuildLoop(validConfig(), creds, validOptions(t))
	if err == nil {
		t.Fatal("BuildLoop() error = nil, want an error for missing credentials")
	}
	if !strings.Contains(err.Error(), defaultProviderID) {
		t.Fatalf("BuildLoop() error = %q, want it to name provider %q", err.Error(), defaultProviderID)
	}
}

func TestBuildLoop_EmptyAPIKeyErrors(t *testing.T) {
	creds := auth.File{
		defaultProviderID: {APIKey: ""},
	}

	_, err := BuildLoop(validConfig(), creds, validOptions(t))
	if err == nil {
		t.Fatal("BuildLoop() error = nil, want an error for an empty api_key")
	}
	if !strings.Contains(err.Error(), defaultProviderID) {
		t.Fatalf("BuildLoop() error = %q, want it to name provider %q", err.Error(), defaultProviderID)
	}
}

func TestBuildLoop_ErrorsNeverLeakAPIKeyValue(t *testing.T) {
	const secret = "sk-super-secret-value"
	creds := auth.File{
		"other-provider": {APIKey: secret},
	}

	_, err := BuildLoop(validConfig(), creds, validOptions(t))
	if err == nil {
		t.Fatal("BuildLoop() error = nil, want an error since openai-api has no credentials")
	}
	if strings.Contains(err.Error(), secret) {
		t.Fatalf("BuildLoop() error = %q, must never contain a raw api_key value", err.Error())
	}
}

// fakePrompter records every call it is asked to resolve and always
// answers with the configured answer/err pair, mirroring the scripted
// fakes used across the permission package's own tests.
type fakePrompter struct {
	answer permission.Answer
	err    error
	calls  []message.ToolUsePart
}

func (f *fakePrompter) Prompt(_ context.Context, call message.ToolUsePart) (permission.Answer, error) {
	f.calls = append(f.calls, call)
	return f.answer, f.err
}

func TestBuildGate_RegistersReadWriteEditWithSchemas(t *testing.T) {
	gate, err := buildGate(validOptions(t))
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	specs := gate.Specs()
	byName := make(map[string]bool, len(specs))
	for _, s := range specs {
		if len(s.InputSchema) == 0 {
			t.Fatalf("tool %q has an empty InputSchema, want a non-nil JSON Schema", s.Name)
		}
		byName[s.Name] = true
	}

	for _, want := range []string{"read", "write", "edit", "bash", "grep", "glob"} {
		if !byName[want] {
			t.Fatalf("gate.Specs() = %+v, want it to include tool %q", specs, want)
		}
	}
}

func TestBuildGate_ReadIsAllowedWithoutPrompting(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "a.txt"), []byte("hello"), 0o644); err != nil {
		t.Fatalf("seed file: %v", err)
	}

	fp := &fakePrompter{answer: permission.AnswerDenyOnce}
	gate, err := buildGate(Options{ProjectRoot: dir, Prompter: fp})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	call := message.ToolUsePart{ID: "tu_1", Name: "read", Input: json.RawMessage(`{"path":"a.txt"}`)}
	result, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("gate.Run(read) error = %v, want nil", err)
	}
	if result.IsError {
		t.Fatalf("gate.Run(read) result = %+v, want IsError == false", result)
	}
	if len(fp.calls) != 0 {
		t.Fatalf("prompter was consulted %d time(s) for a read call, want 0 (read is pre-seeded Allow)", len(fp.calls))
	}
}

// failOnCallPrompter fails the test immediately if Prompt is invoked. It
// proves a call resolved to Allow without ever consulting the Prompter.
type failOnCallPrompter struct {
	t *testing.T
}

func (f failOnCallPrompter) Prompt(_ context.Context, call message.ToolUsePart) (permission.Answer, error) {
	f.t.Fatalf("Prompter.Prompt was called for tool %q, want it to resolve Allow without prompting", call.Name)
	return permission.AnswerDenyOnce, nil
}

func TestBuildGate_GrepAndGlobAreAllowedWithoutPrompting(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "a.go"), []byte("package a\n\nconst needle = 1\n"), 0o644); err != nil {
		t.Fatalf("seed file: %v", err)
	}

	gate, err := buildGate(Options{ProjectRoot: dir, Prompter: failOnCallPrompter{t: t}})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	grepCall := message.ToolUsePart{ID: "tu_1", Name: "grep", Input: json.RawMessage(`{"pattern":"needle"}`)}
	result, err := gate.Run(context.Background(), grepCall)
	if err != nil {
		t.Fatalf("gate.Run(grep) error = %v, want nil", err)
	}
	if result.IsError {
		t.Fatalf("gate.Run(grep) result = %+v, want IsError == false", result)
	}

	globCall := message.ToolUsePart{ID: "tu_2", Name: "glob", Input: json.RawMessage(`{"pattern":"*.go"}`)}
	result, err = gate.Run(context.Background(), globCall)
	if err != nil {
		t.Fatalf("gate.Run(glob) error = %v, want nil", err)
	}
	if result.IsError {
		t.Fatalf("gate.Run(glob) result = %+v, want IsError == false", result)
	}
}

func TestBuildGate_WriteAskConsultsPrompter_AllowOnceExecutes(t *testing.T) {
	dir := t.TempDir()
	fp := &fakePrompter{answer: permission.AnswerAllowOnce}
	gate, err := buildGate(Options{ProjectRoot: dir, Prompter: fp})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	call := message.ToolUsePart{ID: "tu_1", Name: "write", Input: json.RawMessage(`{"path":"b.txt","content":"hi"}`)}
	result, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("gate.Run(write) error = %v, want nil", err)
	}
	if result.IsError {
		t.Fatalf("gate.Run(write) result = %+v, want IsError == false for an allow-once answer", result)
	}
	if len(fp.calls) != 1 {
		t.Fatalf("prompter was consulted %d time(s) for a write call, want exactly 1", len(fp.calls))
	}

	data, err := os.ReadFile(filepath.Join(dir, "b.txt"))
	if err != nil {
		t.Fatalf("read back written file: %v", err)
	}
	if string(data) != "hi" {
		t.Fatalf("written file content = %q, want %q", string(data), "hi")
	}
}

func TestBuildGate_WriteAskConsultsPrompter_DenyOnceDenies(t *testing.T) {
	dir := t.TempDir()
	fp := &fakePrompter{answer: permission.AnswerDenyOnce}
	gate, err := buildGate(Options{ProjectRoot: dir, Prompter: fp})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	call := message.ToolUsePart{ID: "tu_1", Name: "write", Input: json.RawMessage(`{"path":"c.txt","content":"hi"}`)}
	result, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("gate.Run(write) error = %v, want nil", err)
	}
	if !result.IsError {
		t.Fatalf("gate.Run(write) result = %+v, want IsError == true for a deny-once answer", result)
	}
	if len(fp.calls) != 1 {
		t.Fatalf("prompter was consulted %d time(s) for a write call, want exactly 1", len(fp.calls))
	}
	if _, statErr := os.Stat(filepath.Join(dir, "c.txt")); !os.IsNotExist(statErr) {
		t.Fatalf("Stat(c.txt) error = %v, want a not-exist error since the write must not have executed", statErr)
	}
}

func TestBuildGate_NilPrompterDefaultsToDenyPrompter(t *testing.T) {
	dir := t.TempDir()
	gate, err := buildGate(Options{ProjectRoot: dir})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	call := message.ToolUsePart{ID: "tu_1", Name: "write", Input: json.RawMessage(`{"path":"d.txt","content":"hi"}`)}
	result, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("gate.Run(write) error = %v, want nil", err)
	}
	if !result.IsError {
		t.Fatalf("gate.Run(write) result = %+v, want IsError == true when Options.Prompter is nil", result)
	}
}

func TestBuildGate_BashAskConsultsPrompter_DenyOnceDenies(t *testing.T) {
	dir := t.TempDir()
	fp := &fakePrompter{answer: permission.AnswerDenyOnce}
	gate, err := buildGate(Options{ProjectRoot: dir, Prompter: fp})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	call := message.ToolUsePart{ID: "tu_1", Name: "bash", Input: json.RawMessage(`{"command":"echo hi"}`)}
	result, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("gate.Run(bash) error = %v, want nil", err)
	}
	if !result.IsError {
		t.Fatalf("gate.Run(bash) result = %+v, want IsError == true for a deny-once answer", result)
	}
	if len(fp.calls) != 1 {
		t.Fatalf("prompter was consulted %d time(s) for a bash call, want exactly 1 (bash must resolve to Ask, no seeded rule)", len(fp.calls))
	}
}

func TestBuildGate_EmptyProjectRootFallsBackToWorkingDir(t *testing.T) {
	gate, err := buildGate(Options{})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}
	if gate == nil {
		t.Fatal("buildGate() gate = nil, want non-nil")
	}
}

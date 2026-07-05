package agent

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/auth"
	"github.com/iperez/agens/internal/config"
	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/permission"
	"github.com/iperez/agens/internal/provider"
	"github.com/iperez/agens/internal/provider/chatgpt"
	"github.com/iperez/agens/internal/provider/openai"
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

// chatgptCreds returns an auth.File with a well-formed ChatGPT OAuth entry:
// a non-expired access token plus a refresh token, which is what
// selectProviderID requires to infer the chatgpt provider.
func chatgptCreds() auth.File {
	expiresAt := time.Now().Add(time.Hour)
	return auth.File{
		chatgptProviderID: {
			AccessToken:  "access-token",
			RefreshToken: "refresh-token",
			AccountID:    "acct_123",
			ExpiresAt:    &expiresAt,
		},
	}
}

// validOptions returns an Options with a fresh, isolated ProjectRoot so
// every BuildLoop/buildGate call in this file opens a real confinement
// root instead of falling back to the test binary's working directory.
func validOptions(t *testing.T) Options {
	t.Helper()
	return Options{ProjectRoot: t.TempDir()}
}

func loopMaxIterations(t *testing.T, loop any) int {
	t.Helper()
	value := reflect.ValueOf(loop)
	if value.Kind() != reflect.Pointer || value.IsNil() {
		t.Fatalf("loop = %T, want non-nil pointer", loop)
	}
	field := value.Elem().FieldByName("maxIter")
	if !field.IsValid() {
		t.Fatal("loop.maxIter field not found")
	}
	return int(field.Int())
}

func loopParallelToolCalls(t *testing.T, loop any) bool {
	t.Helper()
	value := reflect.ValueOf(loop)
	if value.Kind() != reflect.Pointer || value.IsNil() {
		t.Fatalf("loop = %T, want non-nil pointer", loop)
	}
	field := value.Elem().FieldByName("parallelToolCalls")
	if !field.IsValid() {
		t.Fatal("loop.parallelToolCalls field not found")
	}
	return field.Bool()
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
		{name: "falls back to provider default when both empty", optsModel: "", cfgModel: "", wantErr: false},
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

func TestBuildLoop_ParallelToolCallsComesFromConfig(t *testing.T) {
	cfg := validConfig()
	cfg.Agent.ParallelToolCalls = false

	loop, err := BuildLoop(cfg, validCreds(), validOptions(t))
	if err != nil {
		t.Fatalf("BuildLoop() error = %v, want nil", err)
	}
	if loopParallelToolCalls(t, loop) {
		t.Fatalf("loop parallelToolCalls = true, want false from config rollback knob")
	}
}

func TestBuildLoop_MaxIterationsPrecedence(t *testing.T) {
	tests := []struct {
		name    string
		optsMax int
		cfgMax  int
		want    int
	}{
		{name: "opts overrides cfg", optsMax: 7, cfgMax: 9, want: 7},
		{name: "falls back to cfg when opts unset", optsMax: 0, cfgMax: 9, want: 9},
		{name: "falls back to loop default when both unset", optsMax: 0, cfgMax: 0, want: 60},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := validConfig()
			cfg.Agent.MaxIterations = tt.cfgMax

			opts := validOptions(t)
			opts.MaxIterations = tt.optsMax

			loop, err := BuildLoop(cfg, validCreds(), opts)
			if err != nil {
				t.Fatalf("BuildLoop() error = %v, want nil", err)
			}
			if got := loopMaxIterations(t, loop); got != tt.want {
				t.Fatalf("loop maxIter = %d, want %d", got, tt.want)
			}
		})
	}
}

func TestBuildLoop_NegativeMaxIterationsError(t *testing.T) {
	tests := []struct {
		name    string
		optsMax int
		cfgMax  int
	}{
		{name: "opts negative", optsMax: -1, cfgMax: 9},
		{name: "cfg negative", optsMax: 0, cfgMax: -1},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := validConfig()
			cfg.Agent.MaxIterations = tt.cfgMax

			opts := validOptions(t)
			opts.MaxIterations = tt.optsMax

			_, err := BuildLoop(cfg, validCreds(), opts)
			if err == nil {
				t.Fatal("BuildLoop() error = nil, want an error for max iterations < 0")
			}
			if !strings.Contains(err.Error(), "max iterations") {
				t.Fatalf("BuildLoop() error = %q, want it to mention max iterations", err.Error())
			}
		})
	}
}

// TestBuildSystemPrompt_OverridePrecedence proves buildSystemPrompt resolves
// opts.SystemPrompt over cfg.Agent.SystemPrompt, and that the assembled
// prompt always contains the environment block regardless of which base
// prompt (or none) was supplied.
func TestBuildSystemPrompt_OverridePrecedence(t *testing.T) {
	tests := []struct {
		name        string
		optsSys     string
		cfgSys      string
		wantContain string
	}{
		{name: "opts overrides cfg", optsSys: "opts prompt", cfgSys: "cfg prompt", wantContain: "opts prompt"},
		{name: "falls back to cfg when opts empty", optsSys: "", cfgSys: "cfg prompt", wantContain: "cfg prompt"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			t.Setenv("AGENS_CONFIG_HOME", t.TempDir())

			cfg := validConfig()
			cfg.Agent.SystemPrompt = tt.cfgSys

			opts := validOptions(t)
			opts.SystemPrompt = tt.optsSys

			got, err := BuildSystemPrompt(cfg, opts, "gpt-4.1")
			if err != nil {
				t.Fatalf("BuildSystemPrompt() error = %v, want nil", err)
			}
			if !strings.Contains(got, tt.wantContain) {
				t.Fatalf("BuildSystemPrompt() = %q, want it to contain override %q", got, tt.wantContain)
			}
			if !strings.Contains(got, "You are powered by the model named gpt-4.1.") {
				t.Fatalf("BuildSystemPrompt() = %q, want it to contain the environment model marker", got)
			}
		})
	}
}

// TestBuildSystemPrompt_NoOverrideUsesModelSelectedBase proves that with no
// override at all, buildSystemPrompt still returns a non-empty prompt
// containing the environment block, sourced from prompt.Select(model)
// instead of a hardcoded persona string.
func TestBuildSystemPrompt_NoOverrideUsesModelSelectedBase(t *testing.T) {
	t.Setenv("AGENS_CONFIG_HOME", t.TempDir())

	cfg := validConfig()
	cfg.Agent.SystemPrompt = ""

	opts := validOptions(t)
	opts.SystemPrompt = ""

	got, err := BuildSystemPrompt(cfg, opts, "gpt-4.1")
	if err != nil {
		t.Fatalf("BuildSystemPrompt() error = %v, want nil", err)
	}
	if got == "" {
		t.Fatal("BuildSystemPrompt() = \"\", want a non-empty assembled prompt")
	}
	if !strings.Contains(got, "You are powered by the model named gpt-4.1.") {
		t.Fatalf("BuildSystemPrompt() = %q, want it to contain the environment model marker", got)
	}
}

func TestBuildLoop_MissingAPIKeyErrors(t *testing.T) {
	creds := auth.File{}

	_, err := BuildLoop(validConfig(), creds, validOptions(t))
	if err == nil {
		t.Fatal("BuildLoop() error = nil, want an error for missing credentials")
	}
	if !strings.Contains(err.Error(), "no credentials found") {
		t.Fatalf("BuildLoop() error = %q, want it to mention %q", err.Error(), "no credentials found")
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
	if !strings.Contains(err.Error(), "no credentials found") {
		t.Fatalf("BuildLoop() error = %q, want it to mention %q", err.Error(), "no credentials found")
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

func TestBuildLoop_APIKeyOnlyCreds_FallsBackToOpenAIDefaultModel(t *testing.T) {
	cfg := validConfig()
	cfg.Provider.Model = ""

	opts := validOptions(t)
	loop, err := BuildLoop(cfg, validCreds(), opts)
	if err != nil {
		t.Fatalf("BuildLoop() error = %v, want nil", err)
	}
	if loop == nil {
		t.Fatal("BuildLoop() loop = nil, want non-nil")
	}
}

func TestBuildLoop_ChatGPTOnlyCreds_Success(t *testing.T) {
	cfg := validConfig()
	cfg.Provider.Model = ""

	loop, err := BuildLoop(cfg, chatgptCreds(), validOptions(t))
	if err != nil {
		t.Fatalf("BuildLoop() error = %v, want nil", err)
	}
	if loop == nil {
		t.Fatal("BuildLoop() loop = nil, want non-nil")
	}
}

func TestBuildLoop_ExplicitTypeOverridesInference(t *testing.T) {
	cfg := validConfig()
	cfg.Provider.Type = chatgptProviderID
	cfg.Provider.Model = ""

	// validCreds() only has an "openai-api" entry: without the explicit
	// Type override this would infer "openai-api" and fail to find a
	// ChatGPT entry to build with, proving Type actually took effect.
	creds := chatgptCreds()

	loop, err := BuildLoop(cfg, creds, validOptions(t))
	if err != nil {
		t.Fatalf("BuildLoop() error = %v, want nil", err)
	}
	if loop == nil {
		t.Fatal("BuildLoop() loop = nil, want non-nil")
	}
}

func TestBuildLoop_BothEntriesPresent_ChatGPTWinsTiebreak(t *testing.T) {
	cfg := validConfig()
	cfg.Provider.Model = ""

	creds := validCreds()
	for k, v := range chatgptCreds() {
		creds[k] = v
	}

	loop, err := BuildLoop(cfg, creds, validOptions(t))
	if err != nil {
		t.Fatalf("BuildLoop() error = %v, want nil", err)
	}
	if loop == nil {
		t.Fatal("BuildLoop() loop = nil, want non-nil")
	}
}

func TestBuildProvider_APIKeyOnlyCreds_Success(t *testing.T) {
	p, err := BuildProvider(validConfig(), validCreds(), validOptions(t))
	if err != nil {
		t.Fatalf("BuildProvider() error = %v, want nil", err)
	}
	if p == nil {
		t.Fatal("BuildProvider() provider = nil, want non-nil")
	}
	if p.ID() != defaultProviderID {
		t.Fatalf("BuildProvider().ID() = %q, want %q", p.ID(), defaultProviderID)
	}
}

func TestBuildProvider_ChatGPTOnlyCreds_Success(t *testing.T) {
	cfg := validConfig()
	cfg.Provider.Model = ""

	p, err := BuildProvider(cfg, chatgptCreds(), validOptions(t))
	if err != nil {
		t.Fatalf("BuildProvider() error = %v, want nil", err)
	}
	if p == nil {
		t.Fatal("BuildProvider() provider = nil, want non-nil")
	}
	if p.ID() != chatgptProviderID {
		t.Fatalf("BuildProvider().ID() = %q, want %q", p.ID(), chatgptProviderID)
	}
}

func TestBuildProvider_MissingCredsErrors(t *testing.T) {
	_, err := BuildProvider(validConfig(), auth.File{}, validOptions(t))
	if err == nil {
		t.Fatal("BuildProvider() error = nil, want an error for missing credentials")
	}
	if !strings.Contains(err.Error(), "no credentials found") {
		t.Fatalf("BuildProvider() error = %q, want it to mention %q", err.Error(), "no credentials found")
	}
}

func TestBuildLoop_UnknownProviderTypeErrors(t *testing.T) {
	cfg := validConfig()
	cfg.Provider.Type = "some-other-provider"

	_, err := BuildLoop(cfg, validCreds(), validOptions(t))
	if err == nil {
		t.Fatal("BuildLoop() error = nil, want an error for an unknown provider type")
	}
	if !strings.Contains(err.Error(), "unknown provider type") {
		t.Fatalf("BuildLoop() error = %q, want it to mention %q", err.Error(), "unknown provider type")
	}
	if !strings.Contains(err.Error(), "some-other-provider") {
		t.Fatalf("BuildLoop() error = %q, want it to name the unknown type", err.Error())
	}
}

func TestSelectProviderID(t *testing.T) {
	tests := []struct {
		name    string
		cfgType string
		creds   auth.File
		want    string
		wantErr string
	}{
		{name: "explicit type wins", cfgType: chatgptProviderID, creds: validCreds(), want: chatgptProviderID},
		{name: "infers api-key from creds", creds: validCreds(), want: defaultProviderID},
		{name: "infers chatgpt from creds", creds: chatgptCreds(), want: chatgptProviderID},
		{name: "no credentials errors", creds: auth.File{}, wantErr: "no credentials found"},
		{name: "unknown explicit type errors", cfgType: "bogus", creds: validCreds(), wantErr: "unknown provider type"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := validConfig()
			cfg.Provider.Type = tt.cfgType

			got, err := selectProviderID(cfg, tt.creds)
			if tt.wantErr != "" {
				if err == nil || !strings.Contains(err.Error(), tt.wantErr) {
					t.Fatalf("selectProviderID() error = %v, want it to contain %q", err, tt.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatalf("selectProviderID() error = %v, want nil", err)
			}
			if got != tt.want {
				t.Fatalf("selectProviderID() = %q, want %q", got, tt.want)
			}
		})
	}
}

// TestSelectProviderID_PartialChatGPTCredsNeverSelectsChatGPT locks the
// invariant that selectProviderID only infers chatgptProviderID when BOTH
// AccessToken and RefreshToken are present: a half-populated entry (e.g. an
// access token that survived a failed refresh, or a refresh token before the
// first exchange) must never win the tiebreak over a usable api-key entry,
// and must never be reported as a usable credential on its own.
func TestSelectProviderID_PartialChatGPTCredsNeverSelectsChatGPT(t *testing.T) {
	tests := []struct {
		name  string
		entry auth.Entry
	}{
		{name: "access token only", entry: auth.Entry{AccessToken: "access-token"}},
		{name: "refresh token only", entry: auth.Entry{RefreshToken: "refresh-token"}},
	}

	for _, tt := range tests {
		t.Run(tt.name+"/falls back to api-key when present", func(t *testing.T) {
			creds := auth.File{
				chatgptProviderID: tt.entry,
				defaultProviderID: {APIKey: "sk-test-key"},
			}

			got, err := selectProviderID(validConfig(), creds)
			if err != nil {
				t.Fatalf("selectProviderID() error = %v, want nil", err)
			}
			if got != defaultProviderID {
				t.Fatalf("selectProviderID() = %q, want %q (a partial chatgpt entry must never win the tiebreak)", got, defaultProviderID)
			}
		})

		t.Run(tt.name+"/errors when no api-key entry exists", func(t *testing.T) {
			creds := auth.File{
				chatgptProviderID: tt.entry,
			}

			_, err := selectProviderID(validConfig(), creds)
			if err == nil || !strings.Contains(err.Error(), "no credentials found") {
				t.Fatalf("selectProviderID() error = %v, want it to contain %q", err, "no credentials found")
			}
		})
	}
}

func TestDefaultModelFor(t *testing.T) {
	if got := defaultModelFor(defaultProviderID); got != openai.DefaultModel {
		t.Fatalf("defaultModelFor(%q) = %q, want %q", defaultProviderID, got, openai.DefaultModel)
	}
	if got := defaultModelFor(chatgptProviderID); got != chatgpt.DefaultModel {
		t.Fatalf("defaultModelFor(%q) = %q, want %q", chatgptProviderID, got, chatgpt.DefaultModel)
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

	for _, want := range []string{"read", "write", "edit", "bash", "grep", "glob", "webfetch"} {
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

func TestBuildGate_WebfetchAskConsultsPrompter_DenyOnceDenies(t *testing.T) {
	dir := t.TempDir()
	fp := &fakePrompter{answer: permission.AnswerDenyOnce}
	gate, err := buildGate(Options{ProjectRoot: dir, Prompter: fp})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	call := message.ToolUsePart{ID: "tu_1", Name: "webfetch", Input: json.RawMessage(`{"url":"http://169.254.169.254/"}`)}
	result, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("gate.Run(webfetch) error = %v, want nil", err)
	}
	if !result.IsError {
		t.Fatalf("gate.Run(webfetch) result = %+v, want IsError == true for a deny-once answer", result)
	}
	if len(fp.calls) != 1 {
		t.Fatalf("prompter was consulted %d time(s) for a webfetch call, want exactly 1 (webfetch must resolve to Ask, no seeded rule)", len(fp.calls))
	}
}

// TestPersistChatGPTEntry_PreservesOtherProviderEntries proves persistChatGPTEntry's
// load-modify-save round trip never drops a sibling provider's entry: a
// live token refresh for openai-chatgpt must not silently erase an
// already-configured openai-api credential in the same file.
func TestPersistChatGPTEntry_PreservesOtherProviderEntries(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("AGENS_CONFIG_HOME", dir)

	seed := auth.File{
		defaultProviderID: {APIKey: "sk-existing-key"},
	}
	if err := auth.Save(auth.DefaultPath(), seed); err != nil {
		t.Fatalf("seed auth.Save() error = %v, want nil", err)
	}

	refreshed := auth.Entry{AccessToken: "new-access", RefreshToken: "new-refresh"}
	if err := persistChatGPTEntry(refreshed); err != nil {
		t.Fatalf("persistChatGPTEntry() error = %v, want nil", err)
	}

	got, err := auth.Load(auth.DefaultPath())
	if err != nil {
		t.Fatalf("auth.Load() error = %v, want nil", err)
	}
	if got[defaultProviderID].APIKey != "sk-existing-key" {
		t.Fatalf("got[%q].APIKey = %q, want the pre-existing entry preserved", defaultProviderID, got[defaultProviderID].APIKey)
	}
	if got[chatgptProviderID] != refreshed {
		t.Fatalf("got[%q] = %+v, want %+v", chatgptProviderID, got[chatgptProviderID], refreshed)
	}
}

// TestPersistChatGPTEntry_WritesEvenWhenNoExistingFile proves a missing (or
// unreadable) credentials file never blocks persisting a refreshed token:
// persistChatGPTEntry falls back to an empty auth.File and still writes the
// refreshed entry, instead of losing it.
func TestPersistChatGPTEntry_WritesEvenWhenNoExistingFile(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("AGENS_CONFIG_HOME", dir)

	refreshed := auth.Entry{AccessToken: "new-access", RefreshToken: "new-refresh"}
	if err := persistChatGPTEntry(refreshed); err != nil {
		t.Fatalf("persistChatGPTEntry() error = %v, want nil", err)
	}

	got, err := auth.Load(auth.DefaultPath())
	if err != nil {
		t.Fatalf("auth.Load() error = %v, want nil", err)
	}
	if got[chatgptProviderID] != refreshed {
		t.Fatalf("got[%q] = %+v, want %+v", chatgptProviderID, got[chatgptProviderID], refreshed)
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

// fakeSubRunner is a scripted task.Runner recording the description it was asked
// to run and returning a canned result, so the parent gate's task wiring can be
// exercised without a provider or a nested loop.
type fakeSubRunner struct {
	out     string
	gotDesc string
}

func (f *fakeSubRunner) Run(_ context.Context, description string) (string, error) {
	f.gotDesc = description
	return f.out, nil
}

func resultText(r message.ToolResultPart) string {
	var b strings.Builder
	for _, part := range r.Content {
		if text, ok := part.(message.TextPart); ok {
			b.WriteString(text.Text)
		}
	}
	return b.String()
}

func specNames(t *testing.T, gate interface{ Specs() []provider.ToolSpec }) map[string]bool {
	t.Helper()
	names := map[string]bool{}
	for _, s := range gate.Specs() {
		names[s.Name] = true
	}
	return names
}

func TestBuildGate_ExcludesTaskSoSubagentsDoNotRecurse(t *testing.T) {
	gate, err := buildGate(validOptions(t))
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}
	if specNames(t, gate)["task"] {
		t.Fatal("buildGate() includes task, want it absent so a subagent cannot delegate recursively")
	}
}

func TestBuildParentGate_RegistersTaskAllowedWithoutPrompting(t *testing.T) {
	runner := &fakeSubRunner{out: "subagent report"}
	gate, err := buildParentGate(Options{ProjectRoot: t.TempDir(), Prompter: failOnCallPrompter{t: t}}, runner)
	if err != nil {
		t.Fatalf("buildParentGate() error = %v, want nil", err)
	}

	if !specNames(t, gate)["task"] {
		t.Fatal("buildParentGate() specs missing task, want it registered")
	}

	call := message.ToolUsePart{ID: "tu_1", Name: "task", Input: json.RawMessage(`{"description":"go do X"}`)}
	result, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("gate.Run(task) error = %v, want nil", err)
	}
	if result.IsError {
		t.Fatalf("gate.Run(task) result = %+v, want a successful delegation (task pre-seeded Allow)", result)
	}
	if got := resultText(result); got != "subagent report" {
		t.Fatalf("gate.Run(task) text = %q, want the subagent's report", got)
	}
	if runner.gotDesc != "go do X" {
		t.Fatalf("runner got description %q, want %q", runner.gotDesc, "go do X")
	}
}

// fakeLoop is a loopRunner double: it records the history it was driven with and
// returns a canned grown history / error, so the subagent runner's glue can be
// tested without a provider.
type fakeLoop struct {
	gotHistory []message.Message
	ret        []message.Message
	err        error
	emits      []agentloop.LoopEvent
}

func (f *fakeLoop) Run(_ context.Context, history []message.Message, sink func(agentloop.LoopEvent)) ([]message.Message, error) {
	f.gotHistory = history
	// Replay the subagent's own scripted events into the sink so the runner's
	// translation to LoopSubagent* events can be observed.
	if sink != nil {
		for _, ev := range f.emits {
			sink(ev)
		}
	}
	return f.ret, f.err
}

func TestSubagentRunner_SeedsDescriptionAndReturnsFinalText(t *testing.T) {
	fl := &fakeLoop{ret: []message.Message{
		message.NewMessage(message.RoleUser, message.TextPart{Text: "investigate the bug"}),
		message.NewMessage(message.RoleAssistant, message.TextPart{Text: "here are the findings"}),
	}}

	out, err := newSubagentRunner(fl, "subagent", "gpt-x").Run(context.Background(), "investigate the bug")
	if err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if out != "here are the findings" {
		t.Fatalf("Run() = %q, want the subagent's final assistant text", out)
	}

	if len(fl.gotHistory) != 1 {
		t.Fatalf("subagent seeded with %d messages, want exactly 1 (the task description)", len(fl.gotHistory))
	}
	if fl.gotHistory[0].Role != message.RoleUser {
		t.Fatalf("subagent's seed message role = %v, want user", fl.gotHistory[0].Role)
	}
	if got := resultTextOf(fl.gotHistory[0]); got != "investigate the bug" {
		t.Fatalf("subagent seeded with %q, want the task description", got)
	}
}

func TestSubagentRunner_PropagatesLoopError(t *testing.T) {
	fl := &fakeLoop{err: errors.New("stream failed")}

	if _, err := newSubagentRunner(fl, "subagent", "gpt-x").Run(context.Background(), "do it"); err == nil {
		t.Fatal("Run() error = nil, want the loop error propagated")
	}
}

func TestSubagentRunner_StreamsLifecycleToParentSink(t *testing.T) {
	// The subagent's finalized assistant message carries two tool calls; the sink
	// turns each into a subagent activity event.
	toolMsg := message.NewMessage(message.RoleAssistant,
		message.ToolUsePart{ID: "t1", Name: "read"},
		message.ToolUsePart{ID: "t2", Name: "bash"},
	)
	fl := &fakeLoop{
		ret: []message.Message{
			message.NewMessage(message.RoleAssistant, message.TextPart{Text: "final report"}),
		},
		emits: []agentloop.LoopEvent{
			{Kind: agentloop.LoopMessageDone, Message: &toolMsg},
		},
	}

	var got []agentloop.LoopEvent
	ctx := agentloop.WithEventSink(context.Background(), func(ev agentloop.LoopEvent) { got = append(got, ev) })

	if _, err := newSubagentRunner(fl, "explore", "gpt-5.5").Run(ctx, "look around"); err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}

	if len(got) == 0 {
		t.Fatal("no events reached the parent sink, want the subagent lifecycle streamed")
	}
	if got[0].Kind != agentloop.LoopSubagentStarted {
		t.Fatalf("first event kind = %v, want LoopSubagentStarted", got[0].Kind)
	}
	if got[0].Subagent.Name != "explore" || got[0].Subagent.Model != "gpt-5.5" {
		t.Fatalf("started event = %+v, want the runner's name/model", got[0].Subagent)
	}

	last := got[len(got)-1]
	if last.Kind != agentloop.LoopSubagentFinished {
		t.Fatalf("last event kind = %v, want LoopSubagentFinished", last.Kind)
	}
	if last.Subagent.Failed {
		t.Fatalf("finished event Failed = true, want a clean completion")
	}
	if last.Subagent.Result != "final report" {
		t.Fatalf("finished event Result = %q, want the subagent's report", last.Subagent.Result)
	}

	// Every started/finished pair shares one id, and the two tool calls became
	// activity events.
	id := got[0].Subagent.ID
	activity := 0
	for _, ev := range got {
		if ev.Subagent.ID != id {
			t.Fatalf("event %+v uses a different subagent id, want all %q", ev, id)
		}
		if ev.Kind == agentloop.LoopSubagentActivity && ev.Subagent.ToolCall.Name != "" {
			activity++
		}
	}
	if activity != 2 {
		t.Fatalf("activity events = %d, want 2 (one per tool the subagent invoked)", activity)
	}
}

func resultTextOf(m message.Message) string {
	var b strings.Builder
	for _, part := range m.Parts {
		if text, ok := part.(message.TextPart); ok {
			b.WriteString(text.Text)
		}
	}
	return b.String()
}

func TestSubagentSystemPrompt_AppendsInstructionToBase(t *testing.T) {
	got := subagentSystemPrompt("BASE PROMPT")
	if !strings.HasPrefix(got, "BASE PROMPT") {
		t.Fatalf("subagentSystemPrompt() = %q, want it to keep the base prompt", got)
	}
	if !strings.Contains(got, "subagent") || !strings.Contains(got, "cannot delegate") {
		t.Fatalf("subagentSystemPrompt() = %q, want the subagent instruction appended", got)
	}

	if got := subagentSystemPrompt(""); !strings.Contains(got, "subagent") {
		t.Fatalf("subagentSystemPrompt(\"\") = %q, want the bare instruction", got)
	}
}

func TestLastAssistantText(t *testing.T) {
	history := []message.Message{
		message.NewMessage(message.RoleUser, message.TextPart{Text: "do it"}),
		message.NewMessage(message.RoleAssistant, message.TextPart{Text: "thinking out loud"}),
		message.NewMessage(message.RoleUser, message.TextPart{Text: "a tool result"}),
		message.NewMessage(message.RoleAssistant, message.TextPart{Text: "final answer"}),
	}

	if got := lastAssistantText(history); got != "final answer" {
		t.Fatalf("lastAssistantText() = %q, want the most recent assistant text", got)
	}
	if got := lastAssistantText(nil); got != "" {
		t.Fatalf("lastAssistantText(nil) = %q, want empty", got)
	}
}

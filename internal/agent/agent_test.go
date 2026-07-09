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

	"github.com/0xErwin1/agens/internal/agentdef"
	"github.com/0xErwin1/agens/internal/agentloop"
	"github.com/0xErwin1/agens/internal/auth"
	"github.com/0xErwin1/agens/internal/config"
	"github.com/0xErwin1/agens/internal/mcpclient"
	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/permission"
	"github.com/0xErwin1/agens/internal/provider"
	"github.com/0xErwin1/agens/internal/provider/chatgpt"
	"github.com/0xErwin1/agens/internal/provider/openai"
	agenttool "github.com/0xErwin1/agens/internal/tool"
	"github.com/0xErwin1/agens/internal/tool/task"
	"github.com/google/jsonschema-go/jsonschema"
)

// loadTestDefs returns the built-in agent definitions (build, plan) for wiring
// the parent gate and subagent runner in tests without touching disk.
func loadTestDefs(t *testing.T) *agentdef.Set {
	t.Helper()
	set, _ := agentdef.Load("", "")
	return set
}

// fakeBuilder is a subLoopBuilder that ignores the resolved definition and model
// and always returns fl, so the subagent runner's glue can be tested with a
// scripted loop.
func fakeBuilder(fl loopRunner) subLoopBuilder {
	return func(agentdef.Definition, string) (loopRunner, error) { return fl, nil }
}

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

// writeProjectSkill writes a valid SKILL.md under projectRoot/.agens/skills/<name>.
func writeProjectSkill(t *testing.T, projectRoot, name, description string) {
	t.Helper()
	dir := filepath.Join(projectRoot, ".agens", "skills", name)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatalf("mkdir skill dir: %v", err)
	}
	content := "---\nname: " + name + "\ndescription: " + description + "\n---\nthe skill body\n"
	if err := os.WriteFile(filepath.Join(dir, "SKILL.md"), []byte(content), 0o644); err != nil {
		t.Fatalf("write SKILL.md: %v", err)
	}
}

func TestBuildSystemPrompt_IncludesSkillsWhenSet(t *testing.T) {
	t.Setenv("AGENS_CONFIG_HOME", t.TempDir())

	opts := validOptions(t)
	writeProjectSkill(t, opts.ProjectRoot, "git-commit", "make conventional commits")

	skills, _, err := LoadSkills(opts)
	if err != nil {
		t.Fatalf("LoadSkills() error = %v, want nil", err)
	}
	opts.Skills = skills

	got, err := BuildSystemPrompt(validConfig(), opts, "gpt-4.1")
	if err != nil {
		t.Fatalf("BuildSystemPrompt() error = %v, want nil", err)
	}
	if !strings.Contains(got, "Available skills") || !strings.Contains(got, "git-commit") {
		t.Fatalf("BuildSystemPrompt() = %q, want the level-1 skills block", got)
	}
}

func TestLoadSkills_DiscoversProjectSkillsAndWarnsOnMalformed(t *testing.T) {
	t.Setenv("AGENS_CONFIG_HOME", t.TempDir())

	opts := validOptions(t)
	writeProjectSkill(t, opts.ProjectRoot, "good", "a good skill")

	// A malformed skill (no name) must be skipped with a warning, not fail the load.
	badDir := filepath.Join(opts.ProjectRoot, ".agens", "skills", "bad")
	if err := os.MkdirAll(badDir, 0o755); err != nil {
		t.Fatalf("mkdir bad skill: %v", err)
	}
	if err := os.WriteFile(filepath.Join(badDir, "SKILL.md"), []byte("---\ndescription: no name\n---\nbody\n"), 0o644); err != nil {
		t.Fatalf("write bad SKILL.md: %v", err)
	}

	set, warnings, err := LoadSkills(opts)
	if err != nil {
		t.Fatalf("LoadSkills() error = %v, want nil", err)
	}
	if _, ok := set.ByName("good"); !ok {
		t.Fatal("the valid skill was not discovered")
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], "bad") {
		t.Fatalf("warnings = %v, want one naming the malformed skill", warnings)
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

type fakeMCPDiscoverer struct {
	tools       []agenttool.Tool
	diagnostics []mcpclient.Diagnostic
	ctx         context.Context
}

func (f *fakeMCPDiscoverer) Discover(ctx context.Context) ([]agenttool.Tool, []mcpclient.Diagnostic) {
	f.ctx = ctx
	return f.tools, f.diagnostics
}

type fakeMCPTool struct {
	name        string
	description string
	result      agenttool.Result
	err         error
	calls       int
}

func (f *fakeMCPTool) Name() string        { return f.name }
func (f *fakeMCPTool) Description() string { return f.description }
func (f *fakeMCPTool) Schema() *jsonschema.Schema {
	return nil
}
func (f *fakeMCPTool) Execute(context.Context, json.RawMessage) (agenttool.Result, error) {
	f.calls++
	return f.result, f.err
}

func TestBuildGate_RegistersDiscoveredMCPTools(t *testing.T) {
	searchTool := &fakeMCPTool{name: "docs_search", description: "Search docs", result: agenttool.Result{Text: "found docs"}}
	fetchTool := &fakeMCPTool{name: "docs_fetch", description: "Fetch docs", result: agenttool.Result{Text: "fetched docs"}}
	gate, err := buildGate(Options{
		ProjectRoot:   t.TempDir(),
		Prompter:      &fakePrompter{answer: permission.AnswerAllowOnce},
		mcpDiscoverer: &fakeMCPDiscoverer{tools: []agenttool.Tool{searchTool, fetchTool}},
	})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	names := specNames(t, gate)
	if !names["docs_search"] || !names["docs_fetch"] {
		t.Fatalf("gate specs names = %#v, want docs_search and docs_fetch", names)
	}

	result, err := gate.Run(context.Background(), message.ToolUsePart{ID: "tu_mcp", Name: "docs_search", Input: json.RawMessage(`{}`)})
	if err != nil {
		t.Fatalf("gate.Run(docs_search) error = %v, want nil", err)
	}
	if result.IsError || resultText(result) != "found docs" {
		t.Fatalf("gate.Run(docs_search) result = %+v text %q, want successful MCP result", result, resultText(result))
	}
	if searchTool.calls != 1 {
		t.Fatalf("docs_search calls = %d, want 1", searchTool.calls)
	}
}

func TestBuildGate_DuplicateMCPToolNamesAreErrors(t *testing.T) {
	_, err := buildGate(Options{
		ProjectRoot: t.TempDir(),
		mcpDiscoverer: &fakeMCPDiscoverer{tools: []agenttool.Tool{
			&fakeMCPTool{name: "docs_search", description: "first"},
			&fakeMCPTool{name: "docs_search", description: "second"},
		}},
	})
	if err == nil {
		t.Fatal("buildGate() error = nil, want duplicate MCP tool name error")
	}
	if !strings.Contains(err.Error(), "duplicate MCP tool name") || !strings.Contains(err.Error(), "docs_search") {
		t.Fatalf("buildGate() error = %q, want clear duplicate docs_search diagnostic", err.Error())
	}
}

func TestBuildGate_DiscoveredMCPToolRemainsVisibleAfterRuntimeFailure(t *testing.T) {
	searchTool := &fakeMCPTool{name: "docs_search", description: "Search docs", result: agenttool.Result{Text: "connect refused", IsError: true}}
	gate, err := buildGate(Options{
		ProjectRoot:   t.TempDir(),
		Prompter:      &fakePrompter{answer: permission.AnswerAllowOnce},
		mcpDiscoverer: &fakeMCPDiscoverer{tools: []agenttool.Tool{searchTool}},
	})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}
	if !specNames(t, gate)["docs_search"] {
		t.Fatal("gate specs missing docs_search after successful discovery")
	}

	result, err := gate.Run(context.Background(), message.ToolUsePart{ID: "tu_mcp", Name: "docs_search", Input: json.RawMessage(`{"query":"mcp"}`)})
	if err != nil {
		t.Fatalf("gate.Run(docs_search) error = %v, want nil", err)
	}
	if !result.IsError || !strings.Contains(resultText(result), "connect refused") {
		t.Fatalf("gate.Run(docs_search) result = %+v text %q, want tool-level runtime failure", result, resultText(result))
	}
	if !specNames(t, gate)["docs_search"] {
		t.Fatal("gate specs lost docs_search after runtime failure")
	}
}

func TestBuildGate_MCPToolsUseExistingPermissionGate(t *testing.T) {
	searchTool := &fakeMCPTool{name: "docs_search", description: "Search docs", result: agenttool.Result{Text: "should not execute"}}
	fp := &fakePrompter{answer: permission.AnswerDenyOnce}
	gate, err := buildGate(Options{
		ProjectRoot:   t.TempDir(),
		Prompter:      fp,
		mcpDiscoverer: &fakeMCPDiscoverer{tools: []agenttool.Tool{searchTool}},
	})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	result, err := gate.Run(context.Background(), message.ToolUsePart{ID: "tu_mcp", Name: "docs_search", Input: json.RawMessage(`{}`)})
	if err != nil {
		t.Fatalf("gate.Run(docs_search) error = %v, want nil", err)
	}
	if !result.IsError || !strings.Contains(resultText(result), "permission denied") {
		t.Fatalf("gate.Run(docs_search) result = %+v text %q, want permission denial", result, resultText(result))
	}
	if len(fp.calls) != 1 || fp.calls[0].Name != "docs_search" {
		t.Fatalf("prompter calls = %+v, want one docs_search permission prompt", fp.calls)
	}
	if searchTool.calls != 0 {
		t.Fatalf("docs_search calls = %d, want 0 after permission denial", searchTool.calls)
	}
}

func TestBuildGate_MCPDiscoveryUsesBoundedContext(t *testing.T) {
	discoverer := &fakeMCPDiscoverer{}
	_, err := buildGate(Options{ProjectRoot: t.TempDir(), mcpDiscoverer: discoverer})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}
	if discoverer.ctx == nil {
		t.Fatal("Discover context = nil")
	}
	deadline, ok := discoverer.ctx.Deadline()
	if !ok {
		t.Fatal("Discover context has no deadline")
	}
	if remaining := time.Until(deadline); remaining <= 0 || remaining > mcpDiscoveryTimeout {
		t.Fatalf("Discover deadline remaining = %s, want within %s", remaining, mcpDiscoveryTimeout)
	}
}

func TestBuildGate_MCPDiscoveryFailureDoesNotFabricateTools(t *testing.T) {
	gate, err := buildGate(Options{
		ProjectRoot: t.TempDir(),
		Prompter:    &fakePrompter{answer: permission.AnswerAllowOnce},
		mcpDiscoverer: &fakeMCPDiscoverer{diagnostics: []mcpclient.Diagnostic{{
			Server: "offline",
			Err:    "connect refused",
		}}},
	})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil degraded startup", err)
	}
	if specNames(t, gate)["offline_search"] {
		t.Fatal("gate specs include offline_search, want no fabricated MCP tool after discovery failure")
	}

	result, err := gate.Run(context.Background(), message.ToolUsePart{ID: "tu_mcp", Name: "offline_search", Input: json.RawMessage(`{}`)})
	if err != nil {
		t.Fatalf("gate.Run(offline_search) error = %v, want nil unknown-tool result", err)
	}
	if !result.IsError || !strings.Contains(resultText(result), "unknown tool") {
		t.Fatalf("gate.Run(offline_search) result = %+v text %q, want unknown tool", result, resultText(result))
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

func TestBuildGate_InvalidPermissionsMatcherFailsWithAClearError(t *testing.T) {
	tests := []struct {
		name  string
		perms config.Permissions
	}{
		{name: "invalid global allow", perms: config.Permissions{GlobalAllow: []string{"["}}},
		{name: "invalid global deny", perms: config.Permissions{GlobalDeny: []string{"["}}},
		{name: "invalid project allow", perms: config.Permissions{ProjectAllow: []string{"["}}},
		{name: "invalid project deny", perms: config.Permissions{ProjectDeny: []string{"["}}},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			opts := validOptions(t)
			opts.Permissions = tt.perms

			_, err := buildGate(opts)
			if err == nil {
				t.Fatalf("buildGate() error = nil, want a clear composition error for an invalid matcher")
			}
			if !strings.Contains(err.Error(), `"["`) {
				t.Fatalf("buildGate() error = %q, want it to name the offending pattern %q", err.Error(), "[")
			}
		})
	}
}

// These two tests use bare tool-name matchers (no argument pattern):
// assembleGate's static-rule composition and WithGlobalDenies wiring is name
// scoped regardless of which Projector the Engine uses, so they exercise
// task 2.8's precedence wiring without depending on which Projector
// assembleGate installs.
func TestBuildGate_ProjectAllowCannotReachGlobalDeny(t *testing.T) {
	dir := t.TempDir()
	fp := &fakePrompter{answer: permission.AnswerAllowOnce}
	gate, err := buildGate(Options{
		ProjectRoot: dir,
		Prompter:    fp,
		Permissions: config.Permissions{
			GlobalDeny:   []string{"bash"},
			ProjectAllow: []string{"bash"},
		},
	})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	call := message.ToolUsePart{ID: "tu_1", Name: "bash"}
	result, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("gate.Run() error = %v, want nil (a denial is a tool-level result, not a Go error)", err)
	}
	if !result.IsError {
		t.Fatalf("gate.Run() result = %+v, want IsError == true (a project allow must never reach a global deny)", result)
	}
	if len(fp.calls) != 0 {
		t.Fatalf("prompter was consulted %d time(s), want 0 (a global deny must short-circuit before the Prompter)", len(fp.calls))
	}
}

func TestBuildGate_GlobalAndProjectAllowSkipThePrompter(t *testing.T) {
	dir := t.TempDir()
	gate, err := buildGate(Options{
		ProjectRoot: dir,
		Prompter:    failOnCallPrompter{t: t},
		Permissions: config.Permissions{
			GlobalAllow:  []string{"engram_mem_save"},
			ProjectAllow: []string{"bash"},
		},
	})
	if err != nil {
		t.Fatalf("buildGate() error = %v, want nil", err)
	}

	call := message.ToolUsePart{ID: "tu_1", Name: "bash", Input: json.RawMessage(`{"command":"echo hi"}`)}
	result, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("gate.Run() error = %v, want nil", err)
	}
	if result.IsError {
		t.Fatalf("gate.Run() result = %+v, want a project allow to run without prompting", result)
	}
}

// fakeSubRunner is a scripted task.Runner recording the description it was asked
// to run and returning a canned result, so the parent gate's task wiring can be
// exercised without a provider or a nested loop.
type fakeSubRunner struct {
	out     string
	gotDesc string
}

func (f *fakeSubRunner) Run(_ context.Context, req task.Request) (string, error) {
	f.gotDesc = req.Description
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
	catalog := task.NewCatalog(subagentOptions(loadTestDefs(t)))
	gate, err := buildParentGate(Options{ProjectRoot: t.TempDir(), Prompter: failOnCallPrompter{t: t}}, runner, catalog, nil)
	if err != nil {
		t.Fatalf("buildParentGate() error = %v, want nil", err)
	}

	if !specNames(t, gate)["task"] {
		t.Fatal("buildParentGate() specs missing task, want it registered")
	}
	if specNames(t, gate)["skill"] {
		t.Fatal("buildParentGate() registered skill with no skills, want it absent")
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

func TestBuildParentGate_RegistersSkillAllowedWhenSkillsPresent(t *testing.T) {
	projectRoot := t.TempDir()
	writeProjectSkill(t, projectRoot, "git-commit", "make conventional commits")

	skills, _, err := LoadSkills(Options{ProjectRoot: projectRoot})
	if err != nil {
		t.Fatalf("LoadSkills() error = %v, want nil", err)
	}

	runner := &fakeSubRunner{out: "report"}
	catalog := task.NewCatalog(subagentOptions(loadTestDefs(t)))
	gate, err := buildParentGate(Options{ProjectRoot: projectRoot, Prompter: failOnCallPrompter{t: t}}, runner, catalog, skills)
	if err != nil {
		t.Fatalf("buildParentGate() error = %v, want nil", err)
	}

	if !specNames(t, gate)["skill"] {
		t.Fatal("buildParentGate() specs missing skill, want it registered when skills exist")
	}

	// skill is pre-seeded to Allow, so it runs without the failOnCallPrompter firing.
	call := message.ToolUsePart{ID: "tu_1", Name: "skill", Input: json.RawMessage(`{"name":"git-commit"}`)}
	result, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("gate.Run(skill) error = %v, want nil", err)
	}
	if result.IsError {
		t.Fatalf("gate.Run(skill) result = %+v, want the skill loaded without prompting", result)
	}
	if !strings.Contains(resultText(result), "the skill body") {
		t.Fatalf("gate.Run(skill) text = %q, want the SKILL.md body", resultText(result))
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

	out, err := newSubagentRunner(fakeBuilder(fl), loadTestDefs(t), nil, "gpt-x").Run(context.Background(), task.Request{Description: "investigate the bug", Agent: "build"})
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

	if _, err := newSubagentRunner(fakeBuilder(fl), loadTestDefs(t), nil, "gpt-x").Run(context.Background(), task.Request{Description: "do it", Agent: "build"}); err == nil {
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

	req := task.Request{Description: "look around", Agent: "build", Model: "gpt-5.5"}
	if _, err := newSubagentRunner(fakeBuilder(fl), loadTestDefs(t), nil, "gpt-x").Run(ctx, req); err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}

	if len(got) == 0 {
		t.Fatal("no events reached the parent sink, want the subagent lifecycle streamed")
	}
	if got[0].Kind != agentloop.LoopSubagentStarted {
		t.Fatalf("first event kind = %v, want LoopSubagentStarted", got[0].Kind)
	}
	if got[0].Subagent.Name != "build" || got[0].Subagent.Model != "gpt-5.5" {
		t.Fatalf("started event = %+v, want the resolved agent name/model", got[0].Subagent)
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

	// Every event shares one subagent id, and the subagent's own message-done
	// event is forwarded verbatim as an activity event carrying its two tool calls.
	id := got[0].Subagent.ID
	activity := 0
	for _, ev := range got {
		if ev.Subagent.ID != id {
			t.Fatalf("event %+v uses a different subagent id, want all %q", ev, id)
		}
		if ev.Kind == agentloop.LoopSubagentActivity && ev.Subagent.Event != nil {
			activity++
			if ev.Subagent.Event.Kind != agentloop.LoopMessageDone {
				t.Fatalf("forwarded activity event kind = %v, want the subagent's LoopMessageDone verbatim", ev.Subagent.Event.Kind)
			}
			calls := 0
			for _, part := range ev.Subagent.Event.Message.Parts {
				if _, ok := part.(message.ToolUsePart); ok {
					calls++
				}
			}
			if calls != 2 {
				t.Fatalf("forwarded message carries %d tool calls, want 2", calls)
			}
		}
	}
	if activity != 1 {
		t.Fatalf("activity events = %d, want 1 (the forwarded message-done event)", activity)
	}
}

func TestSubagentRunner_ResolvesModelPrecedence(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "agents")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatalf("mkdir: %v", err)
	}
	body := "---\nmodel: def-model\nmodels:\n  - def-model\n  - req-model\n---\nwork\n"
	if err := os.WriteFile(filepath.Join(dir, "worker.md"), []byte(body), 0o644); err != nil {
		t.Fatalf("write agent: %v", err)
	}

	defs, _ := agentdef.Load("", dir)

	r := newSubagentRunner(nil, defs, nil, "parent-model")

	if _, model := r.resolve(task.Request{Agent: "worker", Model: "req-model"}); model != "req-model" {
		t.Fatalf("resolve model = %q, want the request's model to win", model)
	}
	if _, model := r.resolve(task.Request{Agent: "worker"}); model != "def-model" {
		t.Fatalf("resolve model = %q, want the definition's default model", model)
	}
	if _, model := r.resolve(task.Request{Agent: "build"}); model != "parent-model" {
		t.Fatalf("resolve model = %q, want the parent model when neither request nor def sets one", model)
	}
	if def, _ := r.resolve(task.Request{Agent: "ghost"}); def.Name == "" {
		t.Fatal("resolve of an unknown agent must fall back to a subagent-capable definition, not an empty one")
	}
}

// TestSubagentRunner_ResolveClampsToCatalogAllowList proves that an omitted or
// parent-inherited model is clamped to the agent's allowed set, closing the
// bypass where the task tool only validates an explicitly requested model.
func TestSubagentRunner_ResolveClampsToCatalogAllowList(t *testing.T) {
	defs := loadTestDefs(t) // build (unrestricted), plan
	catalog := task.NewCatalog([]task.Agent{
		{Name: "build", Models: []string{"gpt-4.1"}}, // restricted live
		{Name: "plan"}, // unrestricted
	})
	r := newSubagentRunner(nil, defs, catalog, "gpt-5.5")

	// Model omitted for a restricted agent → clamped to its first allowed model,
	// not the disallowed parent model.
	if _, model := r.resolve(task.Request{Agent: "build"}); model != "gpt-4.1" {
		t.Fatalf("resolve model = %q, want it clamped to the agent's allowed gpt-4.1, not the parent gpt-5.5", model)
	}

	// An unrestricted agent still inherits the parent model.
	if _, model := r.resolve(task.Request{Agent: "plan"}); model != "gpt-5.5" {
		t.Fatalf("resolve model = %q, want the parent model for an unrestricted agent", model)
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

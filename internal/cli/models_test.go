package cli

import (
	"bytes"
	"context"
	"errors"
	"strings"
	"testing"

	"github.com/0xErwin1/agens/internal/agent"
	"github.com/0xErwin1/agens/internal/provider"
)

// modelsFakeProvider is a minimal provider.Provider for models command
// tests: Models returns the scripted models/err pair, and Stream is never
// exercised by this command.
type modelsFakeProvider struct {
	models    []provider.ModelInfo
	modelsErr error
}

var _ provider.Provider = (*modelsFakeProvider)(nil)

func (p *modelsFakeProvider) ID() string { return "models-fake-provider" }

func (p *modelsFakeProvider) Models(context.Context) ([]provider.ModelInfo, error) {
	return p.models, p.modelsErr
}

func (p *modelsFakeProvider) EffortLevels() []string { return nil }

func (p *modelsFakeProvider) Stream(context.Context, provider.ChatRequest) (provider.StreamReader, error) {
	return nil, nil
}

func TestModelsCommand_PrintsTableWithContextWindow(t *testing.T) {
	fake := &modelsFakeProvider{
		models: []provider.ModelInfo{
			{ID: "gpt-5", DisplayName: "GPT-5", ContextWindow: 400000},
			{ID: "gpt-5-mini", DisplayName: "GPT-5 Mini"},
		},
	}
	build := func(agent.Options) (provider.Provider, error) { return fake, nil }

	cmd := newModelsCommandWithBuilder(build)
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	got := out.String()
	for _, want := range []string{"ID", "NAME", "CONTEXT", "gpt-5", "GPT-5", "400000", "gpt-5-mini", "GPT-5 Mini"} {
		if !strings.Contains(got, want) {
			t.Fatalf("stdout = %q, want it to contain %q", got, want)
		}
	}
	if !strings.Contains(got, "-") {
		t.Fatalf("stdout = %q, want a %q placeholder for the zero-value context window", got, "-")
	}
}

func TestModelsCommand_PrintsTableWithPriceColumn(t *testing.T) {
	inCost := 0.15
	outCost := 0.6
	fake := &modelsFakeProvider{
		models: []provider.ModelInfo{
			{ID: "gpt-4o-mini", DisplayName: "GPT-4o mini", ContextWindow: 128000, InputCostPerMTok: &inCost, OutputCostPerMTok: &outCost},
			{ID: "custom-model", DisplayName: "Custom Model"},
		},
	}
	build := func(agent.Options) (provider.Provider, error) { return fake, nil }

	cmd := newModelsCommandWithBuilder(build)
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	got := out.String()
	if !strings.Contains(got, "PRICE") {
		t.Fatalf("stdout = %q, want a PRICE column header", got)
	}
	if !strings.Contains(got, "$0.15/$0.60") {
		t.Fatalf("stdout = %q, want the formatted price for gpt-4o-mini", got)
	}

	var customLine string
	for _, line := range strings.Split(strings.TrimRight(got, "\n"), "\n") {
		if strings.Contains(line, "custom-model") {
			customLine = line
		}
	}
	if customLine == "" {
		t.Fatalf("stdout = %q, want a row for custom-model", got)
	}
	if strings.Contains(customLine, "$0.00") || strings.Contains(customLine, "$0") {
		t.Fatalf("row for custom-model = %q, want no $0 price rendered for nil pricing", customLine)
	}
	if !strings.HasSuffix(strings.TrimRight(customLine, " "), "-") {
		t.Fatalf("row for custom-model = %q, want it to end in the %q placeholder for nil pricing", customLine, "-")
	}
}

func TestModelsCommand_EmptyListPrintsNoModelsAvailable(t *testing.T) {
	fake := &modelsFakeProvider{models: nil}
	build := func(agent.Options) (provider.Provider, error) { return fake, nil }

	cmd := newModelsCommandWithBuilder(build)
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if got, want := out.String(), "No models available.\n"; got != want {
		t.Fatalf("stdout = %q, want %q", got, want)
	}
}

func TestModelsCommand_ProviderModelsErrorPropagates(t *testing.T) {
	wantErr := errors.New("models: boom")
	fake := &modelsFakeProvider{modelsErr: wantErr}
	build := func(agent.Options) (provider.Provider, error) { return fake, nil }

	cmd := newModelsCommandWithBuilder(build)
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))

	err := cmd.Execute()
	if err == nil {
		t.Fatal("Execute() error = nil, want the provider's Models error to propagate")
	}
	if !errors.Is(err, wantErr) {
		t.Fatalf("Execute() error = %v, want it to wrap %v", err, wantErr)
	}
	if strings.Contains(out.String(), "ID") && strings.Contains(out.String(), "CONTEXT") {
		t.Fatalf("stdout = %q, want no models table printed when Models errors", out.String())
	}
}

func TestModelsCommand_BuilderErrorPropagates(t *testing.T) {
	wantErr := errors.New("models: no credentials found")
	build := func(agent.Options) (provider.Provider, error) { return nil, wantErr }

	cmd := newModelsCommandWithBuilder(build)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))

	err := cmd.Execute()
	if err == nil {
		t.Fatal("Execute() error = nil, want the builder error to propagate")
	}
	if !errors.Is(err, wantErr) {
		t.Fatalf("Execute() error = %v, want it to wrap %v", err, wantErr)
	}
}

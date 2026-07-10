package modelregistry

import (
	"context"
	"errors"
	"testing"

	"github.com/0xErwin1/agens/internal/provider"
)

// fakeProvider is a minimal provider.Provider test double: Models, Stream,
// ID, and EffortLevels all return scripted values so a test can assert the
// enrichment decorator's merge behavior and passthrough separately.
type fakeProvider struct {
	id           string
	effortLevels []string
	models       []provider.ModelInfo
	modelsErr    error
	streamErr    error
}

var _ provider.Provider = (*fakeProvider)(nil)

func (p *fakeProvider) ID() string { return p.id }

func (p *fakeProvider) Models(context.Context) ([]provider.ModelInfo, error) {
	return p.models, p.modelsErr
}

func (p *fakeProvider) EffortLevels() []string { return p.effortLevels }

func (p *fakeProvider) Stream(context.Context, provider.ChatRequest) (provider.StreamReader, error) {
	return nil, p.streamErr
}

func TestEnrich_MatchingIDFillsPricingAndZeroLimits(t *testing.T) {
	fake := &fakeProvider{models: []provider.ModelInfo{
		{ID: "gpt-4o-mini", DisplayName: "GPT-4o mini"},
	}}

	got, err := Enrich(fake).Models(context.Background())
	if err != nil {
		t.Fatalf("Models() error = %v, want nil", err)
	}
	if len(got) != 1 {
		t.Fatalf("Models() returned %d models, want 1", len(got))
	}

	m := got[0]
	if m.ContextWindow != 128000 {
		t.Fatalf("ContextWindow = %d, want %d", m.ContextWindow, 128000)
	}
	if m.MaxOutputTokens != 16384 {
		t.Fatalf("MaxOutputTokens = %d, want %d", m.MaxOutputTokens, 16384)
	}
	if m.InputCostPerMTok == nil || *m.InputCostPerMTok != 0.15 {
		t.Fatalf("InputCostPerMTok = %v, want 0.15", m.InputCostPerMTok)
	}
	if m.OutputCostPerMTok == nil || *m.OutputCostPerMTok != 0.6 {
		t.Fatalf("OutputCostPerMTok = %v, want 0.6", m.OutputCostPerMTok)
	}
}

func TestEnrich_NoMatchKeepsPricingNilAndStillLists(t *testing.T) {
	fake := &fakeProvider{models: []provider.ModelInfo{
		{ID: "some-unknown-model", DisplayName: "Unknown"},
	}}

	got, err := Enrich(fake).Models(context.Background())
	if err != nil {
		t.Fatalf("Models() error = %v, want nil", err)
	}
	if len(got) != 1 {
		t.Fatalf("Models() returned %d models, want 1 (enrichment must never drop models)", len(got))
	}
	if got[0].InputCostPerMTok != nil || got[0].OutputCostPerMTok != nil {
		t.Fatalf("pricing = (%v, %v), want both nil for a snapshot miss", got[0].InputCostPerMTok, got[0].OutputCostPerMTok)
	}
}

func TestEnrich_SnapshotOnlyEntryNeverSurfaced(t *testing.T) {
	fake := &fakeProvider{models: nil}

	got, err := Enrich(fake).Models(context.Background())
	if err != nil {
		t.Fatalf("Models() error = %v, want nil", err)
	}
	if len(got) != 0 {
		t.Fatalf("Models() = %v, want empty: a snapshot entry with no live model must never be surfaced", got)
	}
}

func TestEnrich_LiveNonZeroContextWindowNotClobbered(t *testing.T) {
	fake := &fakeProvider{models: []provider.ModelInfo{
		{ID: "gpt-4o-mini", DisplayName: "GPT-4o mini", ContextWindow: 200000, MaxOutputTokens: 8192},
	}}

	got, err := Enrich(fake).Models(context.Background())
	if err != nil {
		t.Fatalf("Models() error = %v, want nil", err)
	}

	m := got[0]
	if m.ContextWindow != 200000 {
		t.Fatalf("ContextWindow = %d, want %d (live value must not be clobbered)", m.ContextWindow, 200000)
	}
	if m.MaxOutputTokens != 8192 {
		t.Fatalf("MaxOutputTokens = %d, want %d (live value must not be clobbered)", m.MaxOutputTokens, 8192)
	}
	if m.InputCostPerMTok == nil || *m.InputCostPerMTok != 0.15 {
		t.Fatalf("InputCostPerMTok = %v, want 0.15 (pricing always fills on a hit, live has no pricing source)", m.InputCostPerMTok)
	}
}

func TestEnrich_PassthroughMethodsUnchanged(t *testing.T) {
	wantModelsErr := errors.New("models: boom")
	wantStreamErr := errors.New("stream: boom")
	fake := &fakeProvider{
		id:           "fake-provider",
		effortLevels: []string{"low", "high"},
		modelsErr:    wantModelsErr,
		streamErr:    wantStreamErr,
	}
	p := Enrich(fake)

	if got := p.ID(); got != "fake-provider" {
		t.Fatalf("ID() = %q, want %q", got, "fake-provider")
	}
	if got := p.EffortLevels(); len(got) != 2 || got[0] != "low" || got[1] != "high" {
		t.Fatalf("EffortLevels() = %v, want [low high]", got)
	}
	if _, err := p.Models(context.Background()); !errors.Is(err, wantModelsErr) {
		t.Fatalf("Models() error = %v, want %v", err, wantModelsErr)
	}
	if _, err := p.Stream(context.Background(), provider.ChatRequest{}); !errors.Is(err, wantStreamErr) {
		t.Fatalf("Stream() error = %v, want %v", err, wantStreamErr)
	}
}

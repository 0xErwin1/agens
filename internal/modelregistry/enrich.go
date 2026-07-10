package modelregistry

import (
	"context"

	"github.com/0xErwin1/agens/internal/provider"
)

// enrichedProvider decorates a provider.Provider. Every method is passed
// through unchanged via interface embedding except Models, which merges
// curated snapshot metadata onto the live catalog by id. It performs no I/O
// of its own and cannot fail on its own account.
type enrichedProvider struct {
	provider.Provider
}

// Enrich wraps p so its Models() results are additively enriched with
// curated snapshot metadata: pricing always fills on an id match (live
// providers report no pricing of their own), while ContextWindow and
// MaxOutputTokens fill only when the live value is still zero, so a value
// the live provider already reported is never clobbered. A model absent
// from the snapshot is returned unchanged, never dropped; live Models()
// remains the sole source of truth for which models exist.
func Enrich(p provider.Provider) provider.Provider {
	return enrichedProvider{Provider: p}
}

// Models implements provider.Provider.
func (e enrichedProvider) Models(ctx context.Context) ([]provider.ModelInfo, error) {
	models, err := e.Provider.Models(ctx)
	if err != nil {
		return nil, err
	}

	enriched := make([]provider.ModelInfo, len(models))
	for i, m := range models {
		enriched[i] = mergeMetadata(m)
	}
	return enriched, nil
}

// mergeMetadata fills m's empty fields from the curated snapshot entry for
// m.ID, if one exists. Pricing has no live source, so it is always taken
// from the snapshot on a hit; ContextWindow and MaxOutputTokens are filled
// only when the live value is still zero.
func mergeMetadata(m provider.ModelInfo) provider.ModelInfo {
	meta, ok := Lookup(m.ID)
	if !ok {
		return m
	}

	if m.ContextWindow == 0 {
		m.ContextWindow = meta.ContextWindow
	}
	if m.MaxOutputTokens == 0 {
		m.MaxOutputTokens = meta.MaxOutputTokens
	}

	inputCost := meta.InputCostPerMTok
	outputCost := meta.OutputCostPerMTok
	m.InputCostPerMTok = &inputCost
	m.OutputCostPerMTok = &outputCost

	return m
}

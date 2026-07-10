// Package modelregistry holds a hand-curated, embedded snapshot of the
// models.dev catalog (https://models.dev/api.json, "openai" provider bucket)
// and exposes it as an additive metadata lookup. It performs no network I/O
// and defines no provider transport; enrich.go is the sole consumer, merging
// this metadata onto a live provider.Provider's Models() results.
//
// Source: https://models.dev/api.json — last refreshed: 2026-07-10. The
// snapshot is vendored by hand; there is no go:generate step, so refreshing
// it means re-copying the relevant entries and updating this date.
package modelregistry

import (
	_ "embed"
	"encoding/json"
	"fmt"
)

// Metadata is the curated per-model data this package can supply: the
// subset of the models.dev catalog agens actually consumes.
type Metadata struct {
	ID                string
	Name              string
	ContextWindow     int
	MaxOutputTokens   int
	InputCostPerMTok  float64
	OutputCostPerMTok float64
}

// snapshotEntry is snapshot.json's on-disk shape: a flat array keyed by
// short field names, not models.dev's nested provider/limit/cost shape, to
// keep the embedded asset small and the parser trivial.
type snapshotEntry struct {
	ID         string  `json:"id"`
	Name       string  `json:"name"`
	Context    int     `json:"context"`
	Output     int     `json:"output"`
	InputCost  float64 `json:"input_cost"`
	OutputCost float64 `json:"output_cost"`
}

//go:embed snapshot.json
var snapshotJSON []byte

var registry = mustLoad(snapshotJSON)

// mustLoad decodes raw into a lookup map, panicking on malformed JSON. raw
// is compiled into the binary via go:embed, so a decode failure can only
// mean a broken build artifact rather than a runtime condition — fail fast
// at init, mirroring regexp.MustCompile.
func mustLoad(raw []byte) map[string]Metadata {
	var entries []snapshotEntry
	if err := json.Unmarshal(raw, &entries); err != nil {
		panic(fmt.Sprintf("modelregistry: malformed embedded snapshot.json: %v", err))
	}

	m := make(map[string]Metadata, len(entries))
	for _, e := range entries {
		m[e.ID] = Metadata{
			ID:                e.ID,
			Name:              e.Name,
			ContextWindow:     e.Context,
			MaxOutputTokens:   e.Output,
			InputCostPerMTok:  e.InputCost,
			OutputCostPerMTok: e.OutputCost,
		}
	}
	return m
}

// Lookup returns the curated metadata for id and whether an entry was
// found. A miss returns the zero Metadata and false — never an error.
func Lookup(id string) (Metadata, bool) {
	m, ok := registry[id]
	return m, ok
}

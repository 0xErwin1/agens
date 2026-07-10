package modelregistry

import "testing"

func TestLookup_HitReturnsMetadata(t *testing.T) {
	got, found := Lookup("gpt-4o-mini")
	if !found {
		t.Fatal(`Lookup("gpt-4o-mini") found = false, want true`)
	}
	if got.ID != "gpt-4o-mini" {
		t.Fatalf("Lookup().ID = %q, want %q", got.ID, "gpt-4o-mini")
	}
	if got.ContextWindow != 128000 {
		t.Fatalf("Lookup().ContextWindow = %d, want %d", got.ContextWindow, 128000)
	}
	if got.MaxOutputTokens != 16384 {
		t.Fatalf("Lookup().MaxOutputTokens = %d, want %d", got.MaxOutputTokens, 16384)
	}
	if got.InputCostPerMTok != 0.15 {
		t.Fatalf("Lookup().InputCostPerMTok = %v, want %v", got.InputCostPerMTok, 0.15)
	}
	if got.OutputCostPerMTok != 0.6 {
		t.Fatalf("Lookup().OutputCostPerMTok = %v, want %v", got.OutputCostPerMTok, 0.6)
	}
}

func TestLookup_MissReturnsZeroValueAndFalse(t *testing.T) {
	got, found := Lookup("does-not-exist-in-snapshot")
	if found {
		t.Fatal(`Lookup("does-not-exist-in-snapshot") found = true, want false`)
	}
	if got != (Metadata{}) {
		t.Fatalf("Lookup() on a miss = %+v, want the zero Metadata", got)
	}
}

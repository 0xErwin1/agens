package permission

import (
	"context"
	"testing"
)

func TestNewMemoryStore_NeverErrors(t *testing.T) {
	store := NewMemoryStore()

	if _, err := store.Rules(context.Background()); err != nil {
		t.Fatalf("Rules() error = %v, want nil", err)
	}
	if err := store.Append(context.Background(), Rule{Decision: DecisionAllow, Name: "bash"}); err != nil {
		t.Fatalf("Append() error = %v, want nil", err)
	}
}

func TestMemoryStore_AppendThenRulesReturnsInOrder(t *testing.T) {
	store := NewMemoryStore()
	ctx := context.Background()

	first := Rule{Decision: DecisionAllow, Name: "bash"}
	second := Rule{Decision: DecisionDeny, Name: "fs_write"}

	if err := store.Append(ctx, first); err != nil {
		t.Fatalf("Append(first) error = %v", err)
	}
	if err := store.Append(ctx, second); err != nil {
		t.Fatalf("Append(second) error = %v", err)
	}

	got, err := store.Rules(ctx)
	if err != nil {
		t.Fatalf("Rules() error = %v", err)
	}
	want := []Rule{first, second}
	if len(got) != len(want) || got[0] != want[0] || got[1] != want[1] {
		t.Fatalf("Rules() = %+v, want %+v (append order preserved)", got, want)
	}
}

func TestMemoryStore_RulesReturnsACopy(t *testing.T) {
	store := NewMemoryStore()
	ctx := context.Background()

	if err := store.Append(ctx, Rule{Decision: DecisionAllow, Name: "bash"}); err != nil {
		t.Fatalf("Append() error = %v", err)
	}

	got, err := store.Rules(ctx)
	if err != nil {
		t.Fatalf("Rules() error = %v", err)
	}
	got[0] = Rule{Decision: DecisionDeny, Name: "mutated"}

	after, err := store.Rules(ctx)
	if err != nil {
		t.Fatalf("Rules() error = %v", err)
	}
	if after[0].Name != "bash" {
		t.Fatalf("Rules() = %+v after caller mutation, want the store's internal state unaffected", after)
	}
}

var _ Store = (*MemoryStore)(nil)

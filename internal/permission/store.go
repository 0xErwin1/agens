package permission

import (
	"context"
	"sync"
)

// Store persists Rules synthesized from "always" Prompter answers. Rules
// returned by a Store are evaluated after an Engine's static rules, so
// last-match-wins lets a stored rule override a static one.
type Store interface {
	Append(ctx context.Context, r Rule) error
	Rules(ctx context.Context) ([]Rule, error)
}

// MemoryStore is an in-memory, concurrency-safe Store: rules live only for
// the process's lifetime and are lost on restart.
type MemoryStore struct {
	mu    sync.Mutex
	rules []Rule
}

// NewMemoryStore returns an empty, ready-to-use MemoryStore. It never
// errors.
func NewMemoryStore() *MemoryStore {
	return &MemoryStore{}
}

func (s *MemoryStore) Append(ctx context.Context, r Rule) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	s.rules = append(s.rules, r)
	return nil
}

// Rules returns a copy of the current rules, in append order, so the
// caller cannot mutate the store's internal state through the returned
// slice.
func (s *MemoryStore) Rules(ctx context.Context) ([]Rule, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	rules := make([]Rule, len(s.rules))
	copy(rules, s.rules)
	return rules, nil
}

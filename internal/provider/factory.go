package provider

import (
	"errors"
	"fmt"
	"time"
)

// Config carries the non-auth inputs a factory needs. It is self-contained:
// mapping TOML (internal/config) into this type is future wiring work owned
// by a composition root, never by this package.
type Config struct {
	BaseURL     string        // endpoint override; empty = provider default
	Model       string        // default model id
	HTTPTimeout time.Duration // zero = provider default
}

// ProviderFactory builds a Provider from resolved config and an
// authenticator. Registered under a stable id in a Registry.
type ProviderFactory func(cfg Config, auth Authenticator) (Provider, error)

var (
	// ErrUnknownProvider is wrapped by Get when no factory is registered
	// under the requested id.
	ErrUnknownProvider = errors.New("unknown provider")

	// ErrDuplicateProvider is wrapped by Register when the requested id is
	// already taken.
	ErrDuplicateProvider = errors.New("provider already registered")
)

// Registry maps provider ids to factories. Wiring is explicit: a composition
// root calls NewRegistry and Register at startup — there is no
// package-level registry and no init() registration.
//
// Registry is NOT safe for concurrent use; populate it before serving and
// treat it as read-only afterwards.
type Registry struct {
	factories map[string]ProviderFactory
}

// NewRegistry returns an empty, ready-to-use Registry.
func NewRegistry() *Registry {
	return &Registry{factories: make(map[string]ProviderFactory)}
}

// Register adds f under id. It returns an error for an empty id or a nil f,
// and an error wrapping ErrDuplicateProvider if id is already registered;
// in every error case the existing registration, if any, is left untouched.
func (r *Registry) Register(id string, f ProviderFactory) error {
	if id == "" {
		return errors.New("provider: empty id")
	}
	if f == nil {
		return fmt.Errorf("provider: nil factory for id %q", id)
	}
	if _, exists := r.factories[id]; exists {
		return fmt.Errorf("provider %q: %w", id, ErrDuplicateProvider)
	}

	r.factories[id] = f
	return nil
}

// Get returns the factory registered under id, or an error wrapping
// ErrUnknownProvider if none is registered.
func (r *Registry) Get(id string) (ProviderFactory, error) {
	f, ok := r.factories[id]
	if !ok {
		return nil, fmt.Errorf("provider %q: %w", id, ErrUnknownProvider)
	}

	return f, nil
}

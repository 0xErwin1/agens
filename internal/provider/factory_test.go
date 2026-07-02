package provider

import (
	"errors"
	"testing"
)

// fakeFactory returns a distinct *fakeProvider so tests can assert identity
// across Register/Get round-trips.
func fakeFactory(_ Config, _ Authenticator) (Provider, error) {
	return &fakeProvider{id: "fake"}, nil
}

func otherFakeFactory(_ Config, _ Authenticator) (Provider, error) {
	return &fakeProvider{id: "other"}, nil
}

func TestRegistryRegisterAndGet(t *testing.T) {
	r := NewRegistry()

	if err := r.Register("openai-api", fakeFactory); err != nil {
		t.Fatalf("Register() error = %v, want nil", err)
	}

	got, err := r.Get("openai-api")
	if err != nil {
		t.Fatalf("Get() error = %v, want nil", err)
	}

	provider, err := got(Config{}, &fakeAuthenticator{})
	if err != nil {
		t.Fatalf("factory() error = %v, want nil", err)
	}
	if provider.ID() != "fake" {
		t.Fatalf("Get() returned factory producing ID() = %q, want %q", provider.ID(), "fake")
	}
}

func TestRegistryGetUnknownID(t *testing.T) {
	r := NewRegistry()

	_, err := r.Get("nope")
	if err == nil {
		t.Fatal("Get() error = nil, want non-nil")
	}
	if !errors.Is(err, ErrUnknownProvider) {
		t.Fatalf("Get() error = %v, want wrapping ErrUnknownProvider", err)
	}
}

func TestRegistryRegisterDuplicateID(t *testing.T) {
	r := NewRegistry()

	if err := r.Register("openai-api", fakeFactory); err != nil {
		t.Fatalf("Register() error = %v, want nil", err)
	}

	err := r.Register("openai-api", otherFakeFactory)
	if err == nil {
		t.Fatal("Register() error = nil, want non-nil")
	}
	if !errors.Is(err, ErrDuplicateProvider) {
		t.Fatalf("Register() error = %v, want wrapping ErrDuplicateProvider", err)
	}

	got, err := r.Get("openai-api")
	if err != nil {
		t.Fatalf("Get() error = %v, want nil", err)
	}
	provider, err := got(Config{}, &fakeAuthenticator{})
	if err != nil {
		t.Fatalf("factory() error = %v, want nil", err)
	}
	if provider.ID() != "fake" {
		t.Fatalf("Get() returned factory producing ID() = %q, want original %q (must not overwrite)", provider.ID(), "fake")
	}
}

func TestRegistryRegisterEmptyID(t *testing.T) {
	r := NewRegistry()

	if err := r.Register("", fakeFactory); err == nil {
		t.Fatal("Register() error = nil, want non-nil for empty id")
	}
}

func TestRegistryRegisterNilFactory(t *testing.T) {
	r := NewRegistry()

	if err := r.Register("openai-api", nil); err == nil {
		t.Fatal("Register() error = nil, want non-nil for nil factory")
	}
}

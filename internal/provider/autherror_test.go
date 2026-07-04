package provider

import (
	"errors"
	"fmt"
	"testing"
)

// stubAuthError is a test double whose auth classification is controlled by
// the auth field, standing in for a concrete provider error.
type stubAuthError struct {
	auth bool
}

func (e stubAuthError) Error() string     { return "stub auth error" }
func (e stubAuthError) IsAuthError() bool { return e.auth }

func TestIsAuthError(t *testing.T) {
	cases := []struct {
		name string
		err  error
		want bool
	}{
		{"nil", nil, false},
		{"plain error", errors.New("boom"), false},
		{"auth error", stubAuthError{auth: true}, true},
		{"non-auth classifier", stubAuthError{auth: false}, false},
		{"wrapped auth error", fmt.Errorf("open stream: %w", stubAuthError{auth: true}), true},
		{"wrapped non-auth", fmt.Errorf("open stream: %w", stubAuthError{auth: false}), false},
	}

	for _, c := range cases {
		if got := IsAuthError(c.err); got != c.want {
			t.Fatalf("IsAuthError(%q) = %v, want %v", c.name, got, c.want)
		}
	}
}

package openai

import (
	"context"
	"errors"
	"net/http"
	"time"

	"github.com/iperez/agens/internal/provider"
)

var _ provider.Authenticator = (*APIKeyAuthenticator)(nil)

// APIKeyAuthenticator authenticates chat-completions requests with a static
// OpenAI API key. It never expires: Valid always reports true and Refresh is
// a no-op, unlike the rotating-token authenticator owned by AGN-6.
type APIKeyAuthenticator struct {
	key string
}

// NewAPIKeyAuthenticator returns an APIKeyAuthenticator for key. It returns
// an error if key is empty.
func NewAPIKeyAuthenticator(key string) (*APIKeyAuthenticator, error) {
	if key == "" {
		return nil, errors.New("openai: API key must not be empty")
	}

	return &APIKeyAuthenticator{key: key}, nil
}

// Decorate implements provider.Authenticator. It is safe for concurrent use:
// it only reads the immutable key.
func (a *APIKeyAuthenticator) Decorate(_ context.Context, req *http.Request) error {
	req.Header.Set("Authorization", "Bearer "+a.key)
	return nil
}

// Valid implements provider.Authenticator. A static API key never expires.
func (a *APIKeyAuthenticator) Valid(_ time.Time) bool {
	return true
}

// Refresh implements provider.Authenticator. A static API key has nothing to
// renew.
func (a *APIKeyAuthenticator) Refresh(_ context.Context) error {
	return nil
}

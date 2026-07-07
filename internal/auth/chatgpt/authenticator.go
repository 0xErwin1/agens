package chatgpt

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"

	"github.com/0xErwin1/agens/internal/auth"
	"github.com/0xErwin1/agens/internal/provider"
)

var _ provider.Authenticator = (*Authenticator)(nil)

// refreshSkew is the proactive refresh window: Valid reports false once the
// access token is within refreshSkew of ExpiresAt, so callers refresh ahead
// of the token's actual expiry rather than racing an in-flight request
// against it.
const refreshSkew = 5 * time.Minute

// PersistFunc persists a refreshed auth.Entry, typically by saving it under
// the "openai-chatgpt" provider key in the on-disk credentials file.
type PersistFunc func(auth.Entry) error

// Authenticator implements provider.Authenticator for the ChatGPT OAuth
// credential. It decorates outgoing requests with the current access token
// and account id, and renews the credential via the refresh_token grant
// when it is close to expiry, persisting any renewal through persist.
type Authenticator struct {
	mu      sync.Mutex
	entry   auth.Entry
	persist PersistFunc
	client  *http.Client
	now     func() time.Time
}

// NewAuthenticator returns an Authenticator seeded with entry, persisting
// any refreshed credential via persist. It returns an error if entry has no
// RefreshToken or persist is nil, since Refresh cannot renew the credential
// without either.
func NewAuthenticator(entry auth.Entry, persist PersistFunc) (*Authenticator, error) {
	if entry.RefreshToken == "" {
		return nil, errors.New("chatgpt: authenticator requires a refresh token")
	}
	if persist == nil {
		return nil, errors.New("chatgpt: authenticator requires a persist func")
	}

	return &Authenticator{
		entry:   entry,
		persist: persist,
		client:  http.DefaultClient,
		now:     time.Now,
	}, nil
}

// Valid implements provider.Authenticator.
func (a *Authenticator) Valid(now time.Time) bool {
	a.mu.Lock()
	defer a.mu.Unlock()
	return a.validLocked(now)
}

func (a *Authenticator) validLocked(now time.Time) bool {
	if a.entry.AccessToken == "" || a.entry.ExpiresAt == nil {
		return false
	}
	return now.Before(a.entry.ExpiresAt.Add(-refreshSkew))
}

// Decorate implements provider.Authenticator. It attaches the current
// access token and ChatGPT account id; it never triggers a refresh itself,
// so callers must check Valid and call Refresh beforehand when needed.
func (a *Authenticator) Decorate(_ context.Context, req *http.Request) error {
	a.mu.Lock()
	accessToken := a.entry.AccessToken
	accountID := a.entry.AccountID
	a.mu.Unlock()

	req.Header.Set("Authorization", "Bearer "+accessToken)
	req.Header.Set("ChatGPT-Account-ID", accountID)
	return nil
}

// Refresh implements provider.Authenticator. It exchanges the stored
// refresh token for a new access token via the refresh_token grant.
//
// Refresh holds mu for its full duration and re-checks Valid immediately
// after acquiring it: if a concurrent caller already renewed the credential
// while this call was waiting on the lock, it returns nil without a second
// token request.
func (a *Authenticator) Refresh(ctx context.Context) error {
	a.mu.Lock()
	defer a.mu.Unlock()

	if a.validLocked(a.now()) {
		return nil
	}

	form := url.Values{
		"grant_type":    {"refresh_token"},
		"client_id":     {clientID},
		"refresh_token": {a.entry.RefreshToken},
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, tokenEndpoint, strings.NewReader(form.Encode()))
	if err != nil {
		return fmt.Errorf("chatgpt: build refresh request: %w", err)
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")

	resp, err := a.client.Do(req)
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return ctxErr
		}
		return fmt.Errorf("chatgpt: send refresh request: %w", err)
	}
	defer func() { _ = resp.Body.Close() }()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return fmt.Errorf("chatgpt: read refresh response: %w", err)
	}

	if resp.StatusCode < http.StatusOK || resp.StatusCode >= http.StatusMultipleChoices {
		return &tokenError{statusCode: resp.StatusCode, reason: "non-2xx refresh response"}
	}

	var wire tokenResponse
	if err := json.Unmarshal(body, &wire); err != nil {
		return &tokenError{statusCode: resp.StatusCode, reason: "refresh response body is not valid JSON"}
	}
	if wire.AccessToken == "" {
		return &tokenError{statusCode: resp.StatusCode, reason: "refresh response missing access_token"}
	}

	updated := a.entry
	updated.AccessToken = wire.AccessToken
	if wire.RefreshToken != "" {
		updated.RefreshToken = wire.RefreshToken
	}
	if wire.IDToken != "" {
		accountID, err := parseAccountID(wire.IDToken)
		if err != nil {
			return fmt.Errorf("chatgpt: resolve refreshed account id: %w", err)
		}
		updated.AccountID = accountID
	}
	expiresAt := a.now().Add(time.Duration(wire.ExpiresIn) * time.Second)
	updated.ExpiresAt = &expiresAt

	a.entry = updated

	// The in-memory entry is updated before persist runs: if persist fails,
	// the caller learns about it via the returned error instead of silently
	// keeping stale credentials, since the server may already have rotated
	// the refresh token by the time persist is called.
	if err := a.persist(a.entry); err != nil {
		return fmt.Errorf("chatgpt: persist refreshed credential: %w", err)
	}

	return nil
}

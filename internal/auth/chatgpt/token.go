package chatgpt

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/iperez/agens/internal/auth"
)

// OAuth endpoint and client configuration for the ChatGPT PKCE login flow,
// grounded against openai/codex @98d28aab.
const (
	authorizeURL = "https://auth.openai.com/oauth/authorize"
	tokenURL     = "https://auth.openai.com/oauth/token"
	clientID     = "app_EMoamEEZ73f0CkXaXp7hrann"
	scope        = "openid profile email offline_access"

	// redirectURITemplate is formatted with the loopback server's bound
	// port to build the redirect_uri sent to authorizeURL and tokenURL.
	redirectURITemplate = "http://localhost:%d/auth/callback"

	// pkceVerifierBytes is the byte length of the random PKCE
	// code_verifier before base64url encoding.
	pkceVerifierBytes = 64

	// stateBytes is the byte length of the random OAuth state value
	// before base64url encoding.
	stateBytes = 32
)

// authorizeExtraParams are additional authorize-request query parameters
// beyond the standard OAuth/PKCE ones, grounded against openai/codex
// @98d28aab.
var authorizeExtraParams = map[string]string{
	"id_token_add_organizations": "true",
	"codex_cli_simplified_flow":  "true",
	"originator":                 "codex_cli_rs",
	"response_type":              "code",
	"code_challenge_method":      "S256",
}

// pkce holds a generated PKCE code verifier/challenge pair.
type pkce struct {
	verifier  string
	challenge string
}

// generatePKCE creates a new PKCE verifier/challenge pair per RFC 7636: the
// verifier is pkceVerifierBytes of crypto/rand, base64url-encoded without
// padding; the challenge is the base64url (no padding) encoding of
// sha256(verifier), matching the S256 method.
func generatePKCE() (pkce, error) {
	verifierBytes := make([]byte, pkceVerifierBytes)
	if _, err := rand.Read(verifierBytes); err != nil {
		return pkce{}, fmt.Errorf("chatgpt: generate PKCE verifier: %w", err)
	}
	verifier := base64.RawURLEncoding.EncodeToString(verifierBytes)

	sum := sha256.Sum256([]byte(verifier))
	challenge := base64.RawURLEncoding.EncodeToString(sum[:])

	return pkce{verifier: verifier, challenge: challenge}, nil
}

// generateState creates a new random OAuth state value: stateBytes of
// crypto/rand, base64url-encoded without padding.
func generateState() (string, error) {
	raw := make([]byte, stateBytes)
	if _, err := rand.Read(raw); err != nil {
		return "", fmt.Errorf("chatgpt: generate state: %w", err)
	}
	return base64.RawURLEncoding.EncodeToString(raw), nil
}

// tokenResponse is the JSON body returned by the token endpoint on a
// successful authorization_code exchange.
type tokenResponse struct {
	AccessToken  string `json:"access_token"`
	RefreshToken string `json:"refresh_token"`
	IDToken      string `json:"id_token"`
	ExpiresIn    int64  `json:"expires_in"`
}

// tokenError reports a non-2xx or malformed response from the token
// endpoint. It never includes the request or response body, so it is safe
// to log without leaking the authorization code, PKCE verifier, or any
// issued token.
type tokenError struct {
	statusCode int
	reason     string
}

func (e *tokenError) Error() string {
	return fmt.Sprintf("chatgpt: token exchange failed: HTTP %d: %s", e.statusCode, e.reason)
}

// IsAuthError classifies every token-exchange failure as an authentication
// error: a failed authorization-code exchange or refresh means the stored
// credential is unusable and the user must sign in again, regardless of the
// specific status code (an expired refresh token commonly returns 400).
func (e *tokenError) IsAuthError() bool { return true }

// exchangeCode performs the authorization_code exchange against
// tokenEndpoint and maps the response into an auth.Entry. now supplies the
// current time so Entry.ExpiresAt is deterministic in tests.
func exchangeCode(ctx context.Context, httpClient *http.Client, tokenEndpoint, code, redirectURI, verifier string, now func() time.Time) (auth.Entry, error) {
	form := url.Values{
		"grant_type":    {"authorization_code"},
		"code":          {code},
		"redirect_uri":  {redirectURI},
		"client_id":     {clientID},
		"code_verifier": {verifier},
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, tokenEndpoint, strings.NewReader(form.Encode()))
	if err != nil {
		return auth.Entry{}, fmt.Errorf("chatgpt: build token request: %w", err)
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")

	resp, err := httpClient.Do(req)
	if err != nil {
		return auth.Entry{}, fmt.Errorf("chatgpt: send token request: %w", err)
	}
	defer func() { _ = resp.Body.Close() }()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return auth.Entry{}, fmt.Errorf("chatgpt: read token response: %w", err)
	}

	if resp.StatusCode < http.StatusOK || resp.StatusCode >= http.StatusMultipleChoices {
		return auth.Entry{}, &tokenError{statusCode: resp.StatusCode, reason: "non-2xx response"}
	}

	var wire tokenResponse
	if err := json.Unmarshal(body, &wire); err != nil {
		return auth.Entry{}, &tokenError{statusCode: resp.StatusCode, reason: "response body is not valid JSON"}
	}
	if wire.AccessToken == "" || wire.IDToken == "" {
		return auth.Entry{}, &tokenError{statusCode: resp.StatusCode, reason: "response missing access_token or id_token"}
	}

	accountID, err := parseAccountID(wire.IDToken)
	if err != nil {
		return auth.Entry{}, fmt.Errorf("chatgpt: resolve account id: %w", err)
	}

	expiresAt := now().Add(time.Duration(wire.ExpiresIn) * time.Second)

	return auth.Entry{
		AccessToken:  wire.AccessToken,
		RefreshToken: wire.RefreshToken,
		AccountID:    accountID,
		ExpiresAt:    &expiresAt,
	}, nil
}

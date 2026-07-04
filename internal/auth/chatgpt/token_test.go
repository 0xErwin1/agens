package chatgpt

import (
	"context"
	"crypto/sha256"
	"encoding/base64"
	"errors"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"
	"time"
)

func fakeIDToken(t *testing.T, accountID string) string {
	t.Helper()
	header := base64.RawURLEncoding.EncodeToString([]byte(`{"alg":"none"}`))
	payload := base64.RawURLEncoding.EncodeToString([]byte(`{"chatgpt_account_id":"` + accountID + `"}`))
	return header + "." + payload + ".sig"
}

func TestGroundedOAuthConstants(t *testing.T) {
	if authorizeURL != "https://auth.openai.com/oauth/authorize" {
		t.Fatalf("authorizeURL = %q, want %q", authorizeURL, "https://auth.openai.com/oauth/authorize")
	}
	if tokenURL != "https://auth.openai.com/oauth/token" {
		t.Fatalf("tokenURL = %q, want %q", tokenURL, "https://auth.openai.com/oauth/token")
	}
	if clientID != "app_EMoamEEZ73f0CkXaXp7hrann" {
		t.Fatalf("clientID = %q, want %q", clientID, "app_EMoamEEZ73f0CkXaXp7hrann")
	}
	if scope != "openid profile email offline_access" {
		t.Fatalf("scope = %q, want %q", scope, "openid profile email offline_access")
	}
	if redirectURITemplate != "http://localhost:%d/auth/callback" {
		t.Fatalf("redirectURITemplate = %q, want %q", redirectURITemplate, "http://localhost:%d/auth/callback")
	}

	wantExtraParams := map[string]string{
		"id_token_add_organizations": "true",
		"codex_cli_simplified_flow":  "true",
		"originator":                 "codex_cli_rs",
		"response_type":              "code",
		"code_challenge_method":      "S256",
	}
	if len(authorizeExtraParams) != len(wantExtraParams) {
		t.Fatalf("authorizeExtraParams has %d entries, want %d", len(authorizeExtraParams), len(wantExtraParams))
	}
	for key, want := range wantExtraParams {
		if got := authorizeExtraParams[key]; got != want {
			t.Fatalf("authorizeExtraParams[%q] = %q, want %q", key, got, want)
		}
	}
}

func TestGeneratePKCE_VerifierFormat(t *testing.T) {
	p, err := generatePKCE()
	if err != nil {
		t.Fatalf("generatePKCE() error = %v", err)
	}

	decoded, err := base64.RawURLEncoding.DecodeString(p.verifier)
	if err != nil {
		t.Fatalf("verifier %q is not valid base64url: %v", p.verifier, err)
	}
	if len(decoded) != pkceVerifierBytes {
		t.Fatalf("decoded verifier length = %d, want %d", len(decoded), pkceVerifierBytes)
	}
	if strings.ContainsAny(p.verifier, "+/=") {
		t.Fatalf("verifier %q contains non-urlsafe or padding characters", p.verifier)
	}
}

func TestGeneratePKCE_S256Challenge(t *testing.T) {
	p, err := generatePKCE()
	if err != nil {
		t.Fatalf("generatePKCE() error = %v", err)
	}

	sum := sha256.Sum256([]byte(p.verifier))
	want := base64.RawURLEncoding.EncodeToString(sum[:])
	if p.challenge != want {
		t.Fatalf("challenge = %q, want %q", p.challenge, want)
	}
}

func TestGeneratePKCE_UniquePerCall(t *testing.T) {
	first, err := generatePKCE()
	if err != nil {
		t.Fatalf("generatePKCE() error = %v", err)
	}
	second, err := generatePKCE()
	if err != nil {
		t.Fatalf("generatePKCE() error = %v", err)
	}
	if first.verifier == second.verifier {
		t.Fatal("generatePKCE() produced the same verifier on two consecutive calls")
	}
}

func TestGenerateState_Format(t *testing.T) {
	state, err := generateState()
	if err != nil {
		t.Fatalf("generateState() error = %v", err)
	}

	decoded, err := base64.RawURLEncoding.DecodeString(state)
	if err != nil {
		t.Fatalf("state %q is not valid base64url: %v", state, err)
	}
	if len(decoded) != stateBytes {
		t.Fatalf("decoded state length = %d, want %d", len(decoded), stateBytes)
	}
}

func TestGenerateState_UniquePerCall(t *testing.T) {
	first, err := generateState()
	if err != nil {
		t.Fatalf("generateState() error = %v", err)
	}
	second, err := generateState()
	if err != nil {
		t.Fatalf("generateState() error = %v", err)
	}
	if first == second {
		t.Fatal("generateState() produced the same value on two consecutive calls")
	}
}

func TestExchangeCode_SendsExactFormFields(t *testing.T) {
	var gotForm url.Values
	var gotContentType string

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = r.ParseForm()
		gotForm = r.PostForm
		gotContentType = r.Header.Get("Content-Type")

		_, _ = w.Write([]byte(`{"access_token":"at-1","refresh_token":"rt-1","id_token":"` + fakeIDToken(t, "acct-fake") + `","expires_in":3600}`))
	}))
	defer server.Close()

	fixedNow := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	_, err := exchangeCode(context.Background(), server.Client(), server.URL, "auth-code-value", "http://localhost:1455/auth/callback", "verifier-value", func() time.Time { return fixedNow })
	if err != nil {
		t.Fatalf("exchangeCode() error = %v", err)
	}

	if gotContentType != "application/x-www-form-urlencoded" {
		t.Fatalf("Content-Type = %q, want application/x-www-form-urlencoded", gotContentType)
	}

	want := map[string]string{
		"grant_type":    "authorization_code",
		"code":          "auth-code-value",
		"redirect_uri":  "http://localhost:1455/auth/callback",
		"client_id":     clientID,
		"code_verifier": "verifier-value",
	}
	if len(gotForm) != len(want) {
		t.Fatalf("form has %d fields (%v), want exactly %d", len(gotForm), gotForm, len(want))
	}
	for key, wantValue := range want {
		if got := gotForm.Get(key); got != wantValue {
			t.Fatalf("form[%q] = %q, want %q", key, got, wantValue)
		}
	}
}

func TestExchangeCode_MapsResponseToEntry(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"access_token":"at-1","refresh_token":"rt-1","id_token":"` + fakeIDToken(t, "acct-fake") + `","expires_in":120}`))
	}))
	defer server.Close()

	fixedNow := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	entry, err := exchangeCode(context.Background(), server.Client(), server.URL, "code", "redirect", "verifier", func() time.Time { return fixedNow })
	if err != nil {
		t.Fatalf("exchangeCode() error = %v", err)
	}

	if entry.AccessToken != "at-1" {
		t.Fatalf("AccessToken = %q, want %q", entry.AccessToken, "at-1")
	}
	if entry.RefreshToken != "rt-1" {
		t.Fatalf("RefreshToken = %q, want %q", entry.RefreshToken, "rt-1")
	}
	if entry.AccountID != "acct-fake" {
		t.Fatalf("AccountID = %q, want %q", entry.AccountID, "acct-fake")
	}

	wantExpiry := fixedNow.Add(120 * time.Second)
	if entry.ExpiresAt == nil || !entry.ExpiresAt.Equal(wantExpiry) {
		t.Fatalf("ExpiresAt = %v, want %v", entry.ExpiresAt, wantExpiry)
	}
}

func assertTokenError(t *testing.T, err error) {
	t.Helper()
	if err == nil {
		t.Fatal("exchangeCode() error = nil, want error")
	}
	var terr *tokenError
	if !errors.As(err, &terr) {
		t.Fatalf("exchangeCode() error type = %T, want *tokenError", err)
	}
}

func TestExchangeCode_NonJSONResponseIsTypedError(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte("not json"))
	}))
	defer server.Close()

	_, err := exchangeCode(context.Background(), server.Client(), server.URL, "code", "redirect", "verifier", time.Now)
	assertTokenError(t, err)
}

func TestExchangeCode_MissingAccessTokenIsTypedError(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"refresh_token":"rt-1"}`))
	}))
	defer server.Close()

	_, err := exchangeCode(context.Background(), server.Client(), server.URL, "code", "redirect", "verifier", time.Now)
	assertTokenError(t, err)
}

func TestExchangeCode_NonSuccessStatusIsTypedErrorAndLeaksNoSecret(t *testing.T) {
	const secretCode = "super-secret-authorization-code"
	const secretVerifier = "super-secret-verifier-value"

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusBadRequest)
		_, _ = w.Write([]byte(`{"error":"invalid_grant"}`))
	}))
	defer server.Close()

	_, err := exchangeCode(context.Background(), server.Client(), server.URL, secretCode, "redirect", secretVerifier, time.Now)
	assertTokenError(t, err)

	if strings.Contains(err.Error(), secretCode) || strings.Contains(err.Error(), secretVerifier) {
		t.Fatalf("exchangeCode() error leaked a secret value: %v", err)
	}
}

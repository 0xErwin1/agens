package chatgpt

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"net/url"
	"testing"

	"github.com/0xErwin1/agens/internal/provider"
)

func TestModelsSendsExpectedRequestShapeAndFiltersByVisibility(t *testing.T) {
	var gotMethod, gotPath, gotQuery, gotAccept, gotOriginator, gotUserAgent, gotAuth, gotAccountID string

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotMethod = r.Method
		gotPath = r.URL.Path
		gotQuery = r.URL.RawQuery
		gotAccept = r.Header.Get("Accept")
		gotOriginator = r.Header.Get("originator")
		gotUserAgent = r.Header.Get("User-Agent")
		gotAuth = r.Header.Get("Authorization")
		gotAccountID = r.Header.Get("ChatGPT-Account-ID")

		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(`{"models":[
			{"slug":"gpt-5.5","display_name":"GPT-5.5","context_window":272000,"visibility":"list"},
			{"slug":"gpt-5.5-hidden","display_name":"GPT-5.5 Hidden","context_window":272000,"visibility":"hidden"}
		]}`))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p := newTestProvider(t, server.URL, auth)

	models, err := p.Models(context.Background())
	if err != nil {
		t.Fatalf("Models() error = %v, want nil", err)
	}

	if gotMethod != http.MethodGet {
		t.Fatalf("method = %q, want %q", gotMethod, http.MethodGet)
	}
	if gotPath != "/models" {
		t.Fatalf("path = %q, want %q", gotPath, "/models")
	}

	query, err := url.ParseQuery(gotQuery)
	if err != nil {
		t.Fatalf("ParseQuery(%q) error = %v", gotQuery, err)
	}
	if query.Get("client_version") != codexCLIVersion {
		t.Fatalf("client_version = %q, want %q", query.Get("client_version"), codexCLIVersion)
	}

	if gotAccept != "application/json" {
		t.Fatalf("Accept = %q, want %q", gotAccept, "application/json")
	}
	if gotOriginator != "codex_cli_rs" {
		t.Fatalf("originator = %q, want %q", gotOriginator, "codex_cli_rs")
	}
	if gotUserAgent == "" {
		t.Fatal("User-Agent header is empty, want a codex-style value")
	}
	if gotAuth != "Bearer stub-token" {
		t.Fatalf("Authorization = %q, want %q", gotAuth, "Bearer stub-token")
	}
	if gotAccountID != "acct_stub" {
		t.Fatalf("ChatGPT-Account-ID = %q, want %q", gotAccountID, "acct_stub")
	}
	if !auth.decorated {
		t.Fatal("Decorate() was not called")
	}

	if len(models) != 1 {
		t.Fatalf("Models() returned %d entries, want 1 (hidden entry filtered out): %+v", len(models), models)
	}
	want := provider.ModelInfo{ID: "gpt-5.5", DisplayName: "GPT-5.5", ContextWindow: 272_000, SupportsTools: true}
	if models[0] != want {
		t.Fatalf("Models()[0] = %+v, want %+v", models[0], want)
	}
}

func TestModelsNon2xxReturnsResponseError(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusUnauthorized)
		_, _ = w.Write([]byte(`{"detail":"invalid token"}`))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p := newTestProvider(t, server.URL, auth)

	models, err := p.Models(context.Background())
	if models != nil {
		t.Fatalf("Models() models = %+v, want nil", models)
	}
	if err == nil {
		t.Fatal("Models() error = nil, want non-nil")
	}

	var respErr *ResponseError
	if !errors.As(err, &respErr) {
		t.Fatalf("Models() error = %v, want *ResponseError via errors.As", err)
	}
	if respErr.StatusCode != http.StatusUnauthorized {
		t.Fatalf("ResponseError.StatusCode = %d, want %d", respErr.StatusCode, http.StatusUnauthorized)
	}
	if respErr.Message != "invalid token" {
		t.Fatalf("ResponseError.Message = %q, want %q", respErr.Message, "invalid token")
	}
}

func TestModelsRefreshesExpiredCredentialBeforeDecorate(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(`{"models":[]}`))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: false}
	p := newTestProvider(t, server.URL, auth)

	if _, err := p.Models(context.Background()); err != nil {
		t.Fatalf("Models() error = %v, want nil", err)
	}

	if !auth.refreshed {
		t.Fatal("Refresh() was not called even though Valid() returned false")
	}
	if !auth.decorated {
		t.Fatal("Decorate() was not called")
	}
}

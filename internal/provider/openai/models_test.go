package openai

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestModelsSendsExpectedRequestShapeAndFiltersToChatModels(t *testing.T) {
	var gotMethod, gotPath, gotAccept, gotAuth string

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotMethod = r.Method
		gotPath = r.URL.Path
		gotAccept = r.Header.Get("Accept")
		gotAuth = r.Header.Get("Authorization")

		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(`{"object":"list","data":[
			{"id":"gpt-4.1","object":"model","created":1700000000,"owned_by":"openai"},
			{"id":"o3","object":"model","created":1700000001,"owned_by":"openai"},
			{"id":"chatgpt-4o-latest","object":"model","created":1700000002,"owned_by":"openai"},
			{"id":"text-embedding-3-small","object":"model","created":1700000003,"owned_by":"openai"},
			{"id":"whisper-1","object":"model","created":1700000004,"owned_by":"openai"},
			{"id":"dall-e-3","object":"model","created":1700000005,"owned_by":"openai"}
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
	if gotAccept != "application/json" {
		t.Fatalf("Accept = %q, want %q", gotAccept, "application/json")
	}
	if gotAuth != "Bearer stub-token" {
		t.Fatalf("Authorization = %q, want %q", gotAuth, "Bearer stub-token")
	}
	if !auth.decorated {
		t.Fatal("Decorate() was not called")
	}

	wantIDs := map[string]bool{
		"gpt-4.1":           false,
		"o3":                false,
		"chatgpt-4o-latest": false,
	}
	if len(models) != len(wantIDs) {
		t.Fatalf("Models() returned %d entries, want %d (non-chat ids filtered out): %+v", len(models), len(wantIDs), models)
	}
	for _, m := range models {
		if _, known := wantIDs[m.ID]; !known {
			t.Fatalf("Models() returned unexpected non-chat id %q: %+v", m.ID, models)
		}
		wantIDs[m.ID] = true
		if m.DisplayName != m.ID {
			t.Fatalf("Models() entry %q DisplayName = %q, want %q", m.ID, m.DisplayName, m.ID)
		}
		if !m.SupportsTools {
			t.Fatalf("Models() entry %q SupportsTools = false, want true", m.ID)
		}
	}
	for id, found := range wantIDs {
		if !found {
			t.Fatalf("Models() missing expected chat id %q", id)
		}
	}
}

func TestModelsNon2xxReturnsResponseError(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusUnauthorized)
		_, _ = w.Write([]byte(`{"error":{"message":"bad key","type":"auth","code":"invalid_api_key"}}`))
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
	if respErr.Message != "bad key" || respErr.Type != "auth" || respErr.Code != "invalid_api_key" {
		t.Fatalf("ResponseError = %+v, want Message=%q Type=%q Code=%q", respErr, "bad key", "auth", "invalid_api_key")
	}
}

func TestModelsRefreshesExpiredCredentialBeforeDecorate(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(`{"object":"list","data":[]}`))
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

func TestModelsDoesNotRefreshValidCredential(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(`{"object":"list","data":[]}`))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p := newTestProvider(t, server.URL, auth)

	if _, err := p.Models(context.Background()); err != nil {
		t.Fatalf("Models() error = %v, want nil", err)
	}

	if auth.refreshed {
		t.Fatal("Refresh() was called even though Valid() returned true")
	}
	if !auth.decorated {
		t.Fatal("Decorate() was not called")
	}
}

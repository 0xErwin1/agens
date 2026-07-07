package chatgpt

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/provider"
)

var _ provider.Provider = (*Provider)(nil)

func newTestProvider(t *testing.T, baseURL string, auth provider.Authenticator) provider.Provider {
	t.Helper()

	p, err := New(provider.Config{BaseURL: baseURL, Model: "gpt-5-codex"}, auth)
	if err != nil {
		t.Fatalf("New() error = %v, want nil", err)
	}
	return p
}

func TestNewRejectsNilAuthenticator(t *testing.T) {
	_, err := New(provider.Config{}, nil)
	if err == nil {
		t.Fatal("New() error = nil, want non-nil for nil authenticator")
	}
}

func TestNewDefaultsBaseURL(t *testing.T) {
	p, err := New(provider.Config{}, &stubAuthenticator{validReturn: true})
	if err != nil {
		t.Fatalf("New() error = %v, want nil", err)
	}

	impl, ok := p.(*Provider)
	if !ok {
		t.Fatalf("New() returned %T, want *Provider", p)
	}
	if impl.baseURL != "https://chatgpt.com/backend-api/codex" {
		t.Fatalf("baseURL = %q, want %q", impl.baseURL, "https://chatgpt.com/backend-api/codex")
	}
}

func TestNewTrimsTrailingSlashFromBaseURL(t *testing.T) {
	p, err := New(provider.Config{BaseURL: "https://example.com/codex/"}, &stubAuthenticator{validReturn: true})
	if err != nil {
		t.Fatalf("New() error = %v, want nil", err)
	}

	impl, ok := p.(*Provider)
	if !ok {
		t.Fatalf("New() returned %T, want *Provider", p)
	}
	if impl.baseURL != "https://example.com/codex" {
		t.Fatalf("baseURL = %q, want %q", impl.baseURL, "https://example.com/codex")
	}
}

func TestNewAssignsSessionIDOncePerProvider(t *testing.T) {
	p, err := New(provider.Config{}, &stubAuthenticator{validReturn: true})
	if err != nil {
		t.Fatalf("New() error = %v, want nil", err)
	}

	impl, ok := p.(*Provider)
	if !ok {
		t.Fatalf("New() returned %T, want *Provider", p)
	}
	if impl.sessionID == "" {
		t.Fatal("sessionID = \"\", want a non-empty value assigned by New")
	}
}

func TestProviderID(t *testing.T) {
	p := newTestProvider(t, "", &stubAuthenticator{validReturn: true})

	if got := p.ID(); got != "openai-chatgpt" {
		t.Fatalf("ID() = %q, want %q", got, "openai-chatgpt")
	}
}

func TestProviderModelsFetchesFromModelsEndpoint(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/models" {
			t.Fatalf("path = %q, want %q", r.URL.Path, "/models")
		}
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(`{"models":[{"slug":"` + DefaultModel + `","display_name":"GPT-5.5","context_window":272000,"visibility":"list"}]}`))
	}))
	defer server.Close()

	p := newTestProvider(t, server.URL, &stubAuthenticator{validReturn: true})

	models, err := p.Models(context.Background())
	if err != nil {
		t.Fatalf("Models() error = %v, want nil", err)
	}
	if len(models) == 0 {
		t.Fatal("Models() returned empty slice, want non-empty")
	}

	found := false
	for _, m := range models {
		if m.ID == DefaultModel {
			found = true
		}
	}
	if !found {
		t.Fatalf("Models() = %+v, want an entry with ID %q", models, DefaultModel)
	}
}

// stubAuthenticator records whether Valid/Refresh/Decorate were invoked, and
// lets tests force Valid to report false to exercise the lazy Refresh path.
// Decorate sets both a Bearer token and a ChatGPT-Account-ID header, mirroring
// the two credential pieces a real ChatGPT-backed authenticator attaches.
type stubAuthenticator struct {
	validReturn bool
	refreshErr  error
	refreshed   bool
	decorated   bool
}

func (s *stubAuthenticator) Decorate(_ context.Context, req *http.Request) error {
	s.decorated = true
	req.Header.Set("Authorization", "Bearer stub-token")
	req.Header.Set("ChatGPT-Account-ID", "acct_stub")
	return nil
}

func (s *stubAuthenticator) Valid(_ time.Time) bool {
	return s.validReturn
}

func (s *stubAuthenticator) Refresh(_ context.Context) error {
	s.refreshed = true
	return s.refreshErr
}

func chatRequestFixture() provider.ChatRequest {
	return provider.ChatRequest{
		Messages: []message.Message{
			message.NewMessage(message.RoleUser, message.TextPart{Text: "hello"}),
		},
	}
}

func TestStreamSendsExpectedRequestShape(t *testing.T) {
	var gotMethod, gotPath, gotContentType, gotAccept, gotOriginator, gotUserAgent, gotSessionID, gotBeta, gotAuth, gotAccountID string

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotMethod = r.Method
		gotPath = r.URL.Path
		gotContentType = r.Header.Get("Content-Type")
		gotAccept = r.Header.Get("Accept")
		gotOriginator = r.Header.Get("originator")
		gotUserAgent = r.Header.Get("User-Agent")
		gotSessionID = r.Header.Get("session-id")
		gotBeta = r.Header.Get("OpenAI-Beta")
		gotAuth = r.Header.Get("Authorization")
		gotAccountID = r.Header.Get("ChatGPT-Account-ID")

		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(sseScript(`data: {"type":"response.completed","response":{}}`)))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p := newTestProvider(t, server.URL, auth)
	impl := p.(*Provider)

	reader, err := p.Stream(context.Background(), chatRequestFixture())
	if err != nil {
		t.Fatalf("Stream() error = %v, want nil", err)
	}
	defer func() { _ = reader.Close() }()

	if gotMethod != http.MethodPost {
		t.Fatalf("method = %q, want %q", gotMethod, http.MethodPost)
	}
	if gotPath != "/responses" {
		t.Fatalf("path = %q, want %q", gotPath, "/responses")
	}
	if gotContentType != "application/json" {
		t.Fatalf("Content-Type = %q, want %q", gotContentType, "application/json")
	}
	if gotAccept != "text/event-stream" {
		t.Fatalf("Accept = %q, want %q", gotAccept, "text/event-stream")
	}
	if gotOriginator != "codex_cli_rs" {
		t.Fatalf("originator = %q, want %q", gotOriginator, "codex_cli_rs")
	}
	if gotUserAgent == "" {
		t.Fatal("User-Agent header is empty, want a codex-style value")
	}
	if gotSessionID != impl.sessionID {
		t.Fatalf("session-id = %q, want the provider's sessionID %q", gotSessionID, impl.sessionID)
	}
	if gotBeta != "" {
		t.Fatalf("OpenAI-Beta header = %q, want it absent", gotBeta)
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
	if auth.refreshed {
		t.Fatal("Refresh() was called even though Valid() returned true")
	}
}

func TestStreamRefreshesExpiredCredentialBeforeDecorate(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(sseScript(`data: {"type":"response.completed","response":{}}`)))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: false}
	p := newTestProvider(t, server.URL, auth)

	reader, err := p.Stream(context.Background(), chatRequestFixture())
	if err != nil {
		t.Fatalf("Stream() error = %v, want nil", err)
	}
	defer func() { _ = reader.Close() }()

	if !auth.refreshed {
		t.Fatal("Refresh() was not called even though Valid() returned false")
	}
	if !auth.decorated {
		t.Fatal("Decorate() was not called")
	}
}

func TestStreamDrainsCannedSSEScriptThroughStreamReader(t *testing.T) {
	script := sseScript(
		`data: {"type":"response.output_text.delta","delta":"Hi"}`,
		`data: {"type":"response.completed","response":{}}`,
	)

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(script))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p := newTestProvider(t, server.URL, auth)

	reader, err := p.Stream(context.Background(), chatRequestFixture())
	if err != nil {
		t.Fatalf("Stream() error = %v, want nil", err)
	}
	defer func() { _ = reader.Close() }()

	var got []provider.StreamEvent
	for {
		ev, err := reader.Recv()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			t.Fatalf("Recv() unexpected error = %v", err)
		}
		got = append(got, ev)
	}

	if len(got) != 2 {
		t.Fatalf("Recv() delivered %d events, want 2 (TextDelta, Done): %+v", len(got), got)
	}
	if got[0].Type != provider.EventTextDelta || got[0].Text != "Hi" {
		t.Fatalf("Recv() first event = %+v, want EventTextDelta{Text:\"Hi\"}", got[0])
	}
	if got[1].Type != provider.EventDone || got[1].StopReason != "stop" {
		t.Fatalf("Recv() second event = %+v, want EventDone{StopReason:\"stop\"}", got[1])
	}
}

func TestStreamNon2xxReturnsResponseErrorAndNoStreamReader(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusUnauthorized)
		_, _ = w.Write([]byte(`{"error":{"message":"bad token","code":"invalid_token"}}`))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p := newTestProvider(t, server.URL, auth)

	reader, err := p.Stream(context.Background(), chatRequestFixture())
	if reader != nil {
		t.Fatalf("Stream() reader = %+v, want nil", reader)
	}
	if err == nil {
		t.Fatal("Stream() error = nil, want non-nil")
	}

	var respErr *ResponseError
	if !errors.As(err, &respErr) {
		t.Fatalf("Stream() error = %v, want *ResponseError via errors.As", err)
	}
	if respErr.StatusCode != http.StatusUnauthorized {
		t.Fatalf("ResponseError.StatusCode = %d, want %d", respErr.StatusCode, http.StatusUnauthorized)
	}
	if respErr.Message != "bad token" || respErr.Code != "invalid_token" {
		t.Fatalf("ResponseError = %+v, want Message=%q Code=%q", respErr, "bad token", "invalid_token")
	}
}

func TestStreamUsesRequestModelOverProviderDefault(t *testing.T) {
	var gotModel string

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]any
		b, _ := io.ReadAll(r.Body)
		_ = json.Unmarshal(b, &body)
		gotModel, _ = body["model"].(string)

		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(sseScript(`data: {"type":"response.completed","response":{}}`)))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p := newTestProvider(t, server.URL, auth) // provider default model gpt-5-codex

	req := chatRequestFixture()
	req.Model = "gpt-5-codex-mini"

	reader, err := p.Stream(context.Background(), req)
	if err != nil {
		t.Fatalf("Stream() error = %v, want nil", err)
	}
	defer func() { _ = reader.Close() }()

	if gotModel != "gpt-5-codex-mini" {
		t.Fatalf("model sent = %q, want %q", gotModel, "gpt-5-codex-mini")
	}
}

func TestStreamFallsBackToProviderConfigModel(t *testing.T) {
	var gotModel string

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]any
		b, _ := io.ReadAll(r.Body)
		_ = json.Unmarshal(b, &body)
		gotModel, _ = body["model"].(string)

		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(sseScript(`data: {"type":"response.completed","response":{}}`)))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p := newTestProvider(t, server.URL, auth) // provider config model gpt-5-codex

	reader, err := p.Stream(context.Background(), chatRequestFixture())
	if err != nil {
		t.Fatalf("Stream() error = %v, want nil", err)
	}
	defer func() { _ = reader.Close() }()

	if gotModel != "gpt-5-codex" {
		t.Fatalf("model sent = %q, want %q", gotModel, "gpt-5-codex")
	}
}

func TestStreamFallsBackToDefaultModelWhenNothingConfigured(t *testing.T) {
	var gotModel string

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]any
		b, _ := io.ReadAll(r.Body)
		_ = json.Unmarshal(b, &body)
		gotModel, _ = body["model"].(string)

		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(sseScript(`data: {"type":"response.completed","response":{}}`)))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p, err := New(provider.Config{BaseURL: server.URL}, auth) // no config model
	if err != nil {
		t.Fatalf("New() error = %v, want nil", err)
	}

	reader, err := p.Stream(context.Background(), chatRequestFixture())
	if err != nil {
		t.Fatalf("Stream() error = %v, want nil", err)
	}
	defer func() { _ = reader.Close() }()

	if gotModel != DefaultModel {
		t.Fatalf("model sent = %q, want %q", gotModel, DefaultModel)
	}
}

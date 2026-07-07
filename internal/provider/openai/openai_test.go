package openai

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

func TestDefaultModelConstant(t *testing.T) {
	if DefaultModel != "gpt-4.1" {
		t.Fatalf("DefaultModel = %q, want %q", DefaultModel, "gpt-4.1")
	}
}

func newTestProvider(t *testing.T, baseURL string, auth provider.Authenticator) provider.Provider {
	t.Helper()

	p, err := New(provider.Config{BaseURL: baseURL, Model: "gpt-4o"}, auth)
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
	auth, err := NewAPIKeyAuthenticator("sk-test")
	if err != nil {
		t.Fatalf("NewAPIKeyAuthenticator() error = %v", err)
	}

	p, err := New(provider.Config{}, auth)
	if err != nil {
		t.Fatalf("New() error = %v, want nil", err)
	}

	impl, ok := p.(*Provider)
	if !ok {
		t.Fatalf("New() returned %T, want *Provider", p)
	}
	if impl.baseURL != "https://api.openai.com/v1" {
		t.Fatalf("baseURL = %q, want %q", impl.baseURL, "https://api.openai.com/v1")
	}
}

func TestNewTrimsTrailingSlashFromBaseURL(t *testing.T) {
	auth, err := NewAPIKeyAuthenticator("sk-test")
	if err != nil {
		t.Fatalf("NewAPIKeyAuthenticator() error = %v", err)
	}

	p, err := New(provider.Config{BaseURL: "https://example.com/v1/"}, auth)
	if err != nil {
		t.Fatalf("New() error = %v, want nil", err)
	}

	impl, ok := p.(*Provider)
	if !ok {
		t.Fatalf("New() returned %T, want *Provider", p)
	}
	if impl.baseURL != "https://example.com/v1" {
		t.Fatalf("baseURL = %q, want %q", impl.baseURL, "https://example.com/v1")
	}
}

func TestProviderID(t *testing.T) {
	auth, err := NewAPIKeyAuthenticator("sk-test")
	if err != nil {
		t.Fatalf("NewAPIKeyAuthenticator() error = %v", err)
	}
	p := newTestProvider(t, "", auth)

	if got := p.ID(); got != "openai-api" {
		t.Fatalf("ID() = %q, want %q", got, "openai-api")
	}
}

// stubAuthenticator records whether Valid/Refresh/Decorate were invoked, and
// lets tests force Valid to report false to exercise the D8 Refresh path.
type stubAuthenticator struct {
	validReturn bool
	refreshErr  error
	refreshed   bool
	decorated   bool
}

func (s *stubAuthenticator) Decorate(_ context.Context, req *http.Request) error {
	s.decorated = true
	req.Header.Set("Authorization", "Bearer stub-token")
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
	var gotMethod, gotPath, gotContentType, gotAccept, gotAuth string
	var gotBody map[string]any

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotMethod = r.Method
		gotPath = r.URL.Path
		gotContentType = r.Header.Get("Content-Type")
		gotAccept = r.Header.Get("Accept")
		gotAuth = r.Header.Get("Authorization")

		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Fatalf("read request body: %v", err)
		}
		if err := json.Unmarshal(body, &gotBody); err != nil {
			t.Fatalf("unmarshal request body: %v", err)
		}

		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("data: [DONE]\n\n"))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p := newTestProvider(t, server.URL, auth)

	reader, err := p.Stream(context.Background(), chatRequestFixture())
	if err != nil {
		t.Fatalf("Stream() error = %v, want nil", err)
	}
	defer func() { _ = reader.Close() }()

	if gotMethod != http.MethodPost {
		t.Fatalf("method = %q, want %q", gotMethod, http.MethodPost)
	}
	if gotPath != "/chat/completions" {
		t.Fatalf("path = %q, want %q", gotPath, "/chat/completions")
	}
	if gotContentType != "application/json" {
		t.Fatalf("Content-Type = %q, want %q", gotContentType, "application/json")
	}
	if gotAccept != "text/event-stream" {
		t.Fatalf("Accept = %q, want %q", gotAccept, "text/event-stream")
	}
	if gotAuth != "Bearer stub-token" {
		t.Fatalf("Authorization = %q, want %q", gotAuth, "Bearer stub-token")
	}
	if !auth.decorated {
		t.Fatal("Decorate() was not called")
	}
	if auth.refreshed {
		t.Fatal("Refresh() was called even though Valid() returned true")
	}
	if stream, _ := gotBody["stream"].(bool); !stream {
		t.Fatalf("request body stream = %v, want true", gotBody["stream"])
	}
}

func TestStreamRefreshesExpiredCredentialBeforeDecorate(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("data: [DONE]\n\n"))
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
	script := "" +
		"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n" +
		"data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n" +
		"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n" +
		"data: [DONE]\n\n"

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
		_, _ = w.Write([]byte(`{"error":{"message":"bad key","type":"auth","code":"invalid_api_key"}}`))
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
	if respErr.Message != "bad key" || respErr.Type != "auth" || respErr.Code != "invalid_api_key" {
		t.Fatalf("ResponseError = %+v, want Message=%q Type=%q Code=%q", respErr, "bad key", "auth", "invalid_api_key")
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
		_, _ = w.Write([]byte("data: [DONE]\n\n"))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p := newTestProvider(t, server.URL, auth) // provider default model gpt-4o

	req := chatRequestFixture()
	req.Model = "gpt-4.1"

	reader, err := p.Stream(context.Background(), req)
	if err != nil {
		t.Fatalf("Stream() error = %v, want nil", err)
	}
	defer func() { _ = reader.Close() }()

	if gotModel != "gpt-4.1" {
		t.Fatalf("model sent = %q, want %q", gotModel, "gpt-4.1")
	}
}

func TestStreamFallsBackToProviderDefaultModel(t *testing.T) {
	var gotModel string

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]any
		b, _ := io.ReadAll(r.Body)
		_ = json.Unmarshal(b, &body)
		gotModel, _ = body["model"].(string)

		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("data: [DONE]\n\n"))
	}))
	defer server.Close()

	auth := &stubAuthenticator{validReturn: true}
	p := newTestProvider(t, server.URL, auth) // provider default model gpt-4o

	reader, err := p.Stream(context.Background(), chatRequestFixture())
	if err != nil {
		t.Fatalf("Stream() error = %v, want nil", err)
	}
	defer func() { _ = reader.Close() }()

	if gotModel != "gpt-4o" {
		t.Fatalf("model sent = %q, want %q", gotModel, "gpt-4o")
	}
}

func TestStreamErrorsBeforeNetworkWhenNoModelResolved(t *testing.T) {
	called := false
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		called = true
		w.WriteHeader(http.StatusOK)
	}))
	defer server.Close()

	auth, err := NewAPIKeyAuthenticator("sk-test")
	if err != nil {
		t.Fatalf("NewAPIKeyAuthenticator() error = %v", err)
	}
	p, err := New(provider.Config{BaseURL: server.URL}, auth) // no default model
	if err != nil {
		t.Fatalf("New() error = %v, want nil", err)
	}

	reader, err := p.Stream(context.Background(), chatRequestFixture()) // req.Model also empty
	if err == nil {
		t.Fatal("Stream() error = nil, want non-nil")
	}
	if reader != nil {
		t.Fatalf("Stream() reader = %+v, want nil", reader)
	}
	if called {
		t.Fatal("Stream() reached the network despite unresolved model")
	}
}

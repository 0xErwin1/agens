package provider

import (
	"context"
	"errors"
	"io"
	"net/http"
	"testing"
	"time"
)

// Compile-time assertions that the fakes satisfy the package's interfaces.
var (
	_ Authenticator = (*fakeAuthenticator)(nil)
	_ Provider      = (*fakeProvider)(nil)
	_ StreamReader  = (*fakeStreamReader)(nil)
)

// fakeAuthenticator is a minimal, in-package Authenticator for tests.
type fakeAuthenticator struct {
	decorateErr error
	valid       bool
	refreshErr  error
	refreshed   bool
}

func (f *fakeAuthenticator) Decorate(_ context.Context, req *http.Request) error {
	if f.decorateErr != nil {
		return f.decorateErr
	}
	req.Header.Set("Authorization", "Bearer fake-token")
	return nil
}

func (f *fakeAuthenticator) Valid(_ time.Time) bool {
	return f.valid
}

func (f *fakeAuthenticator) Refresh(_ context.Context) error {
	f.refreshed = true
	return f.refreshErr
}

// fakeStreamReader replays a scripted sequence of events, returning io.EOF
// once exhausted. If failErr is set, it is returned instead of io.EOF after
// the scripted events, without any additional StreamEvent.
type fakeStreamReader struct {
	events  []StreamEvent
	failErr error
	pos     int
	closed  bool
}

func (r *fakeStreamReader) Recv() (StreamEvent, error) {
	if r.pos < len(r.events) {
		ev := r.events[r.pos]
		r.pos++
		return ev, nil
	}
	if r.failErr != nil {
		return StreamEvent{}, r.failErr
	}
	return StreamEvent{}, io.EOF
}

func (r *fakeStreamReader) Close() error {
	r.closed = true
	return nil
}

// fakeProvider is a minimal, in-package Provider for tests.
type fakeProvider struct {
	id        string
	models    []ModelInfo
	reader    StreamReader
	streamErr error
}

func (f *fakeProvider) ID() string {
	return f.id
}

func (f *fakeProvider) Models(_ context.Context) ([]ModelInfo, error) {
	return f.models, nil
}

func (f *fakeProvider) Stream(_ context.Context, _ ChatRequest) (StreamReader, error) {
	if f.streamErr != nil {
		return nil, f.streamErr
	}
	return f.reader, nil
}

func TestFakeStreamReaderReturnsEOFAfterScript(t *testing.T) {
	reader := &fakeStreamReader{
		events: []StreamEvent{
			{Type: EventTextDelta, Text: "hi"},
			{Type: EventDone, StopReason: "end_turn"},
		},
	}

	var got []StreamEvent
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
		t.Fatalf("Recv() delivered %d events, want 2", len(got))
	}

	if _, err := reader.Recv(); !errors.Is(err, io.EOF) {
		t.Fatalf("Recv() after script exhausted error = %v, want io.EOF", err)
	}
}

func TestFakeStreamReaderMidStreamFailureReturnsNonEOFError(t *testing.T) {
	wantErr := errors.New("connection reset")
	reader := &fakeStreamReader{
		events:  []StreamEvent{{Type: EventTextDelta, Text: "partial"}},
		failErr: wantErr,
	}

	ev, err := reader.Recv()
	if err != nil {
		t.Fatalf("Recv() first call error = %v, want nil", err)
	}
	if ev.Text != "partial" {
		t.Fatalf("Recv() first call Text = %q, want %q", ev.Text, "partial")
	}

	_, err = reader.Recv()
	if err == nil {
		t.Fatal("Recv() second call error = nil, want non-nil")
	}
	if errors.Is(err, io.EOF) {
		t.Fatal("Recv() second call error = io.EOF, want a distinct error")
	}
	if !errors.Is(err, wantErr) {
		t.Fatalf("Recv() second call error = %v, want %v", err, wantErr)
	}
}

func TestFakeStreamReaderCloseIsIdempotent(t *testing.T) {
	reader := &fakeStreamReader{}

	if err := reader.Close(); err != nil {
		t.Fatalf("first Close() error = %v, want nil", err)
	}
	if err := reader.Close(); err != nil {
		t.Fatalf("second Close() error = %v, want nil", err)
	}
	if !reader.closed {
		t.Fatal("Close() did not mark reader as closed")
	}
}

func TestFakeAuthenticatorDecorateSetsHeader(t *testing.T) {
	auth := &fakeAuthenticator{valid: true}
	req, err := http.NewRequest(http.MethodGet, "https://example.com", nil)
	if err != nil {
		t.Fatalf("http.NewRequest() error = %v", err)
	}

	if err := auth.Decorate(context.Background(), req); err != nil {
		t.Fatalf("Decorate() error = %v, want nil", err)
	}
	if got := req.Header.Get("Authorization"); got != "Bearer fake-token" {
		t.Fatalf("Authorization header = %q, want %q", got, "Bearer fake-token")
	}
}

func TestFakeAuthenticatorValidReflectsState(t *testing.T) {
	valid := &fakeAuthenticator{valid: true}
	if !valid.Valid(time.Now()) {
		t.Fatal("Valid() = false, want true")
	}

	expired := &fakeAuthenticator{valid: false}
	if expired.Valid(time.Now()) {
		t.Fatal("Valid() = true, want false")
	}
}

// TestFakeAuthenticatorRefreshSignatureReturnsOnlyError pins the
// Authenticator.Refresh contract: it returns solely an error, never a token
// or credential value, because persistence is owned by the implementation.
func TestFakeAuthenticatorRefreshSignatureReturnsOnlyError(t *testing.T) {
	auth := &fakeAuthenticator{}

	err := auth.Refresh(context.Background())

	if err != nil {
		t.Fatalf("Refresh() error = %v, want nil", err)
	}
	if !auth.refreshed {
		t.Fatal("Refresh() did not mark authenticator as refreshed")
	}
}

func TestFakeAuthenticatorRefreshPropagatesError(t *testing.T) {
	wantErr := errors.New("network unreachable")
	auth := &fakeAuthenticator{refreshErr: wantErr}

	if err := auth.Refresh(context.Background()); !errors.Is(err, wantErr) {
		t.Fatalf("Refresh() error = %v, want %v", err, wantErr)
	}
}

func TestFakeProviderIDModelsStream(t *testing.T) {
	want := []StreamEvent{{Type: EventDone, StopReason: "end_turn"}}
	p := &fakeProvider{
		id:     "openai-api",
		models: []ModelInfo{{ID: "gpt-5", DisplayName: "GPT-5"}},
		reader: &fakeStreamReader{events: want},
	}

	if got := p.ID(); got != "openai-api" {
		t.Fatalf("ID() = %q, want %q", got, "openai-api")
	}

	models, err := p.Models(context.Background())
	if err != nil {
		t.Fatalf("Models() error = %v, want nil", err)
	}
	if len(models) != 1 || models[0].ID != "gpt-5" {
		t.Fatalf("Models() = %+v, want one ModelInfo with ID gpt-5", models)
	}

	reader, err := p.Stream(context.Background(), ChatRequest{Model: "gpt-5"})
	if err != nil {
		t.Fatalf("Stream() error = %v, want nil", err)
	}
	ev, err := reader.Recv()
	if err != nil {
		t.Fatalf("Recv() error = %v, want nil", err)
	}
	if ev.Type != EventDone || ev.StopReason != "end_turn" {
		t.Fatalf("Recv() = %+v, want %+v", ev, want[0])
	}
}

func TestFakeProviderStreamPropagatesError(t *testing.T) {
	wantErr := errors.New("rate limited")
	p := &fakeProvider{streamErr: wantErr}

	if _, err := p.Stream(context.Background(), ChatRequest{}); !errors.Is(err, wantErr) {
		t.Fatalf("Stream() error = %v, want %v", err, wantErr)
	}
}
